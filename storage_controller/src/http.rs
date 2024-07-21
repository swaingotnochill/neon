use crate::metrics::{
    HttpRequestLatencyLabelGroup, HttpRequestStatusLabelGroup, PageserverRequestLabelGroup,
    METRICS_REGISTRY,
};
use crate::reconciler::ReconcileError;
use crate::service::{Service, STARTUP_RECONCILE_TIMEOUT};
use anyhow::Context;
use futures::Future;
use hyper::header::CONTENT_TYPE;
use hyper::{Body, Request, Response};
use hyper::{StatusCode, Uri};
use metrics::{BuildInfo, NeonMetrics};
use pageserver_api::controller_api::TenantCreateRequest;
use pageserver_api::models::{
    TenantConfigRequest, TenantLocationConfigRequest, TenantShardSplitRequest,
    TenantTimeTravelRequest, TimelineCreateRequest,
};
use pageserver_api::shard::TenantShardId;
use pageserver_client::mgmt_api;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use utils::auth::{Scope, SwappableJwtAuth};
use utils::failpoint_support::failpoints_handler;
use utils::http::endpoint::{auth_middleware, check_permission_with, request_span};
use utils::http::request::{must_get_query_param, parse_query_param, parse_request_param};
use utils::id::{TenantId, TimelineId};

use utils::{
    http::{
        endpoint::{self},
        error::ApiError,
        json::{json_request, json_response},
        RequestExt, RouterBuilder,
    },
    id::NodeId,
};

use pageserver_api::controller_api::{
    NodeAvailability, NodeConfigureRequest, NodeRegisterRequest, TenantPolicyRequest,
    TenantShardMigrateRequest,
};
use pageserver_api::upcall_api::{ReAttachRequest, ValidateRequest};

use control_plane::storage_controller::{AttachHookRequest, InspectRequest};

use routerify::Middleware;

/// State available to HTTP request handlers
pub struct HttpState {
    service: Arc<crate::service::Service>,
    auth: Option<Arc<SwappableJwtAuth>>,
    neon_metrics: NeonMetrics,
    allowlist_routes: Vec<Uri>,
}

impl HttpState {
    pub fn new(
        service: Arc<crate::service::Service>,
        auth: Option<Arc<SwappableJwtAuth>>,
        build_info: BuildInfo,
    ) -> Self {
        let allowlist_routes = ["/status", "/ready", "/metrics"]
            .iter()
            .map(|v| v.parse().unwrap())
            .collect::<Vec<_>>();
        Self {
            service,
            auth,
            neon_metrics: NeonMetrics::new(build_info),
            allowlist_routes,
        }
    }
}

#[inline(always)]
fn get_state(request: &Request<Body>) -> &HttpState {
    request
        .data::<Arc<HttpState>>()
        .expect("unknown state type")
        .as_ref()
}

/// Pageserver calls into this on startup, to learn which tenants it should attach
async fn handle_re_attach(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::GenerationsApi)?;

    let reattach_req = json_request::<ReAttachRequest>(&mut req).await?;
    let state = get_state(&req);
    json_response(StatusCode::OK, state.service.re_attach(reattach_req).await?)
}

/// Pageserver calls into this before doing deletions, to confirm that it still
/// holds the latest generation for the tenants with deletions enqueued
async fn handle_validate(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::GenerationsApi)?;

    let validate_req = json_request::<ValidateRequest>(&mut req).await?;
    let state = get_state(&req);
    json_response(StatusCode::OK, state.service.validate(validate_req))
}

/// Call into this before attaching a tenant to a pageserver, to acquire a generation number
/// (in the real control plane this is unnecessary, because the same program is managing
///  generation numbers and doing attachments).
async fn handle_attach_hook(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let attach_req = json_request::<AttachHookRequest>(&mut req).await?;
    let state = get_state(&req);

    json_response(
        StatusCode::OK,
        state
            .service
            .attach_hook(attach_req)
            .await
            .map_err(ApiError::InternalServerError)?,
    )
}

