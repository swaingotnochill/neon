//! The Page Service listens for client connections and serves their GetPage@LSN
//! requests.

use anyhow::Context;
use async_compression::tokio::write::GzipEncoder;
use bytes::Buf;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use pageserver_api::key::Key;
use pageserver_api::models::TenantState;
use pageserver_api::models::{
    PagestreamBeMessage, PagestreamDbSizeRequest, PagestreamDbSizeResponse,
    PagestreamErrorResponse, PagestreamExistsRequest, PagestreamExistsResponse,
    PagestreamFeMessage, PagestreamGetPageRequest, PagestreamGetPageResponse,
    PagestreamGetSlruSegmentRequest, PagestreamGetSlruSegmentResponse, PagestreamNblocksRequest,
    PagestreamNblocksResponse, PagestreamProtocolVersion,
};
use pageserver_api::shard::ShardIndex;
use pageserver_api::shard::ShardNumber;
use pageserver_api::shard::TenantShardId;
use postgres_backend::{is_expected_io_error, AuthType, PostgresBackend, QueryError};
use pq_proto::framed::ConnectionError;
use pq_proto::FeStartupPacket;
use pq_proto::{BeMessage, FeMessage, RowDescriptor};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::str;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tracing::*;
use utils::sync::gate::GateGuard;
use utils::{
    auth::{Claims, Scope, SwappableJwtAuth},
    id::{TenantId, TimelineId},
    lsn::Lsn,
    simple_rcu::RcuReadGuard,
};

use crate::auth::check_permission;
use crate::basebackup;
use crate::basebackup::BasebackupError;
use crate::context::{DownloadBehavior, RequestContext};
use crate::metrics;
use crate::metrics::{ComputeCommandKind, COMPUTE_COMMANDS_COUNTERS, LIVE_CONNECTIONS};
use crate::pgdatadir_mapping::Version;
use crate::span::debug_assert_current_span_has_tenant_and_timeline_id;
use crate::span::debug_assert_current_span_has_tenant_and_timeline_id_no_shard_id;
use crate::task_mgr;
use crate::task_mgr::TaskKind;
use crate::tenant::mgr::GetActiveTenantError;
use crate::tenant::mgr::GetTenantError;
use crate::tenant::mgr::ShardResolveResult;
use crate::tenant::mgr::ShardSelector;
use crate::tenant::mgr::TenantManager;
use crate::tenant::timeline::WaitLsnError;
use crate::tenant::GetTimelineError;
use crate::tenant::PageReconstructError;
use crate::tenant::Tenant;
use crate::tenant::Timeline;
use pageserver_api::key::rel_block_to_key;
use pageserver_api::reltag::SlruKind;
use postgres_ffi::pg_constants::DEFAULTTABLESPACE_OID;
use postgres_ffi::BLCKSZ;

// How long we may wait for a [`TenantSlot::InProgress`]` and/or a [`Tenant`] which
// is not yet in state [`TenantState::Active`].
const ACTIVE_TENANT_TIMEOUT: Duration = Duration::from_millis(30000);

///////////////////////////////////////////////////////////////////////////////

///
/// Main loop of the page service.
///
/// Listens for connections, and launches a new handler task for each.
///
pub async fn libpq_listener_main(
    tenant_manager: Arc<TenantManager>,
    auth: Option<Arc<SwappableJwtAuth>>,
    listener: TcpListener,
    auth_type: AuthType,
    listener_ctx: RequestContext,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    listener.set_nonblocking(true)?;
    let tokio_listener = tokio::net::TcpListener::from_std(listener)?;

    // Wait for a new connection to arrive, or for server shutdown.
    while let Some(res) = tokio::select! {
        biased;

        _ = cancel.cancelled() => {
            // We were requested to shut down.
            None
        }

        res = tokio_listener.accept() => {
            Some(res)
        }
    } {
        match res {
            Ok((socket, peer_addr)) => {
                // Connection established. Spawn a new task to handle it.
                debug!("accepted connection from {}", peer_addr);
                let local_auth = auth.clone();

                let connection_ctx = listener_ctx
                    .detached_child(TaskKind::PageRequestHandler, DownloadBehavior::Download);

                // PageRequestHandler tasks are not associated with any particular
                // timeline in the task manager. In practice most connections will
                // only deal with a particular timeline, but we don't know which one
                // yet.
                task_mgr::spawn(
                    &tokio::runtime::Handle::current(),
                    TaskKind::PageRequestHandler,
                    None,
                    None,
                    "serving compute connection task",
                    false,
                    page_service_conn_main(
                        tenant_manager.clone(),
                        local_auth,
                        socket,
                        auth_type,
                        connection_ctx,
                    ),
                );
            }
            Err(err) => {
                // accept() failed. Log the error, and loop back to retry on next connection.
                error!("accept() failed: {:?}", err);
            }
        }
    }

    debug!("page_service loop terminated");

    Ok(())
}

