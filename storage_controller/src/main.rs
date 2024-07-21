use anyhow::{anyhow, Context};
use clap::Parser;
use diesel::Connection;
use metrics::launch_timestamp::LaunchTimestamp;
use metrics::BuildInfo;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use storage_controller::http::make_router;
use storage_controller::metrics::preinitialize_metrics;
use storage_controller::persistence::Persistence;
use storage_controller::service::{
    Config, Service, MAX_UNAVAILABLE_INTERVAL_DEFAULT, RECONCILER_CONCURRENCY_DEFAULT,
};
use tokio::signal::unix::SignalKind;
use tokio_util::sync::CancellationToken;
use utils::auth::{JwtAuth, SwappableJwtAuth};
use utils::logging::{self, LogFormat};

use utils::sentry_init::init_sentry;
use utils::{project_build_tag, project_git_version, tcp_listener};

project_git_version!(GIT_VERSION);
project_build_tag!(BUILD_TAG);

use diesel_migrations::{embed_migrations, EmbeddedMigrations};
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("./migrations");

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(arg_required_else_help(true))]
struct Cli {
    /// Host and port to listen on, like `127.0.0.1:1234`
    #[arg(short, long)]
    listen: std::net::SocketAddr,

    /// Public key for JWT authentication of clients
    #[arg(long)]
    public_key: Option<String>,

    /// Token for authenticating this service with the pageservers it controls
    #[arg(long)]
    jwt_token: Option<String>,

    /// Token for authenticating this service with the control plane, when calling
    /// the compute notification endpoint
    #[arg(long)]
    control_plane_jwt_token: Option<String>,

    /// URL to control plane compute notification endpoint
    #[arg(long)]
    compute_hook_url: Option<String>,

    /// URL to connect to postgres, like postgresql://localhost:1234/storage_controller
    #[arg(long)]
    database_url: Option<String>,

    /// Flag to enable dev mode, which permits running without auth
    #[arg(long, default_value = "false")]
    dev: bool,

    /// Grace period before marking unresponsive pageserver offline
    #[arg(long)]
    max_unavailable_interval: Option<humantime::Duration>,

    /// Size threshold for automatically splitting shards (disabled by default)
    #[arg(long)]
    split_threshold: Option<u64>,

    /// Maximum number of reconcilers that may run in parallel
    #[arg(long)]
    reconciler_concurrency: Option<usize>,

    /// How long to wait for the initial database connection to be available.
    #[arg(long, default_value = "5s")]
    db_connect_timeout: humantime::Duration,

    /// `neon_local` sets this to the path of the neon_local repo dir.
    /// Only relevant for testing.
    // TODO: make `cfg(feature = "testing")`
    #[arg(long)]
    neon_local_repo_dir: Option<PathBuf>,
}

enum StrictMode {
    /// In strict mode, we will require that all secrets are loaded, i.e. security features
    /// may not be implicitly turned off by omitting secrets in the environment.
    Strict,
    /// In dev mode, secrets are optional, and omitting a particular secret will implicitly
    /// disable the auth related to it (e.g. no pageserver jwt key -> send unauthenticated
    /// requests, no public key -> don't authenticate incoming requests).
    Dev,
}

impl Default for StrictMode {
    fn default() -> Self {
        Self::Strict
    }
}

/// Secrets may either be provided on the command line (for testing), or loaded from AWS SecretManager: this
/// type encapsulates the logic to decide which and do the loading.
struct Secrets {
    database_url: String,
    public_key: Option<JwtAuth>,
    jwt_token: Option<String>,
    control_plane_jwt_token: Option<String>,
}

impl Secrets {
    const DATABASE_URL_ENV: &'static str = "DATABASE_URL";
    const PAGESERVER_JWT_TOKEN_ENV: &'static str = "PAGESERVER_JWT_TOKEN";
    const CONTROL_PLANE_JWT_TOKEN_ENV: &'static str = "CONTROL_PLANE_JWT_TOKEN";
    const PUBLIC_KEY_ENV: &'static str = "PUBLIC_KEY";

    /// Load secrets from, in order of preference:
    /// - CLI args if database URL is provided on the CLI
    /// - Environment variables if DATABASE_URL is set.
    /// - AWS Secrets Manager secrets
    async fn load(args: &Cli) -> anyhow::Result<Self> {
        let Some(database_url) =
            Self::load_secret(&args.database_url, Self::DATABASE_URL_ENV).await
        else {
            anyhow::bail!(
                "Database URL is not set (set `--database-url`, or `DATABASE_URL` environment)"
            )
        };

        let public_key = match Self::load_secret(&args.public_key, Self::PUBLIC_KEY_ENV).await {
            Some(v) => Some(JwtAuth::from_key(v).context("Loading public key")?),
            None => None,
        };

        let this = Self {
            database_url,
            public_key,
            jwt_token: Self::load_secret(&args.jwt_token, Self::PAGESERVER_JWT_TOKEN_ENV).await,
            control_plane_jwt_token: Self::load_secret(
                &args.control_plane_jwt_token,
                Self::CONTROL_PLANE_JWT_TOKEN_ENV,
            )
            .await,
        };

        Ok(this)
    }

    async fn load_secret(cli: &Option<String>, env_name: &str) -> Option<String> {
        if let Some(v) = cli {
            Some(v.clone())
        } else if let Ok(v) = std::env::var(env_name) {
            Some(v)
        } else {
            None
        }
    }
}