async fn handle_inspect(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let inspect_req = json_request::<InspectRequest>(&mut req).await?;

    let state = get_state(&req);

    json_response(StatusCode::OK, state.service.inspect(inspect_req))
}

async fn handle_tenant_create(
    service: Arc<Service>,
    mut req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::PageServerApi)?;

    let create_req = json_request::<TenantCreateRequest>(&mut req).await?;

    json_response(
        StatusCode::CREATED,
        service.tenant_create(create_req).await?,
    )
}

async fn handle_tenant_location_config(
    service: Arc<Service>,
    mut req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_shard_id: TenantShardId = parse_request_param(&req, "tenant_shard_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    let config_req = json_request::<TenantLocationConfigRequest>(&mut req).await?;
    json_response(
        StatusCode::OK,
        service
            .tenant_location_config(tenant_shard_id, config_req)
            .await?,
    )
}

async fn handle_tenant_config_set(
    service: Arc<Service>,
    mut req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::PageServerApi)?;

    let config_req = json_request::<TenantConfigRequest>(&mut req).await?;

    json_response(StatusCode::OK, service.tenant_config_set(config_req).await?)
}

async fn handle_tenant_config_get(
    service: Arc<Service>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    json_response(StatusCode::OK, service.tenant_config_get(tenant_id)?)
}

async fn handle_tenant_time_travel_remote_storage(
    service: Arc<Service>,
    mut req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    let time_travel_req = json_request::<TenantTimeTravelRequest>(&mut req).await?;

    let timestamp_raw = must_get_query_param(&req, "travel_to")?;
    let _timestamp = humantime::parse_rfc3339(&timestamp_raw).map_err(|_e| {
        ApiError::BadRequest(anyhow::anyhow!(
            "Invalid time for travel_to: {timestamp_raw:?}"
        ))
    })?;

    let done_if_after_raw = must_get_query_param(&req, "done_if_after")?;
    let _done_if_after = humantime::parse_rfc3339(&done_if_after_raw).map_err(|_e| {
        ApiError::BadRequest(anyhow::anyhow!(
            "Invalid time for done_if_after: {done_if_after_raw:?}"
        ))
    })?;

    service
        .tenant_time_travel_remote_storage(
            &time_travel_req,
            tenant_id,
            timestamp_raw,
            done_if_after_raw,
        )
        .await?;
    json_response(StatusCode::OK, ())
}

fn map_reqwest_hyper_status(status: reqwest::StatusCode) -> Result<hyper::StatusCode, ApiError> {
    hyper::StatusCode::from_u16(status.as_u16())
        .context("invalid status code")
        .map_err(ApiError::InternalServerError)
}

async fn handle_tenant_secondary_download(
    service: Arc<Service>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    let wait = parse_query_param(&req, "wait_ms")?.map(Duration::from_millis);

    let (status, progress) = service.tenant_secondary_download(tenant_id, wait).await?;
    json_response(map_reqwest_hyper_status(status)?, progress)
}

async fn handle_tenant_delete(
    service: Arc<Service>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    let status_code = service
        .tenant_delete(tenant_id)
        .await
        .and_then(map_reqwest_hyper_status)?;

    if status_code == StatusCode::NOT_FOUND {
        // The pageserver uses 404 for successful deletion, but we use 200
        json_response(StatusCode::OK, ())
    } else {
        json_response(status_code, ())
    }
}

async fn handle_tenant_timeline_create(
    service: Arc<Service>,
    mut req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    let create_req = json_request::<TimelineCreateRequest>(&mut req).await?;
    json_response(
        StatusCode::CREATED,
        service
            .tenant_timeline_create(tenant_id, create_req)
            .await?,
    )
}