#[instrument(skip_all, fields(peer_addr))]
async fn page_service_conn_main(
    tenant_manager: Arc<TenantManager>,
    auth: Option<Arc<SwappableJwtAuth>>,
    socket: tokio::net::TcpStream,
    auth_type: AuthType,
    connection_ctx: RequestContext,
) -> anyhow::Result<()> {
    let _guard = LIVE_CONNECTIONS
        .with_label_values(&["page_service"])
        .guard();

    socket
        .set_nodelay(true)
        .context("could not set TCP_NODELAY")?;

    let peer_addr = socket.peer_addr().context("get peer address")?;
    tracing::Span::current().record("peer_addr", field::display(peer_addr));

    // setup read timeout of 10 minutes. the timeout is rather arbitrary for requirements:
    // - long enough for most valid compute connections
    // - less than infinite to stop us from "leaking" connections to long-gone computes
    //
    // no write timeout is used, because the kernel is assumed to error writes after some time.
    let mut socket = tokio_io_timeout::TimeoutReader::new(socket);

    let default_timeout_ms = 10 * 60 * 1000; // 10 minutes by default
    let socket_timeout_ms = (|| {
        fail::fail_point!("simulated-bad-compute-connection", |avg_timeout_ms| {
            // Exponential distribution for simulating
            // poor network conditions, expect about avg_timeout_ms to be around 15
            // in tests
            if let Some(avg_timeout_ms) = avg_timeout_ms {
                let avg = avg_timeout_ms.parse::<i64>().unwrap() as f32;
                let u = rand::random::<f32>();
                ((1.0 - u).ln() / (-avg)) as u64
            } else {
                default_timeout_ms
            }
        });
        default_timeout_ms
    })();

    // A timeout here does not mean the client died, it can happen if it's just idle for
    // a while: we will tear down this PageServerHandler and instantiate a new one if/when
    // they reconnect.
    socket.set_timeout(Some(std::time::Duration::from_millis(socket_timeout_ms)));
    let socket = std::pin::pin!(socket);

    fail::fail_point!("ps::connection-start::pre-login");

    // XXX: pgbackend.run() should take the connection_ctx,
    // and create a child per-query context when it invokes process_query.
    // But it's in a shared crate, so, we store connection_ctx inside PageServerHandler
    // and create the per-query context in process_query ourselves.
    let mut conn_handler = PageServerHandler::new(tenant_manager, auth, connection_ctx);
    let pgbackend = PostgresBackend::new_from_io(socket, peer_addr, auth_type, None)?;

    match pgbackend
        .run(&mut conn_handler, &task_mgr::shutdown_token())
        .await
    {
        Ok(()) => {
            // we've been requested to shut down
            Ok(())
        }
        Err(QueryError::Disconnected(ConnectionError::Io(io_error))) => {
            if is_expected_io_error(&io_error) {
                info!("Postgres client disconnected ({io_error})");
                Ok(())
            } else {
                Err(io_error).context("Postgres connection error")
            }
        }
        other => other.context("Postgres query error"),
    }
}

/// While a handler holds a reference to a Timeline, it also holds a the
/// timeline's Gate open.
struct HandlerTimeline {
    timeline: Arc<Timeline>,
    _guard: GateGuard,
}

struct PageServerHandler {
    auth: Option<Arc<SwappableJwtAuth>>,
    claims: Option<Claims>,

    tenant_manager: Arc<TenantManager>,

    /// The context created for the lifetime of the connection
    /// services by this PageServerHandler.
    /// For each query received over the connection,
    /// `process_query` creates a child context from this one.
    connection_ctx: RequestContext,

    /// See [`Self::cache_timeline`] for usage.
    ///
    /// Note on size: the typical size of this map is 1.  The largest size we expect
    /// to see is the number of shards divided by the number of pageservers (typically < 2),
    /// or the ratio used when splitting shards (i.e. how many children created from one)
    /// parent shard, where a "large" number might be ~8.
    shard_timelines: HashMap<ShardIndex, HandlerTimeline>,
}

