use std::pin::pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::future::select;
use futures::future::try_join;
use futures::future::Either;
use futures::StreamExt;
use futures::TryFutureExt;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper1::body::Body;
use hyper1::body::Incoming;
use hyper1::header;
use hyper1::http::HeaderName;
use hyper1::http::HeaderValue;
use hyper1::Response;
use hyper1::StatusCode;
use hyper1::{HeaderMap, Request};
use pq_proto::StartupMessageParamsBuilder;
use serde::Serialize;
use serde_json::Value;
use tokio::time;
use tokio_postgres::error::DbError;
use tokio_postgres::error::ErrorPosition;
use tokio_postgres::error::SqlState;
use tokio_postgres::GenericClient;
use tokio_postgres::IsolationLevel;
use tokio_postgres::NoTls;
use tokio_postgres::ReadyForQueryStatus;
use tokio_postgres::Transaction;
use tokio_util::sync::CancellationToken;
use tracing::error;
use tracing::info;
use typed_json::json;
use url::Url;
use utils::http::error::ApiError;

use crate::auth::backend::ComputeUserInfo;
use crate::auth::endpoint_sni;
use crate::auth::ComputeUserInfoParseError;
use crate::config::ProxyConfig;
use crate::config::TlsConfig;
use crate::context::RequestMonitoring;
use crate::error::ErrorKind;
use crate::error::ReportableError;
use crate::error::UserFacingError;
use crate::metrics::HttpDirection;
use crate::metrics::Metrics;
use crate::proxy::run_until_cancelled;
use crate::proxy::NeonOptions;
use crate::serverless::backend::HttpConnError;
use crate::usage_metrics::MetricCounterRecorder;
use crate::DbName;
use crate::RoleName;

use super::backend::PoolingBackend;
use super::conn_pool::Client;
use super::conn_pool::ConnInfo;
use super::http_util::json_response;
use super::json::json_to_pg_text;
use super::json::pg_text_row_to_json;
use super::json::JsonConversionError;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryData {
    query: String,
    #[serde(deserialize_with = "bytes_to_pg_text")]
    params: Vec<Option<String>>,
    #[serde(default)]
    array_mode: Option<bool>,
}

#[derive(serde::Deserialize)]
struct BatchQueryData {
    queries: Vec<QueryData>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Payload {
    Single(QueryData),
    Batch(BatchQueryData),
}

const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024; // 10 MiB
const MAX_REQUEST_SIZE: u64 = 10 * 1024 * 1024; // 10 MiB

static RAW_TEXT_OUTPUT: HeaderName = HeaderName::from_static("neon-raw-text-output");
static ARRAY_MODE: HeaderName = HeaderName::from_static("neon-array-mode");
static ALLOW_POOL: HeaderName = HeaderName::from_static("neon-pool-opt-in");
static TXN_ISOLATION_LEVEL: HeaderName = HeaderName::from_static("neon-batch-isolation-level");
static TXN_READ_ONLY: HeaderName = HeaderName::from_static("neon-batch-read-only");
static TXN_DEFERRABLE: HeaderName = HeaderName::from_static("neon-batch-deferrable");

static HEADER_VALUE_TRUE: HeaderValue = HeaderValue::from_static("true");

fn bytes_to_pg_text<'de, D>(deserializer: D) -> Result<Vec<Option<String>>, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    // TODO: consider avoiding the allocation here.
    let json: Vec<Value> = serde::de::Deserialize::deserialize(deserializer)?;
    Ok(json_to_pg_text(json))
}