async fn handle_tenant_timeline_delete(
    service: Arc<Service>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    let timeline_id: TimelineId = parse_request_param(&req, "timeline_id")?;

    // For timeline deletions, which both implement an "initially return 202, then 404 once
    // we're done" semantic, we wrap with a retry loop to expose a simpler API upstream.
    async fn deletion_wrapper<R, F>(service: Arc<Service>, f: F) -> Result<Response<Body>, ApiError>
    where
        R: std::future::Future<Output = Result<StatusCode, ApiError>> + Send + 'static,
        F: Fn(Arc<Service>) -> R + Send + Sync + 'static,
    {
        let started_at = Instant::now();
        // To keep deletion reasonably snappy for small tenants, initially check after 1 second if deletion
        // completed.
        let mut retry_period = Duration::from_secs(1);
        // On subsequent retries, wait longer.
        let max_retry_period = Duration::from_secs(5);
        // Enable callers with a 30 second request timeout to reliably get a response
        let max_wait = Duration::from_secs(25);

        loop {
            let status = f(service.clone()).await?;
            match status {
                StatusCode::ACCEPTED => {
                    tracing::info!("Deletion accepted, waiting to try again...");
                    tokio::time::sleep(retry_period).await;
                    retry_period = max_retry_period;
                }
                StatusCode::NOT_FOUND => {
                    tracing::info!("Deletion complete");
                    return json_response(StatusCode::OK, ());
                }
                _ => {
                    tracing::warn!("Unexpected status {status}");
                    return json_response(status, ());
                }
            }

            let now = Instant::now();
            if now + retry_period > started_at + max_wait {
                tracing::info!("Deletion timed out waiting for 404");
                // REQUEST_TIMEOUT would be more appropriate, but CONFLICT is already part of
                // the pageserver's swagger definition for this endpoint, and has the same desired
                // effect of causing the control plane to retry later.
                return json_response(StatusCode::CONFLICT, ());
            }
        }
    }

    deletion_wrapper(service, move |service| async move {
        service
            .tenant_timeline_delete(tenant_id, timeline_id)
            .await
            .and_then(map_reqwest_hyper_status)
    })
    .await
}

async fn handle_tenant_timeline_detach_ancestor(
    service: Arc<Service>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    let timeline_id: TimelineId = parse_request_param(&req, "timeline_id")?;

    let res = service
        .tenant_timeline_detach_ancestor(tenant_id, timeline_id)
        .await?;

    json_response(StatusCode::OK, res)
}

async fn handle_tenant_timeline_passthrough(
    service: Arc<Service>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    let Some(path) = req.uri().path_and_query() else {
        // This should never happen, our request router only calls us if there is a path
        return Err(ApiError::BadRequest(anyhow::anyhow!("Missing path")));
    };

    tracing::info!("Proxying request for tenant {} ({})", tenant_id, path);

    // Find the node that holds shard zero
    let (node, tenant_shard_id) = service.tenant_shard0_node(tenant_id)?;

    // Callers will always pass an unsharded tenant ID.  Before proxying, we must
    // rewrite this to a shard-aware shard zero ID.
    let path = format!("{}", path);
    let tenant_str = tenant_id.to_string();
    let tenant_shard_str = format!("{}", tenant_shard_id);
    let path = path.replace(&tenant_str, &tenant_shard_str);

    let latency = &METRICS_REGISTRY
        .metrics_group
        .storage_controller_passthrough_request_latency;

    // This is a bit awkward. We remove the param from the request
    // and join the words by '_' to get a label for the request.
    let just_path = path.replace(&tenant_shard_str, "");
    let path_label = just_path
        .split('/')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    let labels = PageserverRequestLabelGroup {
        pageserver_id: &node.get_id().to_string(),
        path: &path_label,
        method: crate::metrics::Method::Get,
    };

    let _timer = latency.start_timer(labels.clone());

    let client = mgmt_api::Client::new(node.base_url(), service.get_config().jwt_token.as_deref());
    let resp = client.get_raw(path).await.map_err(|_e|
        // FIXME: give APiError a proper Unavailable variant.  We return 503 here because
        // if we can't successfully send a request to the pageserver, we aren't available.
        ApiError::ShuttingDown)?;

    if !resp.status().is_success() {
        let error_counter = &METRICS_REGISTRY
            .metrics_group
            .storage_controller_passthrough_request_error;
        error_counter.inc(labels);
    }

    // We have a reqest::Response, would like a http::Response
    let mut builder = hyper::Response::builder().status(map_reqwest_hyper_status(resp.status())?);
    for (k, v) in resp.headers() {
        builder = builder.header(k.as_str(), v.as_bytes());
    }

    let response = builder
        .body(Body::wrap_stream(resp.bytes_stream()))
        .map_err(|e| ApiError::InternalServerError(e.into()))?;

    Ok(response)
}