#[derive(thiserror::Error, Debug)]
enum PageStreamError {
    /// We encountered an error that should prompt the client to reconnect:
    /// in practice this means we drop the connection without sending a response.
    #[error("Reconnect required: {0}")]
    Reconnect(Cow<'static, str>),

    /// We were instructed to shutdown while processing the query
    #[error("Shutting down")]
    Shutdown,

    /// Something went wrong reading a page: this likely indicates a pageserver bug
    #[error("Read error")]
    Read(#[source] PageReconstructError),

    /// Ran out of time waiting for an LSN
    #[error("LSN timeout: {0}")]
    LsnTimeout(WaitLsnError),

    /// The entity required to serve the request (tenant or timeline) is not found,
    /// or is not found in a suitable state to serve a request.
    #[error("Not found: {0}")]
    NotFound(Cow<'static, str>),

    /// Request asked for something that doesn't make sense, like an invalid LSN
    #[error("Bad request: {0}")]
    BadRequest(Cow<'static, str>),
}

impl From<PageReconstructError> for PageStreamError {
    fn from(value: PageReconstructError) -> Self {
        match value {
            PageReconstructError::Cancelled => Self::Shutdown,
            e => Self::Read(e),
        }
    }
}

impl From<GetActiveTimelineError> for PageStreamError {
    fn from(value: GetActiveTimelineError) -> Self {
        match value {
            GetActiveTimelineError::Tenant(GetActiveTenantError::Cancelled) => Self::Shutdown,
            GetActiveTimelineError::Tenant(e) => Self::NotFound(format!("{e}").into()),
            GetActiveTimelineError::Timeline(e) => Self::NotFound(format!("{e}").into()),
        }
    }
}

impl From<WaitLsnError> for PageStreamError {
    fn from(value: WaitLsnError) -> Self {
        match value {
            e @ WaitLsnError::Timeout(_) => Self::LsnTimeout(e),
            WaitLsnError::Shutdown => Self::Shutdown,
            e @ WaitLsnError::BadState { .. } => Self::Reconnect(format!("{e}").into()),
        }
    }
}

impl From<WaitLsnError> for QueryError {
    fn from(value: WaitLsnError) -> Self {
        match value {
            e @ WaitLsnError::Timeout(_) => Self::Other(anyhow::Error::new(e)),
            WaitLsnError::Shutdown => Self::Shutdown,
            WaitLsnError::BadState { .. } => Self::Reconnect,
        }
    }
}

impl PageServerHandler {
    pub fn new(
        tenant_manager: Arc<TenantManager>,
        auth: Option<Arc<SwappableJwtAuth>>,
        connection_ctx: RequestContext,
    ) -> Self {
        PageServerHandler {
            tenant_manager,
            auth,
            claims: None,
            connection_ctx,
            shard_timelines: HashMap::new(),
        }
    }

    /// Future that completes when we need to shut down the connection.
    ///
    /// We currently need to shut down when any of the following happens:
    /// 1. any of the timelines we hold GateGuards for in `shard_timelines` is cancelled
    /// 2. task_mgr requests shutdown of the connection
    ///
    /// NB on (1): the connection's lifecycle is not actually tied to any of the
    /// `shard_timelines`s' lifecycles. But it's _necessary_ in the current
    /// implementation to be responsive to timeline cancellation because
    /// the connection holds their `GateGuards` open (sored in `shard_timelines`).
    /// We currently do the easy thing and terminate the connection if any of the
    /// shard_timelines gets cancelled. But really, we cuold spend more effort
    /// and simply remove the cancelled timeline from the `shard_timelines`, thereby
    /// dropping the guard.
    ///
    /// NB: keep in sync with [`Self::is_connection_cancelled`]
    async fn await_connection_cancelled(&self) {
        // A short wait before we expend the cycles to walk our timeline map.  This avoids incurring
        // that cost every time we check for cancellation.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // This function is never called concurrently with code that adds timelines to shard_timelines,
        // which is enforced by the borrow checker (the future returned by this function carries the
        // immutable &self).  So it's fine to evaluate shard_timelines after the sleep, we don't risk
        // missing any inserts to the map.

        let mut cancellation_sources = Vec::with_capacity(1 + self.shard_timelines.len());
        use futures::future::Either;
        cancellation_sources.push(Either::Left(task_mgr::shutdown_watcher()));
        cancellation_sources.extend(
            self.shard_timelines
                .values()
                .map(|ht| Either::Right(ht.timeline.cancel.cancelled())),
        );
        FuturesUnordered::from_iter(cancellation_sources)
            .next()
            .await;
    }

    /// Checking variant of [`Self::await_connection_cancelled`].
    fn is_connection_cancelled(&self) -> bool {
        task_mgr::is_shutdown_requested()
            || self
                .shard_timelines
                .values()
                .any(|ht| ht.timeline.cancel.is_cancelled() || ht.timeline.is_stopping())
    }

    /// This function always respects cancellation of any timeline in `[Self::shard_timelines]`.  Pass in
    /// a cancellation token at the next scope up (such as a tenant cancellation token) to ensure we respect
    /// cancellation if there aren't any timelines in the cache.
    ///
    /// If calling from a function that doesn't use the `[Self::shard_timelines]` cache, then pass in the
    /// timeline cancellation token.
    async fn flush_cancellable<IO>(
        &self,
        pgb: &mut PostgresBackend<IO>,
        cancel: &CancellationToken,
    ) -> Result<(), QueryError>
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        tokio::select!(
            flush_r = pgb.flush() => {
                Ok(flush_r?)
            },
            _ = self.await_connection_cancelled() => {
                Err(QueryError::Shutdown)
            }
            _ = cancel.cancelled() => {
                Err(QueryError::Shutdown)
            }
        )
    }

    #[instrument(skip_all)]
    async fn handle_pagerequests<IO>(
        &mut self,
        pgb: &mut PostgresBackend<IO>,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        protocol_version: PagestreamProtocolVersion,
        ctx: RequestContext,
    ) -> Result<(), QueryError>
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        debug_assert_current_span_has_tenant_and_timeline_id_no_shard_id();

        let tenant = self
            .get_active_tenant_with_timeout(tenant_id, ShardSelector::First, ACTIVE_TENANT_TIMEOUT)
            .await?;

        // switch client to COPYBOTH
        pgb.write_message_noflush(&BeMessage::CopyBothResponse)?;
        self.flush_cancellable(pgb, &tenant.cancel).await?;

        loop {
            let msg = tokio::select! {
                biased;

                _ = self.await_connection_cancelled() => {
                    // We were requested to shut down.
                    info!("shutdown request received in page handler");
                    return Err(QueryError::Shutdown)
                }

                msg = pgb.read_message() => { msg }
            };

            let copy_data_bytes = match msg? {
                Some(FeMessage::CopyData(bytes)) => bytes,
                Some(FeMessage::Terminate) => break,
                Some(m) => {
                    return Err(QueryError::Other(anyhow::anyhow!(
                        "unexpected message: {m:?} during COPY"
                    )));
                }
                None => break, // client disconnected
            };

            trace!("query: {copy_data_bytes:?}");
            fail::fail_point!("ps::handle-pagerequest-message");

            let neon_fe_msg =
                PagestreamFeMessage::parse(&mut copy_data_bytes.reader(), protocol_version)?;

            // TODO: We could create a new per-request context here, with unique ID.
            // Currently we use the same per-timeline context for all requests

            let (response, span) = match neon_fe_msg {
                PagestreamFeMessage::Exists(req) => {
                    fail::fail_point!("ps::handle-pagerequest-message::exists");
                    let span = tracing::info_span!("handle_get_rel_exists_request", rel = %req.rel, req_lsn = %req.request_lsn);
                    (
                        self.handle_get_rel_exists_request(tenant_id, timeline_id, &req, &ctx)
                            .instrument(span.clone())
                            .await,
                        span,
                    )
                }
                PagestreamFeMessage::Nblocks(req) => {
                    fail::fail_point!("ps::handle-pagerequest-message::nblocks");
                    let span = tracing::info_span!("handle_get_nblocks_request", rel = %req.rel, req_lsn = %req.request_lsn);
                    (
                        self.handle_get_nblocks_request(tenant_id, timeline_id, &req, &ctx)
                            .instrument(span.clone())
                            .await,
                        span,
                    )
                }
                PagestreamFeMessage::GetPage(req) => {
                    fail::fail_point!("ps::handle-pagerequest-message::getpage");
                    // shard_id is filled in by the handler
                    let span = tracing::info_span!("handle_get_page_at_lsn_request", rel = %req.rel, blkno = %req.blkno, req_lsn = %req.request_lsn);
                    (
                        self.handle_get_page_at_lsn_request(tenant_id, timeline_id, &req, &ctx)
                            .instrument(span.clone())
                            .await,
                        span,
                    )
                }
                PagestreamFeMessage::DbSize(req) => {
                    fail::fail_point!("ps::handle-pagerequest-message::dbsize");
                    let span = tracing::info_span!("handle_db_size_request", dbnode = %req.dbnode, req_lsn = %req.request_lsn);
                    (
                        self.handle_db_size_request(tenant_id, timeline_id, &req, &ctx)
                            .instrument(span.clone())
                            .await,
                        span,
                    )
                }
                PagestreamFeMessage::GetSlruSegment(req) => {
                    fail::fail_point!("ps::handle-pagerequest-message::slrusegment");
                    let span = tracing::info_span!("handle_get_slru_segment_request", kind = %req.kind, segno = %req.segno, req_lsn = %req.request_lsn);
                    (
                        self.handle_get_slru_segment_request(tenant_id, timeline_id, &req, &ctx)
                            .instrument(span.clone())
                            .await,
                        span,
                    )
                }
            };

            match response {
                Err(PageStreamError::Shutdown) => {
                    // If we fail to fulfil a request during shutdown, which may be _because_ of
                    // shutdown, then do not send the error to the client.  Instead just drop the
                    // connection.
                    span.in_scope(|| info!("dropping connection due to shutdown"));
                    return Err(QueryError::Shutdown);
                }
                Err(PageStreamError::Reconnect(reason)) => {
                    span.in_scope(|| info!("handler requested reconnect: {reason}"));
                    return Err(QueryError::Reconnect);
                }
                Err(e) if self.is_connection_cancelled() => {
                    // This branch accomodates code within request handlers that returns an anyhow::Error instead of a clean
                    // shutdown error, this may be buried inside a PageReconstructError::Other for example.
                    //
                    // Requests may fail as soon as we are Stopping, even if the Timeline's cancellation token wasn't fired yet,
                    // because wait_lsn etc will drop out
                    // is_stopping(): [`Timeline::flush_and_shutdown`] has entered
                    // is_canceled(): [`Timeline::shutdown`]` has entered
                    span.in_scope(|| info!("dropped error response during shutdown: {e:#}"));
                    return Err(QueryError::Shutdown);
                }
                r => {
                    let response_msg = r.unwrap_or_else(|e| {
                        // print the all details to the log with {:#}, but for the client the
                        // error message is enough.  Do not log if shutting down, as the anyhow::Error
                        // here includes cancellation which is not an error.
                        let full = utils::error::report_compact_sources(&e);
                        span.in_scope(|| {
                            error!("error reading relation or page version: {full:#}")
                        });
                        PagestreamBeMessage::Error(PagestreamErrorResponse {
                            message: e.to_string(),
                        })
                    });

                    pgb.write_message_noflush(&BeMessage::CopyData(&response_msg.serialize()))?;
                    self.flush_cancellable(pgb, &tenant.cancel).await?;
                }
            }
        }
        Ok(())
    }

    /// Helper function to handle the LSN from client request.
    ///
    /// Each GetPage (and Exists and Nblocks) request includes information about
    /// which version of the page is being requested. The primary compute node
    /// will always request the latest page version, by setting 'request_lsn' to
    /// the last inserted or flushed WAL position, while a standby will request
    /// a version at the LSN that it's currently caught up to.
    ///
    /// In either case, if the page server hasn't received the WAL up to the
    /// requested LSN yet, we will wait for it to arrive. The return value is
    /// the LSN that should be used to look up the page versions.
    ///
    /// In addition to the request LSN, each request carries another LSN,
    /// 'not_modified_since', which is a hint to the pageserver that the client
    /// knows that the page has not been modified between 'not_modified_since'
    /// and the request LSN. This allows skipping the wait, as long as the WAL
    /// up to 'not_modified_since' has arrived. If the client doesn't have any
    /// information about when the page was modified, it will use
    /// not_modified_since == lsn. If the client lies and sends a too low
    /// not_modified_hint such that there are in fact later page versions, the
    /// behavior is undefined: the pageserver may return any of the page versions
    /// or an error.
    async fn wait_or_get_last_lsn(
        timeline: &Timeline,
        request_lsn: Lsn,
        not_modified_since: Lsn,
        latest_gc_cutoff_lsn: &RcuReadGuard<Lsn>,
        ctx: &RequestContext,
    ) -> Result<Lsn, PageStreamError> {
        let last_record_lsn = timeline.get_last_record_lsn();

        // Sanity check the request
        if request_lsn < not_modified_since {
            return Err(PageStreamError::BadRequest(
                format!(
                    "invalid request with request LSN {} and not_modified_since {}",
                    request_lsn, not_modified_since,
                )
                .into(),
            ));
        }

        if request_lsn < **latest_gc_cutoff_lsn {
            // Check explicitly for INVALID just to get a less scary error message if the
            // request is obviously bogus
            return Err(if request_lsn == Lsn::INVALID {
                PageStreamError::BadRequest("invalid LSN(0) in request".into())
            } else {
                PageStreamError::BadRequest(format!(
                        "tried to request a page version that was garbage collected. requested at {} gc cutoff {}",
                        request_lsn, **latest_gc_cutoff_lsn
                    ).into())
            });
        }

        // Wait for WAL up to 'not_modified_since' to arrive, if necessary
        if not_modified_since > last_record_lsn {
            timeline
                .wait_lsn(
                    not_modified_since,
                    crate::tenant::timeline::WaitLsnWaiter::PageService,
                    ctx,
                )
                .await?;
            // Since we waited for 'not_modified_since' to arrive, that is now the last
            // record LSN. (Or close enough for our purposes; the last-record LSN can
            // advance immediately after we return anyway)
            Ok(not_modified_since)
        } else {
            // It might be better to use max(not_modified_since, latest_gc_cutoff_lsn)
            // here instead. That would give the same result, since we know that there
            // haven't been any modifications since 'not_modified_since'. Using an older
            // LSN might be faster, because that could allow skipping recent layers when
            // finding the page. However, we have historically used 'last_record_lsn', so
            // stick to that for now.
            Ok(std::cmp::min(last_record_lsn, request_lsn))
        }
    }

    #[instrument(skip_all, fields(shard_id, %lsn))]
    async fn handle_make_lsn_lease<IO>(
        &self,
        pgb: &mut PostgresBackend<IO>,
        tenant_shard_id: TenantShardId,
        timeline_id: TimelineId,
        lsn: Lsn,
        ctx: &RequestContext,
    ) -> Result<(), QueryError>
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        let shard_selector = ShardSelector::Known(tenant_shard_id.to_index());
        let timeline = self
            .get_active_tenant_timeline(tenant_shard_id.tenant_id, timeline_id, shard_selector)
            .await?;
        let lease = timeline.make_lsn_lease(lsn, timeline.get_lsn_lease_length(), ctx)?;
        let valid_until = lease
            .valid_until
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|e| QueryError::Other(e.into()))?;

        pgb.write_message_noflush(&BeMessage::RowDescription(&[RowDescriptor::text_col(
            b"valid_until",
        )]))?
        .write_message_noflush(&BeMessage::DataRow(&[Some(
            &valid_until.as_millis().to_be_bytes(),
        )]))?
        .write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;

        Ok(())
    }