/// Execute the diesel migrations that are built into this binary
async fn migration_run(database_url: &str) -> anyhow::Result<()> {
    use diesel::PgConnection;
    use diesel_migrations::{HarnessWithOutput, MigrationHarness};
    let mut conn = PgConnection::establish(database_url)?;

    HarnessWithOutput::write_to_stdout(&mut conn)
        .run_pending_migrations(MIGRATIONS)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!(e))?;

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        std::process::exit(1);
    }));

    let _sentry_guard = init_sentry(Some(GIT_VERSION.into()), &[]);

    tokio::runtime::Builder::new_current_thread()
        // We use spawn_blocking for database operations, so require approximately
        // as many blocking threads as we will open database connections.
        .max_blocking_threads(Persistence::MAX_CONNECTIONS as usize)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let launch_ts = Box::leak(Box::new(LaunchTimestamp::generate()));

    logging::init(
        LogFormat::Plain,
        logging::TracingErrorLayerEnablement::Disabled,
        logging::Output::Stdout,
    )?;

    preinitialize_metrics();

    let args = Cli::parse();
    tracing::info!(
        "version: {}, launch_timestamp: {}, build_tag {}, listening on {}",
        GIT_VERSION,
        launch_ts.to_string(),
        BUILD_TAG,
        args.listen
    );

    let build_info = BuildInfo {
        revision: GIT_VERSION,
        build_tag: BUILD_TAG,
    };

    let strict_mode = if args.dev {
        StrictMode::Dev
    } else {
        StrictMode::Strict
    };

    let secrets = Secrets::load(&args).await?;

    // Validate required secrets and arguments are provided in strict mode
    match strict_mode {
        StrictMode::Strict
            if (secrets.public_key.is_none()
                || secrets.jwt_token.is_none()
                || secrets.control_plane_jwt_token.is_none()) =>
        {
            // Production systems should always have secrets configured: if public_key was not set
            // then we would implicitly disable auth.
            anyhow::bail!(
                    "Insecure config!  One or more secrets is not set.  This is only permitted in `--dev` mode"
                );
        }
        StrictMode::Strict if args.compute_hook_url.is_none() => {
            // Production systems should always have a compute hook set, to prevent falling
            // back to trying to use neon_local.
            anyhow::bail!(
                "`--compute-hook-url` is not set: this is only permitted in `--dev` mode"
            );
        }
        StrictMode::Strict => {
            tracing::info!("Starting in strict mode: configuration is OK.")
        }
        StrictMode::Dev => {
            tracing::warn!("Starting in dev mode: this may be an insecure configuration.")
        }
    }

    let config = Config {
        jwt_token: secrets.jwt_token,
        control_plane_jwt_token: secrets.control_plane_jwt_token,
        compute_hook_url: args.compute_hook_url,
        max_unavailable_interval: args
            .max_unavailable_interval
            .map(humantime::Duration::into)
            .unwrap_or(MAX_UNAVAILABLE_INTERVAL_DEFAULT),
        reconciler_concurrency: args
            .reconciler_concurrency
            .unwrap_or(RECONCILER_CONCURRENCY_DEFAULT),
        split_threshold: args.split_threshold,
        neon_local_repo_dir: args.neon_local_repo_dir,
    };

    // After loading secrets & config, but before starting anything else, apply database migrations
    Persistence::await_connection(&secrets.database_url, args.db_connect_timeout.into()).await?;

    migration_run(&secrets.database_url)
        .await
        .context("Running database migrations")?;

    let persistence = Arc::new(Persistence::new(secrets.database_url));

    let service = Service::spawn(config, persistence.clone()).await?;

    let http_listener = tcp_listener::bind(args.listen)?;

    let auth = secrets
        .public_key
        .map(|jwt_auth| Arc::new(SwappableJwtAuth::new(jwt_auth)));
    let router = make_router(service.clone(), auth, build_info)
        .build()
        .map_err(|err| anyhow!(err))?;
    let router_service = utils::http::RouterService::new(router).unwrap();

    // Start HTTP server
    let server_shutdown = CancellationToken::new();
    let server = hyper::Server::from_tcp(http_listener)?
        .serve(router_service)
        .with_graceful_shutdown({
            let server_shutdown = server_shutdown.clone();
            async move {
                server_shutdown.cancelled().await;
            }
        });
    tracing::info!("Serving on {0}", args.listen);
    let server_task = tokio::task::spawn(server);

    // Wait until we receive a signal
    let mut sigint = tokio::signal::unix::signal(SignalKind::interrupt())?;
    let mut sigquit = tokio::signal::unix::signal(SignalKind::quit())?;
    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;
    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
        _ = sigquit.recv() => {},
    }
    tracing::info!("Terminating on signal");

    // Stop HTTP server first, so that we don't have to service requests
    // while shutting down Service.
    server_shutdown.cancel();
    match tokio::time::timeout(Duration::from_secs(5), server_task).await {
        Ok(Ok(_)) => {
            tracing::info!("Joined HTTP server task");
        }
        Ok(Err(e)) => {
            tracing::error!("Error joining HTTP server task: {e}")
        }
        Err(_) => {
            tracing::warn!("Timed out joining HTTP server task");
            // We will fall through and shut down the service anyway, any request handlers
            // in flight will experience cancellation & their clients will see a torn connection.
        }
    }

    service.shutdown().await;
    tracing::info!("Service shutdown complete");

    std::process::exit(0);
}