async fn handle_tenant_locate(
    service: Arc<Service>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    json_response(StatusCode::OK, service.tenant_locate(tenant_id)?)
}

async fn handle_tenant_describe(
    service: Arc<Service>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Scrubber)?;

    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    json_response(StatusCode::OK, service.tenant_describe(tenant_id)?)
}

async fn handle_tenant_list(
    service: Arc<Service>,
    req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    json_response(StatusCode::OK, service.tenant_list())
}

async fn handle_node_register(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let register_req = json_request::<NodeRegisterRequest>(&mut req).await?;
    let state = get_state(&req);
    state.service.node_register(register_req).await?;
    json_response(StatusCode::OK, ())
}

async fn handle_node_list(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    let nodes = state.service.node_list().await?;
    let api_nodes = nodes.into_iter().map(|n| n.describe()).collect::<Vec<_>>();

    json_response(StatusCode::OK, api_nodes)
}

async fn handle_node_drop(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    let node_id: NodeId = parse_request_param(&req, "node_id")?;
    json_response(StatusCode::OK, state.service.node_drop(node_id).await?)
}

async fn handle_node_delete(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    let node_id: NodeId = parse_request_param(&req, "node_id")?;
    json_response(StatusCode::OK, state.service.node_delete(node_id).await?)
}

async fn handle_node_configure(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let node_id: NodeId = parse_request_param(&req, "node_id")?;
    let config_req = json_request::<NodeConfigureRequest>(&mut req).await?;
    if node_id != config_req.node_id {
        return Err(ApiError::BadRequest(anyhow::anyhow!(
            "Path and body node_id differ"
        )));
    }
    let state = get_state(&req);

    json_response(
        StatusCode::OK,
        state
            .service
            .node_configure(
                config_req.node_id,
                config_req.availability.map(NodeAvailability::from),
                config_req.scheduling,
            )
            .await?,
    )
}

async fn handle_node_status(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    let node_id: NodeId = parse_request_param(&req, "node_id")?;

    let node_status = state.service.get_node(node_id).await?;

    json_response(StatusCode::OK, node_status)
}

async fn handle_node_drain(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    let node_id: NodeId = parse_request_param(&req, "node_id")?;

    state.service.start_node_drain(node_id).await?;

    json_response(StatusCode::ACCEPTED, ())
}

async fn handle_cancel_node_drain(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    let node_id: NodeId = parse_request_param(&req, "node_id")?;

    state.service.cancel_node_drain(node_id).await?;

    json_response(StatusCode::ACCEPTED, ())
}

async fn handle_node_fill(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    let node_id: NodeId = parse_request_param(&req, "node_id")?;

    state.service.start_node_fill(node_id).await?;

    json_response(StatusCode::ACCEPTED, ())
}

async fn handle_cancel_node_fill(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    let node_id: NodeId = parse_request_param(&req, "node_id")?;

    state.service.cancel_node_fill(node_id).await?;

    json_response(StatusCode::ACCEPTED, ())
}