    #[instrument(skip_all, fields(shard_id))]
    async fn handle_get_rel_exists_request(
        &mut self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        req: &PagestreamExistsRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let timeline = self.get_timeline_shard_zero(tenant_id, timeline_id).await?;
        let _timer = timeline
            .query_metrics
            .start_timer(metrics::SmgrQueryType::GetRelExists, ctx);

        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        let lsn = Self::wait_or_get_last_lsn(
            timeline,
            req.request_lsn,
            req.not_modified_since,
            &latest_gc_cutoff_lsn,
            ctx,
        )
        .await?;

        let exists = timeline
            .get_rel_exists(req.rel, Version::Lsn(lsn), ctx)
            .await?;

        Ok(PagestreamBeMessage::Exists(PagestreamExistsResponse {
            exists,
        }))
    }

    #[instrument(skip_all, fields(shard_id))]
    async fn handle_get_nblocks_request(
        &mut self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        req: &PagestreamNblocksRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let timeline = self.get_timeline_shard_zero(tenant_id, timeline_id).await?;

        let _timer = timeline
            .query_metrics
            .start_timer(metrics::SmgrQueryType::GetRelSize, ctx);

        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        let lsn = Self::wait_or_get_last_lsn(
            timeline,
            req.request_lsn,
            req.not_modified_since,
            &latest_gc_cutoff_lsn,
            ctx,
        )
        .await?;

        let n_blocks = timeline
            .get_rel_size(req.rel, Version::Lsn(lsn), ctx)
            .await?;

        Ok(PagestreamBeMessage::Nblocks(PagestreamNblocksResponse {
            n_blocks,
        }))
    }