#[derive(Debug, thiserror::Error)]
pub enum ConnInfoError {
    #[error("invalid header: {0}")]
    InvalidHeader(&'static str),
    #[error("invalid connection string: {0}")]
    UrlParseError(#[from] url::ParseError),
    #[error("incorrect scheme")]
    IncorrectScheme,
    #[error("missing database name")]
    MissingDbName,
    #[error("invalid database name")]
    InvalidDbName,
    #[error("missing username")]
    MissingUsername,
    #[error("invalid username: {0}")]
    InvalidUsername(#[from] std::string::FromUtf8Error),
    #[error("missing password")]
    MissingPassword,
    #[error("missing hostname")]
    MissingHostname,
    #[error("invalid hostname: {0}")]
    InvalidEndpoint(#[from] ComputeUserInfoParseError),
    #[error("malformed endpoint")]
    MalformedEndpoint,
}

impl ReportableError for ConnInfoError {
    fn get_error_kind(&self) -> ErrorKind {
        ErrorKind::User
    }
}

impl UserFacingError for ConnInfoError {
    fn to_string_client(&self) -> String {
        self.to_string()
    }
}

fn get_conn_info(
    ctx: &mut RequestMonitoring,
    headers: &HeaderMap,
    tls: &TlsConfig,
) -> Result<ConnInfo, ConnInfoError> {
    // HTTP only uses cleartext (for now and likely always)
    ctx.set_auth_method(crate::context::AuthMethod::Cleartext);

    let connection_string = headers
        .get("Neon-Connection-String")
        .ok_or(ConnInfoError::InvalidHeader("Neon-Connection-String"))?
        .to_str()
        .map_err(|_| ConnInfoError::InvalidHeader("Neon-Connection-String"))?;

    let connection_url = Url::parse(connection_string)?;

    let protocol = connection_url.scheme();
    if protocol != "postgres" && protocol != "postgresql" {
        return Err(ConnInfoError::IncorrectScheme);
    }

    let mut url_path = connection_url
        .path_segments()
        .ok_or(ConnInfoError::MissingDbName)?;

    let dbname: DbName = url_path.next().ok_or(ConnInfoError::InvalidDbName)?.into();
    ctx.set_dbname(dbname.clone());

    let username = RoleName::from(urlencoding::decode(connection_url.username())?);
    if username.is_empty() {
        return Err(ConnInfoError::MissingUsername);
    }
    ctx.set_user(username.clone());

    let password = connection_url
        .password()
        .ok_or(ConnInfoError::MissingPassword)?;
    let password = urlencoding::decode_binary(password.as_bytes());

    let hostname = connection_url
        .host_str()
        .ok_or(ConnInfoError::MissingHostname)?;

    let endpoint =
        endpoint_sni(hostname, &tls.common_names)?.ok_or(ConnInfoError::MalformedEndpoint)?;
    ctx.set_endpoint_id(endpoint.clone());

    let pairs = connection_url.query_pairs();

    let mut options = Option::None;

    let mut params = StartupMessageParamsBuilder::default();
    params.insert("user", &username);
    params.insert("database", &dbname);
    for (key, value) in pairs {
        params.insert(&key, &value);
        if key == "options" {
            options = Some(NeonOptions::parse_options_raw(&value));
        }
    }
    ctx.set_db_options(params.freeze());

    let user_info = ComputeUserInfo {
        endpoint,
        user: username,
        options: options.unwrap_or_default(),
    };

    Ok(ConnInfo {
        user_info,
        dbname,
        password: match password {
            std::borrow::Cow::Borrowed(b) => b.into(),
            std::borrow::Cow::Owned(b) => b.into(),
        },
    })
}

// TODO: return different http error codes
pub async fn handle(
    config: &'static ProxyConfig,
    mut ctx: RequestMonitoring,
    request: Request<Incoming>,
    backend: Arc<PoolingBackend>,
    cancel: CancellationToken,
) -> Result<Response<Full<Bytes>>, ApiError> {
    let result = handle_inner(cancel, config, &mut ctx, request, backend).await;

    let mut response = match result {
        Ok(r) => {
            ctx.set_success();
            r
        }
        Err(e @ SqlOverHttpError::Cancelled(_)) => {
            let error_kind = e.get_error_kind();
            ctx.set_error_kind(error_kind);

            let message = "Query cancelled, connection was terminated";

            tracing::info!(
                kind=error_kind.to_metric_label(),
                error=%e,
                msg=message,
                "forwarding error to user"
            );

            json_response(
                StatusCode::BAD_REQUEST,
                json!({ "message": message, "code": SqlState::PROTOCOL_VIOLATION.code() }),
            )?
        }
        Err(e) => {
            let error_kind = e.get_error_kind();
            ctx.set_error_kind(error_kind);

            let mut message = e.to_string_client();
            let db_error = match &e {
                SqlOverHttpError::ConnectCompute(HttpConnError::ConnectionError(e))
                | SqlOverHttpError::Postgres(e) => e.as_db_error(),
                _ => None,
            };
            fn get<'a, T: Default>(db: Option<&'a DbError>, x: impl FnOnce(&'a DbError) -> T) -> T {
                db.map(x).unwrap_or_default()
            }

            if let Some(db_error) = db_error {
                db_error.message().clone_into(&mut message);
            }

            let position = db_error.and_then(|db| db.position());
            let (position, internal_position, internal_query) = match position {
                Some(ErrorPosition::Original(position)) => (Some(position.to_string()), None, None),
                Some(ErrorPosition::Internal { position, query }) => {
                    (None, Some(position.to_string()), Some(query.clone()))
                }
                None => (None, None, None),
            };

            let code = get(db_error, |db| db.code().code());
            let severity = get(db_error, |db| db.severity());
            let detail = get(db_error, |db| db.detail());
            let hint = get(db_error, |db| db.hint());
            let where_ = get(db_error, |db| db.where_());
            let table = get(db_error, |db| db.table());
            let column = get(db_error, |db| db.column());
            let schema = get(db_error, |db| db.schema());
            let datatype = get(db_error, |db| db.datatype());
            let constraint = get(db_error, |db| db.constraint());
            let file = get(db_error, |db| db.file());
            let line = get(db_error, |db| db.line().map(|l| l.to_string()));
            let routine = get(db_error, |db| db.routine());

            tracing::info!(
                kind=error_kind.to_metric_label(),
                error=%e,
                msg=message,
                "forwarding error to user"
            );

            // TODO: this shouldn't always be bad request.
            json_response(
                StatusCode::BAD_REQUEST,
                json!({
                    "message": message,
                    "code": code,
                    "detail": detail,
                    "hint": hint,
                    "position": position,
                    "internalPosition": internal_position,
                    "internalQuery": internal_query,
                    "severity": severity,
                    "where": where_,
                    "table": table,
                    "column": column,
                    "schema": schema,
                    "dataType": datatype,
                    "constraint": constraint,
                    "file": file,
                    "line": line,
                    "routine": routine,
                }),
            )?
        }
    };

    response
        .headers_mut()
        .insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    Ok(response)
}

#[derive(Debug, thiserror::Error)]
pub enum SqlOverHttpError {
    #[error("{0}")]
    ReadPayload(#[from] ReadPayloadError),
    #[error("{0}")]
    ConnectCompute(#[from] HttpConnError),
    #[error("{0}")]
    ConnInfo(#[from] ConnInfoError),
    #[error("request is too large (max is {MAX_REQUEST_SIZE} bytes)")]
    RequestTooLarge,
    #[error("response is too large (max is {MAX_RESPONSE_SIZE} bytes)")]
    ResponseTooLarge,
    #[error("invalid isolation level")]
    InvalidIsolationLevel,
    #[error("{0}")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("{0}")]
    JsonConversion(#[from] JsonConversionError),
    #[error("{0}")]
    Cancelled(SqlOverHttpCancel),
}

impl ReportableError for SqlOverHttpError {
    fn get_error_kind(&self) -> ErrorKind {
        match self {
            SqlOverHttpError::ReadPayload(e) => e.get_error_kind(),
            SqlOverHttpError::ConnectCompute(e) => e.get_error_kind(),
            SqlOverHttpError::ConnInfo(e) => e.get_error_kind(),
            SqlOverHttpError::RequestTooLarge => ErrorKind::User,
            SqlOverHttpError::ResponseTooLarge => ErrorKind::User,
            SqlOverHttpError::InvalidIsolationLevel => ErrorKind::User,
            SqlOverHttpError::Postgres(p) => p.get_error_kind(),
            SqlOverHttpError::JsonConversion(_) => ErrorKind::Postgres,
            SqlOverHttpError::Cancelled(c) => c.get_error_kind(),
        }
    }
}

impl UserFacingError for SqlOverHttpError {
    fn to_string_client(&self) -> String {
        match self {
            SqlOverHttpError::ReadPayload(p) => p.to_string(),
            SqlOverHttpError::ConnectCompute(c) => c.to_string_client(),
            SqlOverHttpError::ConnInfo(c) => c.to_string_client(),
            SqlOverHttpError::RequestTooLarge => self.to_string(),
            SqlOverHttpError::ResponseTooLarge => self.to_string(),
            SqlOverHttpError::InvalidIsolationLevel => self.to_string(),
            SqlOverHttpError::Postgres(p) => p.to_string(),
            SqlOverHttpError::JsonConversion(_) => "could not parse postgres response".to_string(),
            SqlOverHttpError::Cancelled(_) => self.to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReadPayloadError {
    #[error("could not read the HTTP request body: {0}")]
    Read(#[from] hyper1::Error),
    #[error("could not parse the HTTP request body: {0}")]
    Parse(#[from] serde_json::Error),
}

impl ReportableError for ReadPayloadError {
    fn get_error_kind(&self) -> ErrorKind {
        match self {
            ReadPayloadError::Read(_) => ErrorKind::ClientDisconnect,
            ReadPayloadError::Parse(_) => ErrorKind::User,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SqlOverHttpCancel {
    #[error("query was cancelled")]
    Postgres,
    #[error("query was cancelled while stuck trying to connect to the database")]
    Connect,
}

impl ReportableError for SqlOverHttpCancel {
    fn get_error_kind(&self) -> ErrorKind {
        match self {
            SqlOverHttpCancel::Postgres => ErrorKind::ClientDisconnect,
            SqlOverHttpCancel::Connect => ErrorKind::ClientDisconnect,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct HttpHeaders {
    raw_output: bool,
    default_array_mode: bool,
    txn_isolation_level: Option<IsolationLevel>,
    txn_read_only: bool,
    txn_deferrable: bool,
}

impl HttpHeaders {
    fn try_parse(headers: &hyper1::http::HeaderMap) -> Result<Self, SqlOverHttpError> {
        // Determine the output options. Default behaviour is 'false'. Anything that is not
        // strictly 'true' assumed to be false.
        let raw_output = headers.get(&RAW_TEXT_OUTPUT) == Some(&HEADER_VALUE_TRUE);
        let default_array_mode = headers.get(&ARRAY_MODE) == Some(&HEADER_VALUE_TRUE);

        // isolation level, read only and deferrable
        let txn_isolation_level = match headers.get(&TXN_ISOLATION_LEVEL) {
            Some(x) => Some(
                map_header_to_isolation_level(x).ok_or(SqlOverHttpError::InvalidIsolationLevel)?,
            ),
            None => None,
        };

        let txn_read_only = headers.get(&TXN_READ_ONLY) == Some(&HEADER_VALUE_TRUE);
        let txn_deferrable = headers.get(&TXN_DEFERRABLE) == Some(&HEADER_VALUE_TRUE);

        Ok(Self {
            raw_output,
            default_array_mode,
            txn_isolation_level,
            txn_read_only,
            txn_deferrable,
        })
    }
}

fn map_header_to_isolation_level(level: &HeaderValue) -> Option<IsolationLevel> {
    match level.as_bytes() {
        b"Serializable" => Some(IsolationLevel::Serializable),
        b"ReadUncommitted" => Some(IsolationLevel::ReadUncommitted),
        b"ReadCommitted" => Some(IsolationLevel::ReadCommitted),
        b"RepeatableRead" => Some(IsolationLevel::RepeatableRead),
        _ => None,
    }
}

fn map_isolation_level_to_headers(level: IsolationLevel) -> Option<HeaderValue> {
    match level {
        IsolationLevel::ReadUncommitted => Some(HeaderValue::from_static("ReadUncommitted")),
        IsolationLevel::ReadCommitted => Some(HeaderValue::from_static("ReadCommitted")),
        IsolationLevel::RepeatableRead => Some(HeaderValue::from_static("RepeatableRead")),
        IsolationLevel::Serializable => Some(HeaderValue::from_static("Serializable")),
        _ => None,
    }
}

async fn handle_inner(
    cancel: CancellationToken,
    config: &'static ProxyConfig,
    ctx: &mut RequestMonitoring,
    request: Request<Incoming>,
    backend: Arc<PoolingBackend>,
) -> Result<Response<Full<Bytes>>, SqlOverHttpError> {
    let _requeset_gauge = Metrics::get().proxy.connection_requests.guard(ctx.protocol);
    info!(
        protocol = %ctx.protocol,
        "handling interactive connection from client"
    );

    //
    // Determine the destination and connection params
    //
    let headers = request.headers();

    // TLS config should be there.
    let conn_info = get_conn_info(ctx, headers, config.tls_config.as_ref().unwrap())?;
    info!(user = conn_info.user_info.user.as_str(), "credentials");

    // Allow connection pooling only if explicitly requested
    // or if we have decided that http pool is no longer opt-in
    let allow_pool = !config.http_config.pool_options.opt_in
        || headers.get(&ALLOW_POOL) == Some(&HEADER_VALUE_TRUE);

    let parsed_headers = HttpHeaders::try_parse(headers)?;

    let request_content_length = match request.body().size_hint().upper() {
        Some(v) => v,
        None => MAX_REQUEST_SIZE + 1,
    };
    info!(request_content_length, "request size in bytes");
    Metrics::get()
        .proxy
        .http_conn_content_length_bytes
        .observe(HttpDirection::Request, request_content_length as f64);

    // we don't have a streaming request support yet so this is to prevent OOM
    // from a malicious user sending an extremely large request body
    if request_content_length > MAX_REQUEST_SIZE {
        return Err(SqlOverHttpError::RequestTooLarge);
    }

    let fetch_and_process_request = Box::pin(
        async {
            let body = request.into_body().collect().await?.to_bytes();
            info!(length = body.len(), "request payload read");
            let payload: Payload = serde_json::from_slice(&body)?;
            Ok::<Payload, ReadPayloadError>(payload) // Adjust error type accordingly
        }
        .map_err(SqlOverHttpError::from),
    );

    let authenticate_and_connect = Box::pin(
        async {
            let keys = backend
                .authenticate(ctx, &config.authentication_config, &conn_info)
                .await?;
            let client = backend
                .connect_to_compute(ctx, conn_info, keys, !allow_pool)
                .await?;
            // not strictly necessary to mark success here,
            // but it's just insurance for if we forget it somewhere else
            ctx.latency_timer.success();
            Ok::<_, HttpConnError>(client)
        }
        .map_err(SqlOverHttpError::from),
    );

    let (payload, mut client) = match run_until_cancelled(
        // Run both operations in parallel
        try_join(
            pin!(fetch_and_process_request),
            pin!(authenticate_and_connect),
        ),
        &cancel,
    )
    .await
    {
        Some(result) => result?,
        None => return Err(SqlOverHttpError::Cancelled(SqlOverHttpCancel::Connect)),
    };

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json");

    // Now execute the query and return the result.
    let json_output = match payload {
        Payload::Single(stmt) => stmt.process(cancel, &mut client, parsed_headers).await?,
        Payload::Batch(statements) => {
            if parsed_headers.txn_read_only {
                response = response.header(TXN_READ_ONLY.clone(), &HEADER_VALUE_TRUE);
            }
            if parsed_headers.txn_deferrable {
                response = response.header(TXN_DEFERRABLE.clone(), &HEADER_VALUE_TRUE);
            }
            if let Some(txn_isolation_level) = parsed_headers
                .txn_isolation_level
                .and_then(map_isolation_level_to_headers)
            {
                response = response.header(TXN_ISOLATION_LEVEL.clone(), txn_isolation_level);
            }

            statements
                .process(cancel, &mut client, parsed_headers)
                .await?
        }
    };

    let metrics = client.metrics();

    let len = json_output.len();
    let response = response
        .body(Full::new(Bytes::from(json_output)))
        // only fails if invalid status code or invalid header/values are given.
        // these are not user configurable so it cannot fail dynamically
        .expect("building response payload should not fail");

    // count the egress bytes - we miss the TLS and header overhead but oh well...
    // moving this later in the stack is going to be a lot of effort and ehhhh
    metrics.record_egress(len as u64);
    Metrics::get()
        .proxy
        .http_conn_content_length_bytes
        .observe(HttpDirection::Response, len as f64);

    Ok(response)
}

impl QueryData {
    async fn process(
        self,
        cancel: CancellationToken,
        client: &mut Client<tokio_postgres::Client>,
        parsed_headers: HttpHeaders,
    ) -> Result<String, SqlOverHttpError> {
        let (inner, mut discard) = client.inner();
        let cancel_token = inner.cancel_token();

        let res = match select(
            pin!(query_to_json(&*inner, self, &mut 0, parsed_headers)),
            pin!(cancel.cancelled()),
        )
        .await
        {
            // The query successfully completed.
            Either::Left((Ok((status, results)), __not_yet_cancelled)) => {
                discard.check_idle(status);

                let json_output =
                    serde_json::to_string(&results).expect("json serialization should not fail");
                Ok(json_output)
            }
            // The query failed with an error
            Either::Left((Err(e), __not_yet_cancelled)) => {
                discard.discard();
                return Err(e);
            }
            // The query was cancelled.
            Either::Right((_cancelled, query)) => {
                tracing::info!("cancelling query");
                if let Err(err) = cancel_token.cancel_query(NoTls).await {
                    tracing::error!(?err, "could not cancel query");
                }
                // wait for the query cancellation
                match time::timeout(time::Duration::from_millis(100), query).await {
                    // query successed before it was cancelled.
                    Ok(Ok((status, results))) => {
                        discard.check_idle(status);

                        let json_output = serde_json::to_string(&results)
                            .expect("json serialization should not fail");
                        Ok(json_output)
                    }
                    // query failed or was cancelled.
                    Ok(Err(error)) => {
                        let db_error = match &error {
                            SqlOverHttpError::ConnectCompute(HttpConnError::ConnectionError(e))
                            | SqlOverHttpError::Postgres(e) => e.as_db_error(),
                            _ => None,
                        };

                        // if errored for some other reason, it might not be safe to return
                        if !db_error.is_some_and(|e| *e.code() == SqlState::QUERY_CANCELED) {
                            discard.discard();
                        }

                        Err(SqlOverHttpError::Cancelled(SqlOverHttpCancel::Postgres))
                    }
                    Err(_timeout) => {
                        discard.discard();
                        Err(SqlOverHttpError::Cancelled(SqlOverHttpCancel::Postgres))
                    }
                }
            }
        };
        res
    }
}

impl BatchQueryData {
    async fn process(
        self,
        cancel: CancellationToken,
        client: &mut Client<tokio_postgres::Client>,
        parsed_headers: HttpHeaders,
    ) -> Result<String, SqlOverHttpError> {
        info!("starting transaction");
        let (inner, mut discard) = client.inner();
        let cancel_token = inner.cancel_token();
        let mut builder = inner.build_transaction();
        if let Some(isolation_level) = parsed_headers.txn_isolation_level {
            builder = builder.isolation_level(isolation_level);
        }
        if parsed_headers.txn_read_only {
            builder = builder.read_only(true);
        }
        if parsed_headers.txn_deferrable {
            builder = builder.deferrable(true);
        }

        let transaction = builder.start().await.map_err(|e| {
            // if we cannot start a transaction, we should return immediately
            // and not return to the pool. connection is clearly broken
            discard.discard();
            e
        })?;

        let json_output =
            match query_batch(cancel.child_token(), &transaction, self, parsed_headers).await {
                Ok(json_output) => {
                    info!("commit");
                    let status = transaction.commit().await.map_err(|e| {
                        // if we cannot commit - for now don't return connection to pool
                        // TODO: get a query status from the error
                        discard.discard();
                        e
                    })?;
                    discard.check_idle(status);
                    json_output
                }
                Err(SqlOverHttpError::Cancelled(_)) => {
                    if let Err(err) = cancel_token.cancel_query(NoTls).await {
                        tracing::error!(?err, "could not cancel query");
                    }
                    // TODO: after cancelling, wait to see if we can get a status. maybe the connection is still safe.
                    discard.discard();

                    return Err(SqlOverHttpError::Cancelled(SqlOverHttpCancel::Postgres));
                }
                Err(err) => {
                    info!("rollback");
                    let status = transaction.rollback().await.map_err(|e| {
                        // if we cannot rollback - for now don't return connection to pool
                        // TODO: get a query status from the error
                        discard.discard();
                        e
                    })?;
                    discard.check_idle(status);
                    return Err(err);
                }
            };

        Ok(json_output)
    }
}

async fn query_batch(
    cancel: CancellationToken,
    transaction: &Transaction<'_>,
    queries: BatchQueryData,
    parsed_headers: HttpHeaders,
) -> Result<String, SqlOverHttpError> {
    let mut results = Vec::with_capacity(queries.queries.len());
    let mut current_size = 0;
    for stmt in queries.queries {
        let query = pin!(query_to_json(
            transaction,
            stmt,
            &mut current_size,
            parsed_headers,
        ));
        let cancelled = pin!(cancel.cancelled());
        let res = select(query, cancelled).await;
        match res {
            // TODO: maybe we should check that the transaction bit is set here
            Either::Left((Ok((_, values)), _cancelled)) => {
                results.push(values);
            }
            Either::Left((Err(e), _cancelled)) => {
                return Err(e);
            }
            Either::Right((_cancelled, _)) => {
                return Err(SqlOverHttpError::Cancelled(SqlOverHttpCancel::Postgres));
            }
        }
    }

    let results = json!({ "results": results });
    let json_output = serde_json::to_string(&results).expect("json serialization should not fail");

    Ok(json_output)
}

async fn query_to_json<T: GenericClient>(
    client: &T,
    data: QueryData,
    current_size: &mut usize,
    parsed_headers: HttpHeaders,
) -> Result<(ReadyForQueryStatus, impl Serialize), SqlOverHttpError> {
    info!("executing query");
    let query_params = data.params;
    let mut row_stream = std::pin::pin!(client.query_raw_txt(&data.query, query_params).await?);
    info!("finished executing query");

    // Manually drain the stream into a vector to leave row_stream hanging
    // around to get a command tag. Also check that the response is not too
    // big.
    let mut rows: Vec<tokio_postgres::Row> = Vec::new();
    while let Some(row) = row_stream.next().await {
        let row = row?;
        *current_size += row.body_len();
        rows.push(row);
        // we don't have a streaming response support yet so this is to prevent OOM
        // from a malicious query (eg a cross join)
        if *current_size > MAX_RESPONSE_SIZE {
            return Err(SqlOverHttpError::ResponseTooLarge);
        }
    }

    let ready = row_stream.ready_status();

    // grab the command tag and number of rows affected
    let command_tag = row_stream.command_tag().unwrap_or_default();
    let mut command_tag_split = command_tag.split(' ');
    let command_tag_name = command_tag_split.next().unwrap_or_default();
    let command_tag_count = if command_tag_name == "INSERT" {
        // INSERT returns OID first and then number of rows
        command_tag_split.nth(1)
    } else {
        // other commands return number of rows (if any)
        command_tag_split.next()
    }
    .and_then(|s| s.parse::<i64>().ok());

    info!(
        rows = rows.len(),
        ?ready,
        command_tag,
        "finished reading rows"
    );

    let columns_len = row_stream.columns().len();
    let mut fields = Vec::with_capacity(columns_len);
    let mut columns = Vec::with_capacity(columns_len);

    for c in row_stream.columns() {
        fields.push(json!({
            "name": c.name().to_owned(),
            "dataTypeID": c.type_().oid(),
            "tableID": c.table_oid(),
            "columnID": c.column_id(),
            "dataTypeSize": c.type_size(),
            "dataTypeModifier": c.type_modifier(),
            "format": "text",
        }));
        columns.push(client.get_type(c.type_oid()).await?);
    }

    let array_mode = data.array_mode.unwrap_or(parsed_headers.default_array_mode);

    // convert rows to JSON
    let rows = rows
        .iter()
        .map(|row| pg_text_row_to_json(row, &columns, parsed_headers.raw_output, array_mode))
        .collect::<Result<Vec<_>, _>>()?;

    // Resulting JSON format is based on the format of node-postgres result.
    let results = json!({
        "command": command_tag_name.to_string(),
        "rowCount": command_tag_count,
        "rows": rows,
        "fields": fields,
        "rowAsArray": array_mode,
    });

    Ok((ready, results))
}