async fn handle_tenant_shard_split(
    service: Arc<Service>,
    mut req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    let split_req = json_request::<TenantShardSplitRequest>(&mut req).await?;

    json_response(
        StatusCode::OK,
        service.tenant_shard_split(tenant_id, split_req).await?,
    )
}

async fn handle_tenant_shard_migrate(
    service: Arc<Service>,
    mut req: Request<Body>,
) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let tenant_shard_id: TenantShardId = parse_request_param(&req, "tenant_shard_id")?;
    let migrate_req = json_request::<TenantShardMigrateRequest>(&mut req).await?;
    json_response(
        StatusCode::OK,
        service
            .tenant_shard_migrate(tenant_shard_id, migrate_req)
            .await?,
    )
}

async fn handle_tenant_update_policy(mut req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    let update_req = json_request::<TenantPolicyRequest>(&mut req).await?;
    let state = get_state(&req);

    json_response(
        StatusCode::OK,
        state
            .service
            .tenant_update_policy(tenant_id, update_req)
            .await?,
    )
}

async fn handle_tenant_drop(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    let state = get_state(&req);

    json_response(StatusCode::OK, state.service.tenant_drop(tenant_id).await?)
}

async fn handle_tenant_import(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let tenant_id: TenantId = parse_request_param(&req, "tenant_id")?;
    check_permissions(&req, Scope::PageServerApi)?;

    let state = get_state(&req);

    json_response(
        StatusCode::OK,
        state.service.tenant_import(tenant_id).await?,
    )
}

async fn handle_tenants_dump(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    state.service.tenants_dump()
}

async fn handle_scheduler_dump(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);
    state.service.scheduler_dump()
}

async fn handle_consistency_check(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);

    json_response(StatusCode::OK, state.service.consistency_check().await?)
}

async fn handle_reconcile_all(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    check_permissions(&req, Scope::Admin)?;

    let state = get_state(&req);

    json_response(StatusCode::OK, state.service.reconcile_all_now().await?)
}

/// Status endpoint is just used for checking that our HTTP listener is up
async fn handle_status(_req: Request<Body>) -> Result<Response<Body>, ApiError> {
    json_response(StatusCode::OK, ())
}

/// Readiness endpoint indicates when we're done doing startup I/O (e.g. reconciling
/// with remote pageserver nodes).  This is intended for use as a kubernetes readiness probe.
async fn handle_ready(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    let state = get_state(&req);
    if state.service.startup_complete.is_ready() {
        json_response(StatusCode::OK, ())
    } else {
        json_response(StatusCode::SERVICE_UNAVAILABLE, ())
    }
}

impl From<ReconcileError> for ApiError {
    fn from(value: ReconcileError) -> Self {
        ApiError::Conflict(format!("Reconciliation error: {}", value))
    }
}

/// Common wrapper for request handlers that call into Service and will operate on tenants: they must only
/// be allowed to run if Service has finished its initial reconciliation.
async fn tenant_service_handler<R, H>(
    request: Request<Body>,
    handler: H,
    request_name: RequestName,
) -> R::Output
where
    R: std::future::Future<Output = Result<Response<Body>, ApiError>> + Send + 'static,
    H: FnOnce(Arc<Service>, Request<Body>) -> R + Send + Sync + 'static,
{
    let state = get_state(&request);
    let service = state.service.clone();

    let startup_complete = service.startup_complete.clone();
    if tokio::time::timeout(STARTUP_RECONCILE_TIMEOUT, startup_complete.wait())
        .await
        .is_err()
    {
        // This shouldn't happen: it is the responsibilty of [`Service::startup_reconcile`] to use appropriate
        // timeouts around its remote calls, to bound its runtime.
        return Err(ApiError::Timeout(
            "Timed out waiting for service readiness".into(),
        ));
    }

    named_request_span(
        request,
        |request| async move { handler(service, request).await },
        request_name,
    )
    .await
}