    #[instrument(skip_all, fields(shard_id))]
    async fn handle_db_size_request(
        &mut self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        req: &PagestreamDbSizeRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let timeline = self.get_timeline_shard_zero(tenant_id, timeline_id).await?;

        let _timer = timeline
            .query_metrics
            .start_timer(metrics::SmgrQueryType::GetDbSize, ctx);

        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        let lsn = Self::wait_or_get_last_lsn(
            timeline,
            req.request_lsn,
            req.not_modified_since,
            &latest_gc_cutoff_lsn,
            ctx,
        )
        .await?;

        let total_blocks = timeline
            .get_db_size(DEFAULTTABLESPACE_OID, req.dbnode, Version::Lsn(lsn), ctx)
            .await?;
        let db_size = total_blocks as i64 * BLCKSZ as i64;

        Ok(PagestreamBeMessage::DbSize(PagestreamDbSizeResponse {
            db_size,
        }))
    }

    /// For most getpage requests, we will already have a Timeline to serve the request: this function
    /// looks up such a Timeline synchronously and without touching any global state.
    fn get_cached_timeline_for_page(
        &mut self,
        req: &PagestreamGetPageRequest,
    ) -> Result<&Arc<Timeline>, Key> {
        let key = if let Some((first_idx, first_timeline)) = self.shard_timelines.iter().next() {
            // Fastest path: single sharded case
            if first_idx.shard_count.count() == 1 {
                return Ok(&first_timeline.timeline);
            }

            let key = rel_block_to_key(req.rel, req.blkno);
            let shard_num = first_timeline
                .timeline
                .get_shard_identity()
                .get_shard_number(&key);

            // Fast path: matched the first timeline in our local handler map.  This case is common if
            // only one shard per tenant is attached to this pageserver.
            if first_timeline.timeline.get_shard_identity().number == shard_num {
                return Ok(&first_timeline.timeline);
            }

            let shard_index = ShardIndex {
                shard_number: shard_num,
                shard_count: first_timeline.timeline.get_shard_identity().count,
            };

            // Fast-ish path: timeline is in the connection handler's local cache
            if let Some(found) = self.shard_timelines.get(&shard_index) {
                return Ok(&found.timeline);
            }

            key
        } else {
            rel_block_to_key(req.rel, req.blkno)
        };

        Err(key)
    }

    /// Having looked up the [`Timeline`] instance for a particular shard, cache it to enable
    /// use in future requests without having to traverse [`crate::tenant::mgr::TenantManager`]
    /// again.
    ///
    /// Note that all the Timelines in this cache are for the same timeline_id: they're differ
    /// in which shard they belong to.  When we serve a getpage@lsn request, we choose a shard
    /// based on key.
    ///
    /// The typical size of this cache is 1, as we generally create shards to distribute work
    /// across pageservers, so don't tend to have multiple shards for the same tenant on the
    /// same pageserver.
    fn cache_timeline(
        &mut self,
        timeline: Arc<Timeline>,
    ) -> Result<&Arc<Timeline>, GetActiveTimelineError> {
        let gate_guard = timeline
            .gate
            .enter()
            .map_err(|_| GetActiveTimelineError::Tenant(GetActiveTenantError::Cancelled))?;

        let shard_index = timeline.tenant_shard_id.to_index();
        let entry = self
            .shard_timelines
            .entry(shard_index)
            .or_insert(HandlerTimeline {
                timeline,
                _guard: gate_guard,
            });

        Ok(&entry.timeline)
    }

    /// If [`Self::get_cached_timeline_for_page`] missed, then this function is used to populate the cache with
    /// a Timeline to serve requests for this key, if such a Timeline is present on this pageserver.  If no such
    /// Timeline is found, then we will return an error (this indicates that the client is talking to the wrong node).
    async fn load_timeline_for_page(
        &mut self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        key: Key,
    ) -> anyhow::Result<&Arc<Timeline>, GetActiveTimelineError> {
        // Slow path: we must call out to the TenantManager to find the timeline for this Key
        let timeline = self
            .get_active_tenant_timeline(tenant_id, timeline_id, ShardSelector::Page(key))
            .await?;

        self.cache_timeline(timeline)
    }

    async fn get_timeline_shard_zero(
        &mut self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
    ) -> anyhow::Result<&Arc<Timeline>, GetActiveTimelineError> {
        // This is a borrow-checker workaround: we can't return from inside of the  `if let Some` because
        // that would be an immutable-borrow-self return, whereas later in the function we will use a mutable
        // ref to salf.  So instead, we first build a bool, and then return while not borrowing self.
        let have_cached = if let Some((idx, _tl)) = self.shard_timelines.iter().next() {
            idx.shard_number == ShardNumber(0)
        } else {
            false
        };

        if have_cached {
            let entry = self.shard_timelines.iter().next().unwrap();
            Ok(&entry.1.timeline)
        } else {
            let timeline = self
                .get_active_tenant_timeline(tenant_id, timeline_id, ShardSelector::Zero)
                .await?;
            Ok(self.cache_timeline(timeline)?)
        }
    }

