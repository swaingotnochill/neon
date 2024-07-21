use anyhow::{anyhow, bail};
use camino::Utf8PathBuf;
use pageserver_api::shard::TenantShardId;
use reqwest::Url;
use storage_scrubber::garbage::{find_garbage, purge_garbage, PurgeMode};
use storage_scrubber::pageserver_physical_gc::GcMode;
use storage_scrubber::scan_pageserver_metadata::scan_metadata;
use storage_scrubber::tenant_snapshot::SnapshotDownloader;
use storage_scrubber::{find_large_objects, ControllerClientConfig};
use storage_scrubber::{
    init_logging, pageserver_physical_gc::pageserver_physical_gc,
    scan_safekeeper_metadata::scan_safekeeper_metadata, BucketConfig, ConsoleConfig, NodeKind,
    TraversingDepth,
};

use clap::{Parser, Subcommand};
use utils::id::TenantId;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(arg_required_else_help(true))]
struct Cli {
    #[command(subcommand)]
    command: Command,

    #[arg(short, long, default_value_t = false)]
    delete: bool,

    #[arg(long)]
    /// URL to storage controller.  e.g. http://127.0.0.1:1234 when using `neon_local`
    controller_api: Option<Url>,

    #[arg(long)]
    /// JWT token for authenticating with storage controller.  Requires scope 'scrubber' or 'admin'.
    controller_jwt: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    FindGarbage {
        #[arg(short, long)]
        node_kind: NodeKind,
        #[arg(short, long, default_value_t=TraversingDepth::Tenant)]
        depth: TraversingDepth,
        #[arg(short, long, default_value_t = String::from("garbage.json"))]
        output_path: String,
    },
    PurgeGarbage {
        #[arg(short, long)]
        input_path: String,
        #[arg(short, long, default_value_t = PurgeMode::DeletedOnly)]
        mode: PurgeMode,
    },
    #[command(verbatim_doc_comment)]
    ScanMetadata {
        #[arg(short, long)]
        node_kind: NodeKind,
        #[arg(short, long, default_value_t = false)]
        json: bool,
        #[arg(long = "tenant-id", num_args = 0..)]
        tenant_ids: Vec<TenantShardId>,
        #[arg(long, default_value = None)]
        /// For safekeeper node_kind only, points to db with debug dump
        dump_db_connstr: Option<String>,
        /// For safekeeper node_kind only, table in the db with debug dump
        #[arg(long, default_value = None)]
        dump_db_table: Option<String>,
    },
    TenantSnapshot {
        #[arg(long = "tenant-id")]
        tenant_id: TenantId,
        #[arg(long = "concurrency", short = 'j', default_value_t = 8)]
        concurrency: usize,
        #[arg(short, long)]
        output_path: Utf8PathBuf,
    },
    PageserverPhysicalGc {
        #[arg(long = "tenant-id", num_args = 0..)]
        tenant_ids: Vec<TenantShardId>,
        #[arg(long = "min-age")]
        min_age: humantime::Duration,
        #[arg(short, long, default_value_t = GcMode::IndicesOnly)]
        mode: GcMode,
    },
    FindLargeObjects {
        #[arg(long = "min-size")]
        min_size: u64,
        #[arg(short, long, default_value_t = false)]
        ignore_deltas: bool,
        #[arg(long = "concurrency", short = 'j', default_value_t = 64)]
        concurrency: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let bucket_config = BucketConfig::from_env()?;

    let command_log_name = match &cli.command {
        Command::ScanMetadata { .. } => "scan",
        Command::FindGarbage { .. } => "find-garbage",
        Command::PurgeGarbage { .. } => "purge-garbage",
        Command::TenantSnapshot { .. } => "tenant-snapshot",
        Command::PageserverPhysicalGc { .. } => "pageserver-physical-gc",
        Command::FindLargeObjects { .. } => "find-large-objects",
    };
    let _guard = init_logging(&format!(
        "{}_{}_{}_{}.log",
        std::env::args().next().unwrap(),
        command_log_name,
        bucket_config.bucket,
        chrono::Utc::now().format("%Y_%m_%d__%H_%M_%S")
    ));

    match cli.command {
        Command::ScanMetadata {
            json,
            tenant_ids,
            node_kind,
            dump_db_connstr,
            dump_db_table,
        } => {
            if let NodeKind::Safekeeper = node_kind {
                let dump_db_connstr =
                    dump_db_connstr.ok_or(anyhow::anyhow!("dump_db_connstr not specified"))?;
                let dump_db_table =
                    dump_db_table.ok_or(anyhow::anyhow!("dump_db_table not specified"))?;

                let summary = scan_safekeeper_metadata(
                    bucket_config.clone(),
                    tenant_ids.iter().map(|tshid| tshid.tenant_id).collect(),
                    dump_db_connstr,
                    dump_db_table,
                )
                .await?;
                if json {
                    println!("{}", serde_json::to_string(&summary).unwrap())
                } else {
                    println!("{}", summary.summary_string());
                }
                if summary.is_fatal() {
                    bail!("Fatal scrub errors detected");
                }
                if summary.is_empty() {
                    // Strictly speaking an empty bucket is a valid bucket, but if someone ran the
                    // scrubber they were likely expecting to scan something, and if we see no timelines
                    // at all then it's likely due to some configuration issues like a bad prefix
                    bail!(
                        "No timelines found in bucket {} prefix {}",
                        bucket_config.bucket,
                        bucket_config
                            .prefix_in_bucket
                            .unwrap_or("<none>".to_string())
                    );
                }
                Ok(())
            } else {
                match scan_metadata(bucket_config.clone(), tenant_ids).await {
                    Err(e) => {
                        tracing::error!("Failed: {e}");
                        Err(e)
                    }
                    Ok(summary) => {
                        if json {
                            println!("{}", serde_json::to_string(&summary).unwrap())
                        } else {
                            println!("{}", summary.summary_string());
                        }
                        if summary.is_fatal() {
                            Err(anyhow::anyhow!("Fatal scrub errors detected"))
                        } else if summary.is_empty() {
                            // Strictly speaking an empty bucket is a valid bucket, but if someone ran the
                            // scrubber they were likely expecting to scan something, and if we see no timelines
                            // at all then it's likely due to some configuration issues like a bad prefix
                            Err(anyhow::anyhow!(
                                "No timelines found in bucket {} prefix {}",
                                bucket_config.bucket,
                                bucket_config
                                    .prefix_in_bucket
                                    .unwrap_or("<none>".to_string())
                            ))
                        } else {
                            Ok(())
                        }
                    }
                }
            }
        }
        Command::FindGarbage {
            node_kind,
            depth,
            output_path,
        } => {
            let console_config = ConsoleConfig::from_env()?;
            find_garbage(bucket_config, console_config, depth, node_kind, output_path).await
        }
        Command::PurgeGarbage { input_path, mode } => {
            purge_garbage(input_path, mode, !cli.delete).await
        }
        Command::TenantSnapshot {
            tenant_id,
            output_path,
            concurrency,
        } => {
            let downloader =
                SnapshotDownloader::new(bucket_config, tenant_id, output_path, concurrency).await?;
            downloader.download().await
        }
        Command::PageserverPhysicalGc {
            tenant_ids,
            min_age,
            mode,
        } => {
            let controller_client_conf = cli.controller_api.map(|controller_api| {
                ControllerClientConfig {
                    controller_api,
                    // Default to no key: this is a convenience when working in a development environment
                    controller_jwt: cli.controller_jwt.unwrap_or("".to_owned()),
                }
            });

            match (&controller_client_conf, mode) {
                (Some(_), _) => {
                    // Any mode may run when controller API is set
                }
                (None, GcMode::Full) => {
                    // The part of physical GC where we erase ancestor layers cannot be done safely without
                    // confirming the most recent complete shard split with the controller.  Refuse to run, rather
                    // than doing it unsafely.
                    return Err(anyhow!("Full physical GC requires `--controller-api` and `--controller-jwt` to run"));
                }
                (None, GcMode::DryRun | GcMode::IndicesOnly) => {
                    // These GcModes do not require the controller to run.
                }
            }

            let summary = pageserver_physical_gc(
                bucket_config,
                controller_client_conf,
                tenant_ids,
                min_age.into(),
                mode,
            )
            .await?;
            println!("{}", serde_json::to_string(&summary).unwrap());
            Ok(())
        }
        Command::FindLargeObjects {
            min_size,
            ignore_deltas,
            concurrency,
        } => {
            let summary = find_large_objects::find_large_objects(
                bucket_config,
                min_size,
                ignore_deltas,
                concurrency,
            )
            .await?;
            println!("{}", serde_json::to_string(&summary).unwrap());
            Ok(())
        }
    }
}