/// Check if the required scope is held in the request's token, or if the request has
/// a token with 'admin' scope then always permit it.
fn check_permissions(request: &Request<Body>, required_scope: Scope) -> Result<(), ApiError> {
    check_permission_with(request, |claims| {
        match crate::auth::check_permission(claims, required_scope) {
            Err(e) => match crate::auth::check_permission(claims, Scope::Admin) {
                Ok(()) => Ok(()),
                Err(_) => Err(e),
            },
            Ok(()) => Ok(()),
        }
    })
}

#[derive(Clone, Debug)]
struct RequestMeta {
    method: hyper::http::Method,
    at: Instant,
}

fn prologue_metrics_middleware<B: hyper::body::HttpBody + Send + Sync + 'static>(
) -> Middleware<B, ApiError> {
    Middleware::pre(move |req| async move {
        let meta = RequestMeta {
            method: req.method().clone(),
            at: Instant::now(),
        };

        req.set_context(meta);

        Ok(req)
    })
}

fn epilogue_metrics_middleware<B: hyper::body::HttpBody + Send + Sync + 'static>(
) -> Middleware<B, ApiError> {
    Middleware::post_with_info(move |resp, req_info| async move {
        let request_name = match req_info.context::<RequestName>() {
            Some(name) => name,
            None => {
                return Ok(resp);
            }
        };

        if let Some(meta) = req_info.context::<RequestMeta>() {
            let status = &crate::metrics::METRICS_REGISTRY
                .metrics_group
                .storage_controller_http_request_status;
            let latency = &crate::metrics::METRICS_REGISTRY
                .metrics_group
                .storage_controller_http_request_latency;

            status.inc(HttpRequestStatusLabelGroup {
                path: request_name.0,
                method: meta.method.clone().into(),
                status: crate::metrics::StatusCode(resp.status()),
            });

            latency.observe(
                HttpRequestLatencyLabelGroup {
                    path: request_name.0,
                    method: meta.method.into(),
                },
                meta.at.elapsed().as_secs_f64(),
            );
        }
        Ok(resp)
    })
}

pub async fn measured_metrics_handler(req: Request<Body>) -> Result<Response<Body>, ApiError> {
    pub const TEXT_FORMAT: &str = "text/plain; version=0.0.4";

    let state = get_state(&req);
    let payload = crate::metrics::METRICS_REGISTRY.encode(&state.neon_metrics);
    let response = Response::builder()
        .status(200)
        .header(CONTENT_TYPE, TEXT_FORMAT)
        .body(payload.into())
        .unwrap();

    Ok(response)
}