    #[instrument(skip_all, fields(shard_id))]
    async fn handle_get_page_at_lsn_request(
        &mut self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        req: &PagestreamGetPageRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let timeline = match self.get_cached_timeline_for_page(req) {
            Ok(tl) => {
                set_tracing_field_shard_id(tl);
                tl
            }
            Err(key) => {
                match self
                    .load_timeline_for_page(tenant_id, timeline_id, key)
                    .await
                {
                    Ok(t) => t,
                    Err(GetActiveTimelineError::Tenant(GetActiveTenantError::NotFound(_))) => {
                        // We already know this tenant exists in general, because we resolved it at
                        // start of connection.  Getting a NotFound here indicates that the shard containing
                        // the requested page is not present on this node: the client's knowledge of shard->pageserver
                        // mapping is out of date.
                        //
                        // Closing the connection by returning ``::Reconnect` has the side effect of rate-limiting above message, via
                        // client's reconnect backoff, as well as hopefully prompting the client to load its updated configuration
                        // and talk to a different pageserver.
                        return Err(PageStreamError::Reconnect(
                            "getpage@lsn request routed to wrong shard".into(),
                        ));
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };

        let _timer = timeline
            .query_metrics
            .start_timer(metrics::SmgrQueryType::GetPageAtLsn, ctx);

        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        let lsn = Self::wait_or_get_last_lsn(
            timeline,
            req.request_lsn,
            req.not_modified_since,
            &latest_gc_cutoff_lsn,
            ctx,
        )
        .await?;

        let page = timeline
            .get_rel_page_at_lsn(req.rel, req.blkno, Version::Lsn(lsn), ctx)
            .await?;

        Ok(PagestreamBeMessage::GetPage(PagestreamGetPageResponse {
            page,
        }))
    }

    #[instrument(skip_all, fields(shard_id))]
    async fn handle_get_slru_segment_request(
        &mut self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        req: &PagestreamGetSlruSegmentRequest,
        ctx: &RequestContext,
    ) -> Result<PagestreamBeMessage, PageStreamError> {
        let timeline = self.get_timeline_shard_zero(tenant_id, timeline_id).await?;

        let _timer = timeline
            .query_metrics
            .start_timer(metrics::SmgrQueryType::GetSlruSegment, ctx);

        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        let lsn = Self::wait_or_get_last_lsn(
            timeline,
            req.request_lsn,
            req.not_modified_since,
            &latest_gc_cutoff_lsn,
            ctx,
        )
        .await?;

        let kind = SlruKind::from_repr(req.kind)
            .ok_or(PageStreamError::BadRequest("invalid SLRU kind".into()))?;
        let segment = timeline.get_slru_segment(kind, req.segno, lsn, ctx).await?;

        Ok(PagestreamBeMessage::GetSlruSegment(
            PagestreamGetSlruSegmentResponse { segment },
        ))
    }

    /// Note on "fullbackup":
    /// Full basebackups should only be used for debugging purposes.
    /// Originally, it was introduced to enable breaking storage format changes,
    /// but that is not applicable anymore.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(shard_id, ?lsn, ?prev_lsn, %full_backup))]
    async fn handle_basebackup_request<IO>(
        &mut self,
        pgb: &mut PostgresBackend<IO>,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        lsn: Option<Lsn>,
        prev_lsn: Option<Lsn>,
        full_backup: bool,
        gzip: bool,
        ctx: &RequestContext,
    ) -> Result<(), QueryError>
    where
        IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        fn map_basebackup_error(err: BasebackupError) -> QueryError {
            match err {
                BasebackupError::Client(e) => QueryError::Disconnected(ConnectionError::Io(e)),
                BasebackupError::Server(e) => QueryError::Other(e),
            }
        }

        let started = std::time::Instant::now();

        // check that the timeline exists
        let timeline = self
            .get_active_tenant_timeline(tenant_id, timeline_id, ShardSelector::Zero)
            .await?;
        let latest_gc_cutoff_lsn = timeline.get_latest_gc_cutoff_lsn();
        if let Some(lsn) = lsn {
            // Backup was requested at a particular LSN. Wait for it to arrive.
            info!("waiting for {}", lsn);
            timeline
                .wait_lsn(
                    lsn,
                    crate::tenant::timeline::WaitLsnWaiter::PageService,
                    ctx,
                )
                .await?;
            timeline
                .check_lsn_is_in_scope(lsn, &latest_gc_cutoff_lsn)
                .context("invalid basebackup lsn")?;
        }

        let lsn_awaited_after = started.elapsed();

        // switch client to COPYOUT
        pgb.write_message_noflush(&BeMessage::CopyOutResponse)
            .map_err(QueryError::Disconnected)?;
        self.flush_cancellable(pgb, &timeline.cancel).await?;

        // Send a tarball of the latest layer on the timeline. Compress if not
        // fullbackup. TODO Compress in that case too (tests need to be updated)
        if full_backup {
            let mut writer = pgb.copyout_writer();
            basebackup::send_basebackup_tarball(
                &mut writer,
                &timeline,
                lsn,
                prev_lsn,
                full_backup,
                ctx,
            )
            .await
            .map_err(map_basebackup_error)?;
        } else {
            let mut writer = pgb.copyout_writer();
            if gzip {
                let mut encoder = GzipEncoder::with_quality(
                    writer,
                    // NOTE using fast compression because it's on the critical path
                    //      for compute startup. For an empty database, we get
                    //      <100KB with this method. The Level::Best compression method
                    //      gives us <20KB, but maybe we should add basebackup caching
                    //      on compute shutdown first.
                    async_compression::Level::Fastest,
                );
                basebackup::send_basebackup_tarball(
                    &mut encoder,
                    &timeline,
                    lsn,
                    prev_lsn,
                    full_backup,
                    ctx,
                )
                .await
                .map_err(map_basebackup_error)?;
                // shutdown the encoder to ensure the gzip footer is written
                encoder
                    .shutdown()
                    .await
                    .map_err(|e| QueryError::Disconnected(ConnectionError::Io(e)))?;
            } else {
                basebackup::send_basebackup_tarball(
                    &mut writer,
                    &timeline,
                    lsn,
                    prev_lsn,
                    full_backup,
                    ctx,
                )
                .await
                .map_err(map_basebackup_error)?;
            }
        }

        pgb.write_message_noflush(&BeMessage::CopyDone)
            .map_err(QueryError::Disconnected)?;
        self.flush_cancellable(pgb, &timeline.cancel).await?;

        let basebackup_after = started
            .elapsed()
            .checked_sub(lsn_awaited_after)
            .unwrap_or(Duration::ZERO);

        info!(
            lsn_await_millis = lsn_awaited_after.as_millis(),
            basebackup_millis = basebackup_after.as_millis(),
            "basebackup complete"
        );

        Ok(())
    }

    // when accessing management api supply None as an argument
    // when using to authorize tenant pass corresponding tenant id
    fn check_permission(&self, tenant_id: Option<TenantId>) -> Result<(), QueryError> {
        if self.auth.is_none() {
            // auth is set to Trust, nothing to check so just return ok
            return Ok(());
        }
        // auth is some, just checked above, when auth is some
        // then claims are always present because of checks during connection init
        // so this expect won't trigger
        let claims = self
            .claims
            .as_ref()
            .expect("claims presence already checked");
        check_permission(claims, tenant_id).map_err(|e| QueryError::Unauthorized(e.0))
    }

    /// Shorthand for getting a reference to a Timeline of an Active tenant.
    async fn get_active_tenant_timeline(
        &self,
        tenant_id: TenantId,
        timeline_id: TimelineId,
        selector: ShardSelector,
    ) -> Result<Arc<Timeline>, GetActiveTimelineError> {
        let tenant = self
            .get_active_tenant_with_timeout(tenant_id, selector, ACTIVE_TENANT_TIMEOUT)
            .await
            .map_err(GetActiveTimelineError::Tenant)?;
        let timeline = tenant.get_timeline(timeline_id, true)?;
        set_tracing_field_shard_id(&timeline);
        Ok(timeline)
    }

    /// Get a shard's [`Tenant`] in its active state, if present.  If we don't find the shard and some
    /// slots for this tenant are `InProgress` then we will wait.
    /// If we find the [`Tenant`] and it's not yet in state [`TenantState::Active`], we will wait.
    ///
    /// `timeout` is used as a total timeout for the whole wait operation.
    async fn get_active_tenant_with_timeout(
        &self,
        tenant_id: TenantId,
        shard_selector: ShardSelector,
        timeout: Duration,
    ) -> Result<Arc<Tenant>, GetActiveTenantError> {
        let wait_start = Instant::now();
        let deadline = wait_start + timeout;

        // Resolve TenantId to TenantShardId.  This is usually a quick one-shot thing, the loop is
        // for handling the rare case that the slot we're accessing is InProgress.
        let tenant_shard = loop {
            let resolved = self
                .tenant_manager
                .resolve_attached_shard(&tenant_id, shard_selector);
            match resolved {
                ShardResolveResult::Found(tenant_shard) => break tenant_shard,
                ShardResolveResult::NotFound => {
                    return Err(GetActiveTenantError::NotFound(GetTenantError::NotFound(
                        tenant_id,
                    )));
                }
                ShardResolveResult::InProgress(barrier) => {
                    // We can't authoritatively answer right now: wait for InProgress state
                    // to end, then try again
                    tokio::select! {
                        _ = self.await_connection_cancelled() => {
                            return Err(GetActiveTenantError::Cancelled)
                        },
                        _  = barrier.wait() => {
                            // The barrier completed: proceed around the loop to try looking up again
                        },
                        _ = tokio::time::sleep(deadline.duration_since(Instant::now())) => {
                            return Err(GetActiveTenantError::WaitForActiveTimeout {
                                latest_state: None,
                                wait_time: timeout,
                            });
                        }
                    }
                }
            };
        };

        tracing::debug!("Waiting for tenant to enter active state...");
        tenant_shard
            .wait_to_become_active(deadline.duration_since(Instant::now()))
            .await?;
        Ok(tenant_shard)
    }
}

#[async_trait::async_trait]
impl<IO> postgres_backend::Handler<IO> for PageServerHandler
where
    IO: AsyncRead + AsyncWrite + Send + Sync + Unpin,
{
    fn check_auth_jwt(
        &mut self,
        _pgb: &mut PostgresBackend<IO>,
        jwt_response: &[u8],
    ) -> Result<(), QueryError> {
        // this unwrap is never triggered, because check_auth_jwt only called when auth_type is NeonJWT
        // which requires auth to be present
        let data = self
            .auth
            .as_ref()
            .unwrap()
            .decode(str::from_utf8(jwt_response).context("jwt response is not UTF-8")?)
            .map_err(|e| QueryError::Unauthorized(e.0))?;

        if matches!(data.claims.scope, Scope::Tenant) && data.claims.tenant_id.is_none() {
            return Err(QueryError::Unauthorized(
                "jwt token scope is Tenant, but tenant id is missing".into(),
            ));
        }

        debug!(
            "jwt scope check succeeded for scope: {:#?} by tenant id: {:?}",
            data.claims.scope, data.claims.tenant_id,
        );

        self.claims = Some(data.claims);
        Ok(())
    }

    fn startup(
        &mut self,
        _pgb: &mut PostgresBackend<IO>,
        _sm: &FeStartupPacket,
    ) -> Result<(), QueryError> {
        fail::fail_point!("ps::connection-start::startup-packet");
        Ok(())
    }

    #[instrument(skip_all, fields(tenant_id, timeline_id))]
    async fn process_query(
        &mut self,
        pgb: &mut PostgresBackend<IO>,
        query_string: &str,
    ) -> Result<(), QueryError> {
        fail::fail_point!("simulated-bad-compute-connection", |_| {
            info!("Hit failpoint for bad connection");
            Err(QueryError::SimulatedConnectionError)
        });

        fail::fail_point!("ps::connection-start::process-query");

        let ctx = self.connection_ctx.attached_child();
        debug!("process query {query_string:?}");
        let parts = query_string.split_whitespace().collect::<Vec<_>>();
        if let Some(params) = parts.strip_prefix(&["pagestream_v2"]) {
            if params.len() != 2 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for pagestream command"
                )));
            }
            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            self.check_permission(Some(tenant_id))?;

            COMPUTE_COMMANDS_COUNTERS
                .for_command(ComputeCommandKind::PageStreamV2)
                .inc();

            self.handle_pagerequests(
                pgb,
                tenant_id,
                timeline_id,
                PagestreamProtocolVersion::V2,
                ctx,
            )
            .await?;
        } else if let Some(params) = parts.strip_prefix(&["pagestream"]) {
            if params.len() != 2 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for pagestream command"
                )));
            }
            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            self.check_permission(Some(tenant_id))?;

            COMPUTE_COMMANDS_COUNTERS
                .for_command(ComputeCommandKind::PageStream)
                .inc();

            self.handle_pagerequests(
                pgb,
                tenant_id,
                timeline_id,
                PagestreamProtocolVersion::V1,
                ctx,
            )
            .await?;
        } else if let Some(params) = parts.strip_prefix(&["basebackup"]) {
            if params.len() < 2 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for basebackup command"
                )));
            }

            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            self.check_permission(Some(tenant_id))?;

            COMPUTE_COMMANDS_COUNTERS
                .for_command(ComputeCommandKind::Basebackup)
                .inc();

            let lsn = if let Some(lsn_str) = params.get(2) {
                Some(
                    Lsn::from_str(lsn_str)
                        .with_context(|| format!("Failed to parse Lsn from {lsn_str}"))?,
                )
            } else {
                None
            };

            let gzip = match params.get(3) {
                Some(&"--gzip") => true,
                None => false,
                Some(third_param) => {
                    return Err(QueryError::Other(anyhow::anyhow!(
                        "Parameter in position 3 unknown {third_param}",
                    )))
                }
            };

            let metric_recording = metrics::BASEBACKUP_QUERY_TIME.start_recording(&ctx);
            let res = async {
                self.handle_basebackup_request(
                    pgb,
                    tenant_id,
                    timeline_id,
                    lsn,
                    None,
                    false,
                    gzip,
                    &ctx,
                )
                .await?;
                pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;
                Result::<(), QueryError>::Ok(())
            }
            .await;
            metric_recording.observe(&res);
            res?;
        }
        // same as basebackup, but result includes relational data as well
        else if let Some(params) = parts.strip_prefix(&["fullbackup"]) {
            if params.len() < 2 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number for fullbackup command"
                )));
            }

            let tenant_id = TenantId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_id))
                .record("timeline_id", field::display(timeline_id));

            // The caller is responsible for providing correct lsn and prev_lsn.
            let lsn = if let Some(lsn_str) = params.get(2) {
                Some(
                    Lsn::from_str(lsn_str)
                        .with_context(|| format!("Failed to parse Lsn from {lsn_str}"))?,
                )
            } else {
                None
            };
            let prev_lsn = if let Some(prev_lsn_str) = params.get(3) {
                Some(
                    Lsn::from_str(prev_lsn_str)
                        .with_context(|| format!("Failed to parse Lsn from {prev_lsn_str}"))?,
                )
            } else {
                None
            };

            self.check_permission(Some(tenant_id))?;

            COMPUTE_COMMANDS_COUNTERS
                .for_command(ComputeCommandKind::Fullbackup)
                .inc();

            // Check that the timeline exists
            self.handle_basebackup_request(
                pgb,
                tenant_id,
                timeline_id,
                lsn,
                prev_lsn,
                true,
                false,
                &ctx,
            )
            .await?;
            pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;
        } else if query_string.to_ascii_lowercase().starts_with("set ") {
            // important because psycopg2 executes "SET datestyle TO 'ISO'"
            // on connect
            pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?;
        } else if query_string.starts_with("lease lsn ") {
            let params = &parts[2..];
            if params.len() != 3 {
                return Err(QueryError::Other(anyhow::anyhow!(
                    "invalid param number {} for lease lsn command",
                    params.len()
                )));
            }

            let tenant_shard_id = TenantShardId::from_str(params[0])
                .with_context(|| format!("Failed to parse tenant id from {}", params[0]))?;
            let timeline_id = TimelineId::from_str(params[1])
                .with_context(|| format!("Failed to parse timeline id from {}", params[1]))?;

            tracing::Span::current()
                .record("tenant_id", field::display(tenant_shard_id))
                .record("timeline_id", field::display(timeline_id));

            self.check_permission(Some(tenant_shard_id.tenant_id))?;

            COMPUTE_COMMANDS_COUNTERS
                .for_command(ComputeCommandKind::LeaseLsn)
                .inc();

            // The caller is responsible for providing correct lsn.
            let lsn = Lsn::from_str(params[2])
                .with_context(|| format!("Failed to parse Lsn from {}", params[2]))?;

            match self
                .handle_make_lsn_lease(pgb, tenant_shard_id, timeline_id, lsn, &ctx)
                .await
            {
                Ok(()) => pgb.write_message_noflush(&BeMessage::CommandComplete(b"SELECT 1"))?,
                Err(e) => {
                    error!("error obtaining lsn lease for {lsn}: {e:?}");
                    pgb.write_message_noflush(&BeMessage::ErrorResponse(
                        &e.to_string(),
                        Some(e.pg_error_code()),
                    ))?
                }
            };
        } else {
            return Err(QueryError::Other(anyhow::anyhow!(
                "unknown command {query_string}"
            )));
        }

        Ok(())
    }
}