#[derive(Clone)]
struct RequestName(&'static str);

async fn named_request_span<R, H>(
    request: Request<Body>,
    handler: H,
    name: RequestName,
) -> R::Output
where
    R: Future<Output = Result<Response<Body>, ApiError>> + Send + 'static,
    H: FnOnce(Request<Body>) -> R + Send + Sync + 'static,
{
    request.set_context(name);
    request_span(request, handler).await
}

pub fn make_router(
    service: Arc<Service>,
    auth: Option<Arc<SwappableJwtAuth>>,
    build_info: BuildInfo,
) -> RouterBuilder<hyper::Body, ApiError> {
    let mut router = endpoint::make_router()
        .middleware(prologue_metrics_middleware())
        .middleware(epilogue_metrics_middleware());
    if auth.is_some() {
        router = router.middleware(auth_middleware(|request| {
            let state = get_state(request);
            if state.allowlist_routes.contains(request.uri()) {
                None
            } else {
                state.auth.as_deref()
            }
        }));
    }

    router
        .data(Arc::new(HttpState::new(service, auth, build_info)))
        .get("/metrics", |r| {
            named_request_span(r, measured_metrics_handler, RequestName("metrics"))
        })
        // Non-prefixed generic endpoints (status, metrics)
        .get("/status", |r| {
            named_request_span(r, handle_status, RequestName("status"))
        })
        .get("/ready", |r| {
            named_request_span(r, handle_ready, RequestName("ready"))
        })
        // Upcalls for the pageserver: point the pageserver's `control_plane_api` config to this prefix
        .post("/upcall/v1/re-attach", |r| {
            named_request_span(r, handle_re_attach, RequestName("upcall_v1_reattach"))
        })
        .post("/upcall/v1/validate", |r| {
            named_request_span(r, handle_validate, RequestName("upcall_v1_validate"))
        })
        // Test/dev/debug endpoints
        .post("/debug/v1/attach-hook", |r| {
            named_request_span(r, handle_attach_hook, RequestName("debug_v1_attach_hook"))
        })
        .post("/debug/v1/inspect", |r| {
            named_request_span(r, handle_inspect, RequestName("debug_v1_inspect"))
        })
        .post("/debug/v1/tenant/:tenant_id/drop", |r| {
            named_request_span(r, handle_tenant_drop, RequestName("debug_v1_tenant_drop"))
        })
        .post("/debug/v1/node/:node_id/drop", |r| {
            named_request_span(r, handle_node_drop, RequestName("debug_v1_node_drop"))
        })
        .post("/debug/v1/tenant/:tenant_id/import", |r| {
            named_request_span(
                r,
                handle_tenant_import,
                RequestName("debug_v1_tenant_import"),
            )
        })
        .get("/debug/v1/tenant", |r| {
            named_request_span(r, handle_tenants_dump, RequestName("debug_v1_tenant"))
        })
        .get("/debug/v1/tenant/:tenant_id/locate", |r| {
            tenant_service_handler(
                r,
                handle_tenant_locate,
                RequestName("debug_v1_tenant_locate"),
            )
        })
        .get("/debug/v1/scheduler", |r| {
            named_request_span(r, handle_scheduler_dump, RequestName("debug_v1_scheduler"))
        })
        .post("/debug/v1/consistency_check", |r| {
            named_request_span(
                r,
                handle_consistency_check,
                RequestName("debug_v1_consistency_check"),
            )
        })
        .post("/debug/v1/reconcile_all", |r| {
            request_span(r, handle_reconcile_all)
        })
        .put("/debug/v1/failpoints", |r| {
            request_span(r, |r| failpoints_handler(r, CancellationToken::new()))
        })
        // Node operations
        .post("/control/v1/node", |r| {
            named_request_span(r, handle_node_register, RequestName("control_v1_node"))
        })
        .delete("/control/v1/node/:node_id", |r| {
            named_request_span(r, handle_node_delete, RequestName("control_v1_node_delete"))
        })
        .get("/control/v1/node", |r| {
            named_request_span(r, handle_node_list, RequestName("control_v1_node"))
        })
        .put("/control/v1/node/:node_id/config", |r| {
            named_request_span(
                r,
                handle_node_configure,
                RequestName("control_v1_node_config"),
            )
        })
        .get("/control/v1/node/:node_id", |r| {
            named_request_span(r, handle_node_status, RequestName("control_v1_node_status"))
        })
        .put("/control/v1/node/:node_id/drain", |r| {
            named_request_span(r, handle_node_drain, RequestName("control_v1_node_drain"))
        })
        .delete("/control/v1/node/:node_id/drain", |r| {
            named_request_span(
                r,
                handle_cancel_node_drain,
                RequestName("control_v1_cancel_node_drain"),
            )
        })
        .put("/control/v1/node/:node_id/fill", |r| {
            named_request_span(r, handle_node_fill, RequestName("control_v1_node_fill"))
        })
        .delete("/control/v1/node/:node_id/fill", |r| {
            named_request_span(
                r,
                handle_cancel_node_fill,
                RequestName("control_v1_cancel_node_fill"),
            )
        })
        // TODO(vlad): endpoint for cancelling drain and fill
        // Tenant Shard operations
        .put("/control/v1/tenant/:tenant_shard_id/migrate", |r| {
            tenant_service_handler(
                r,
                handle_tenant_shard_migrate,
                RequestName("control_v1_tenant_migrate"),
            )
        })
        .put("/control/v1/tenant/:tenant_id/shard_split", |r| {
            tenant_service_handler(
                r,
                handle_tenant_shard_split,
                RequestName("control_v1_tenant_shard_split"),
            )
        })
        .get("/control/v1/tenant/:tenant_id", |r| {
            tenant_service_handler(
                r,
                handle_tenant_describe,
                RequestName("control_v1_tenant_describe"),
            )
        })
        .get("/control/v1/tenant", |r| {
            tenant_service_handler(r, handle_tenant_list, RequestName("control_v1_tenant_list"))
        })
        .put("/control/v1/tenant/:tenant_id/policy", |r| {
            named_request_span(
                r,
                handle_tenant_update_policy,
                RequestName("control_v1_tenant_policy"),
            )
        })
        // Tenant operations
        // The ^/v1/ endpoints act as a "Virtual Pageserver", enabling shard-naive clients to call into
        // this service to manage tenants that actually consist of many tenant shards, as if they are a single entity.
        .post("/v1/tenant", |r| {
            tenant_service_handler(r, handle_tenant_create, RequestName("v1_tenant"))
        })
        .delete("/v1/tenant/:tenant_id", |r| {
            tenant_service_handler(r, handle_tenant_delete, RequestName("v1_tenant"))
        })
        .put("/v1/tenant/config", |r| {
            tenant_service_handler(r, handle_tenant_config_set, RequestName("v1_tenant_config"))
        })
        .get("/v1/tenant/:tenant_id/config", |r| {
            tenant_service_handler(r, handle_tenant_config_get, RequestName("v1_tenant_config"))
        })
        .put("/v1/tenant/:tenant_shard_id/location_config", |r| {
            tenant_service_handler(
                r,
                handle_tenant_location_config,
                RequestName("v1_tenant_location_config"),
            )
        })
        .put("/v1/tenant/:tenant_id/time_travel_remote_storage", |r| {
            tenant_service_handler(
                r,
                handle_tenant_time_travel_remote_storage,
                RequestName("v1_tenant_time_travel_remote_storage"),
            )
        })
        .post("/v1/tenant/:tenant_id/secondary/download", |r| {
            tenant_service_handler(
                r,
                handle_tenant_secondary_download,
                RequestName("v1_tenant_secondary_download"),
            )
        })
        // Timeline operations
        .delete("/v1/tenant/:tenant_id/timeline/:timeline_id", |r| {
            tenant_service_handler(
                r,
                handle_tenant_timeline_delete,
                RequestName("v1_tenant_timeline"),
            )
        })
        .post("/v1/tenant/:tenant_id/timeline", |r| {
            tenant_service_handler(
                r,
                handle_tenant_timeline_create,
                RequestName("v1_tenant_timeline"),
            )
        })
        .put(
            "/v1/tenant/:tenant_id/timeline/:timeline_id/detach_ancestor",
            |r| {
                tenant_service_handler(
                    r,
                    handle_tenant_timeline_detach_ancestor,
                    RequestName("v1_tenant_timeline_detach_ancestor"),
                )
            },
        )
        // Tenant detail GET passthrough to shard zero:
        .get("/v1/tenant/:tenant_id", |r| {
            tenant_service_handler(
                r,
                handle_tenant_timeline_passthrough,
                RequestName("v1_tenant_passthrough"),
            )
        })
        // The `*` in the  URL is a wildcard: any tenant/timeline GET APIs on the pageserver
        // are implicitly exposed here.  This must be last in the list to avoid
        // taking precedence over other GET methods we might implement by hand.
        .get("/v1/tenant/:tenant_id/*", |r| {
            tenant_service_handler(
                r,
                handle_tenant_timeline_passthrough,
                RequestName("v1_tenant_passthrough"),
            )
        })
}