impl From<GetActiveTenantError> for QueryError {
    fn from(e: GetActiveTenantError) -> Self {
        match e {
            GetActiveTenantError::WaitForActiveTimeout { .. } => QueryError::Disconnected(
                ConnectionError::Io(io::Error::new(io::ErrorKind::TimedOut, e.to_string())),
            ),
            GetActiveTenantError::Cancelled
            | GetActiveTenantError::WillNotBecomeActive(TenantState::Stopping { .. }) => {
                QueryError::Shutdown
            }
            e @ GetActiveTenantError::NotFound(_) => QueryError::NotFound(format!("{e}").into()),
            e => QueryError::Other(anyhow::anyhow!(e)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum GetActiveTimelineError {
    #[error(transparent)]
    Tenant(GetActiveTenantError),
    #[error(transparent)]
    Timeline(#[from] GetTimelineError),
}

impl From<GetActiveTimelineError> for QueryError {
    fn from(e: GetActiveTimelineError) -> Self {
        match e {
            GetActiveTimelineError::Tenant(GetActiveTenantError::Cancelled) => QueryError::Shutdown,
            GetActiveTimelineError::Tenant(e) => e.into(),
            GetActiveTimelineError::Timeline(e) => QueryError::NotFound(format!("{e}").into()),
        }
    }
}

fn set_tracing_field_shard_id(timeline: &Timeline) {
    debug_assert_current_span_has_tenant_and_timeline_id_no_shard_id();
    tracing::Span::current().record(
        "shard_id",
        tracing::field::display(timeline.tenant_shard_id.shard_slug()),
    );
    debug_assert_current_span_has_tenant_and_timeline_id();
}
