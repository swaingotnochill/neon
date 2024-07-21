use std::sync::Arc;

use super::{layer_manager::LayerManager, FlushLayerError, Timeline};
use crate::{
    context::{DownloadBehavior, RequestContext},
    task_mgr::TaskKind,
    tenant::{
        storage_layer::{AsLayerDesc as _, DeltaLayerWriter, Layer, ResidentLayer},
        Tenant,
    },
    virtual_file::{MaybeFatalIo, VirtualFile},
};
use pageserver_api::models::detach_ancestor::AncestorDetached;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use utils::{completion, generation::Generation, http::error::ApiError, id::TimelineId, lsn::Lsn};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("no ancestors")]
    NoAncestor,
    #[error("too many ancestors")]
    TooManyAncestors,
    #[error("shutting down, please retry later")]
    ShuttingDown,
    #[error("flushing failed")]
    FlushAncestor(#[source] FlushLayerError),
    #[error("layer download failed")]
    RewrittenDeltaDownloadFailed(#[source] anyhow::Error),
    #[error("copying LSN prefix locally failed")]
    CopyDeltaPrefix(#[source] anyhow::Error),
    #[error("upload rewritten layer")]
    UploadRewritten(#[source] anyhow::Error),

    #[error("ancestor is already being detached by: {}", .0)]
    OtherTimelineDetachOngoing(TimelineId),

    #[error("remote copying layer failed")]
    CopyFailed(#[source] anyhow::Error),

    #[error("unexpected error")]
    Unexpected(#[source] anyhow::Error),

    #[error("failpoint: {}", .0)]
    Failpoint(&'static str),
}

impl From<Error> for ApiError {
    fn from(value: Error) -> Self {
        match value {
            e @ Error::NoAncestor => ApiError::Conflict(e.to_string()),
            // TODO: ApiError converts the anyhow using debug formatting ... just stop using ApiError?
            e @ Error::TooManyAncestors => ApiError::BadRequest(anyhow::anyhow!("{}", e)),
            Error::ShuttingDown => ApiError::ShuttingDown,
            Error::OtherTimelineDetachOngoing(_) => {
                ApiError::ResourceUnavailable("other timeline detach is already ongoing".into())
            }
            // All of these contain shutdown errors, in fact, it's the most common
            e @ Error::FlushAncestor(_)
            | e @ Error::RewrittenDeltaDownloadFailed(_)
            | e @ Error::CopyDeltaPrefix(_)
            | e @ Error::UploadRewritten(_)
            | e @ Error::CopyFailed(_)
            | e @ Error::Unexpected(_)
            | e @ Error::Failpoint(_) => ApiError::InternalServerError(e.into()),
        }
    }
}

impl From<crate::tenant::upload_queue::NotInitialized> for Error {
    fn from(_: crate::tenant::upload_queue::NotInitialized) -> Self {
        // treat all as shutting down signals, even though that is not entirely correct
        // (uninitialized state)
        Error::ShuttingDown
    }
}

impl From<FlushLayerError> for Error {
    fn from(value: FlushLayerError) -> Self {
        match value {
            FlushLayerError::Cancelled => Error::ShuttingDown,
            FlushLayerError::NotRunning(_) => {
                // FIXME(#6424): technically statically unreachable right now, given how we never
                // drop the sender
                Error::ShuttingDown
            }
            FlushLayerError::CreateImageLayersError(_) | FlushLayerError::Other(_) => {
                Error::FlushAncestor(value)
            }
        }
    }
}

pub(crate) enum Progress {
    Prepared(completion::Completion, PreparedTimelineDetach),
    Done(AncestorDetached),
}

pub(crate) struct PreparedTimelineDetach {
    layers: Vec<Layer>,
}

/// TODO: this should be part of PageserverConf because we cannot easily modify cplane arguments.
#[derive(Debug)]
pub(crate) struct Options {
    pub(crate) rewrite_concurrency: std::num::NonZeroUsize,
    pub(crate) copy_concurrency: std::num::NonZeroUsize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            rewrite_concurrency: std::num::NonZeroUsize::new(2).unwrap(),
            copy_concurrency: std::num::NonZeroUsize::new(100).unwrap(),
        }
    }
}

/// See [`Timeline::prepare_to_detach_from_ancestor`]
pub(super) async fn prepare(
    detached: &Arc<Timeline>,
    tenant: &Tenant,
    options: Options,
    ctx: &RequestContext,
) -> Result<Progress, Error> {
    use Error::*;

    let Some((ancestor, ancestor_lsn)) = detached
        .ancestor_timeline
        .as_ref()
        .map(|tl| (tl.clone(), detached.ancestor_lsn))
    else {
        {
            let accessor = detached.remote_client.initialized_upload_queue()?;

            // we are safe to inspect the latest uploaded, because we can only witness this after
            // restart is complete and ancestor is no more.
            let latest = accessor.latest_uploaded_index_part();
            if !latest.lineage.is_detached_from_original_ancestor() {
                return Err(NoAncestor);
            }
        }

        // detached has previously been detached; let's inspect each of the current timelines and
        // report back the timelines which have been reparented by our detach
        let mut all_direct_children = tenant
            .timelines
            .lock()
            .unwrap()
            .values()
            .filter(|tl| matches!(tl.ancestor_timeline.as_ref(), Some(ancestor) if Arc::ptr_eq(ancestor, detached)))
            .map(|tl| (tl.ancestor_lsn, tl.clone()))
            .collect::<Vec<_>>();

        let mut any_shutdown = false;

        all_direct_children.retain(
            |(_, tl)| match tl.remote_client.initialized_upload_queue() {
                Ok(accessor) => accessor
                    .latest_uploaded_index_part()
                    .lineage
                    .is_reparented(),
                Err(_shutdownalike) => {
                    // not 100% a shutdown, but let's bail early not to give inconsistent results in
                    // sharded enviroment.
                    any_shutdown = true;
                    true
                }
            },
        );

        if any_shutdown {
            // it could be one or many being deleted; have client retry
            return Err(Error::ShuttingDown);
        }

        let mut reparented = all_direct_children;
        // why this instead of hashset? there is a reason, but I've forgotten it many times.
        //
        // maybe if this was a hashset we would not be able to distinguish some race condition.
        reparented.sort_unstable_by_key(|(lsn, tl)| (*lsn, tl.timeline_id));

        return Ok(Progress::Done(AncestorDetached {
            reparented_timelines: reparented
                .into_iter()
                .map(|(_, tl)| tl.timeline_id)
                .collect(),
        }));
    };

    if !ancestor_lsn.is_valid() {
        // rare case, probably wouldn't even load
        tracing::error!("ancestor is set, but ancestor_lsn is invalid, this timeline needs fixing");
        return Err(NoAncestor);
    }

    if ancestor.ancestor_timeline.is_some() {
        // non-technical requirement; we could flatten N ancestors just as easily but we chose
        // not to, at least initially
        return Err(TooManyAncestors);
    }

    // before we acquire the gate, we must mark the ancestor as having a detach operation
    // ongoing which will block other concurrent detach operations so we don't get to ackward
    // situations where there would be two branches trying to reparent earlier branches.
    let (guard, barrier) = completion::channel();

    {
        let mut guard = tenant.ongoing_timeline_detach.lock().unwrap();
        if let Some((tl, other)) = guard.as_ref() {
            if !other.is_ready() {
                return Err(OtherTimelineDetachOngoing(*tl));
            }
        }
        *guard = Some((detached.timeline_id, barrier));
    }

    let _gate_entered = detached.gate.enter().map_err(|_| ShuttingDown)?;

    utils::pausable_failpoint!("timeline-detach-ancestor::before_starting_after_locking_pausable");

    fail::fail_point!(
        "timeline-detach-ancestor::before_starting_after_locking",
        |_| Err(Error::Failpoint(
            "timeline-detach-ancestor::before_starting_after_locking"
        ))
    );

    if ancestor_lsn >= ancestor.get_disk_consistent_lsn() {
        let span =
            tracing::info_span!("freeze_and_flush", ancestor_timeline_id=%ancestor.timeline_id);
        async {
            let started_at = std::time::Instant::now();
            let freeze_and_flush = ancestor.freeze_and_flush0();
            let mut freeze_and_flush = std::pin::pin!(freeze_and_flush);

            let res =
                tokio::time::timeout(std::time::Duration::from_secs(1), &mut freeze_and_flush)
                    .await;

            let res = match res {
                Ok(res) => res,
                Err(_elapsed) => {
                    tracing::info!("freezing and flushing ancestor is still ongoing");
                    freeze_and_flush.await
                }
            };

            res?;

            // we do not need to wait for uploads to complete but we do need `struct Layer`,
            // copying delta prefix is unsupported currently for `InMemoryLayer`.
            tracing::info!(
                elapsed_ms = started_at.elapsed().as_millis(),
                "froze and flushed the ancestor"
            );
            Ok::<_, Error>(())
        }
        .instrument(span)
        .await?;
    }

    let end_lsn = ancestor_lsn + 1;

    let (filtered_layers, straddling_branchpoint, rest_of_historic) = {
        // we do not need to start from our layers, because they can only be layers that come
        // *after* ancestor_lsn
        let layers = tokio::select! {
            guard = ancestor.layers.read() => guard,
            _ = detached.cancel.cancelled() => {
                return Err(ShuttingDown);
            }
            _ = ancestor.cancel.cancelled() => {
                return Err(ShuttingDown);
            }
        };

        // between retries, these can change if compaction or gc ran in between. this will mean
        // we have to redo work.
        partition_work(ancestor_lsn, &layers)
    };

    // TODO: layers are already sorted by something: use that to determine how much of remote
    // copies are already done.
    tracing::info!(filtered=%filtered_layers, to_rewrite = straddling_branchpoint.len(), historic=%rest_of_historic.len(), "collected layers");

    // TODO: copying and lsn prefix copying could be done at the same time with a single fsync after
    let mut new_layers: Vec<Layer> =
        Vec::with_capacity(straddling_branchpoint.len() + rest_of_historic.len());

    {
        tracing::debug!(to_rewrite = %straddling_branchpoint.len(), "copying prefix of delta layers");

        let mut tasks = tokio::task::JoinSet::new();

        let mut wrote_any = false;

        let limiter = Arc::new(tokio::sync::Semaphore::new(
            options.rewrite_concurrency.get(),
        ));

        for layer in straddling_branchpoint {
            let limiter = limiter.clone();
            let timeline = detached.clone();
            let ctx = ctx.detached_child(TaskKind::DetachAncestor, DownloadBehavior::Download);

            tasks.spawn(async move {
                let _permit = limiter.acquire().await;
                let copied =
                    upload_rewritten_layer(end_lsn, &layer, &timeline, &timeline.cancel, &ctx)
                        .await?;
                Ok(copied)
            });
        }

        while let Some(res) = tasks.join_next().await {
            match res {
                Ok(Ok(Some(copied))) => {
                    wrote_any = true;
                    tracing::info!(layer=%copied, "rewrote and uploaded");
                    new_layers.push(copied);
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => return Err(e),
                Err(je) => return Err(Unexpected(je.into())),
            }
        }

        // FIXME: the fsync should be mandatory, after both rewrites and copies
        if wrote_any {
            let timeline_dir = VirtualFile::open(
                &detached
                    .conf
                    .timeline_path(&detached.tenant_shard_id, &detached.timeline_id),
                ctx,
            )
            .await
            .fatal_err("VirtualFile::open for timeline dir fsync");
            timeline_dir
                .sync_all()
                .await
                .fatal_err("VirtualFile::sync_all timeline dir");
        }
    }

    let mut tasks = tokio::task::JoinSet::new();
    let limiter = Arc::new(tokio::sync::Semaphore::new(options.copy_concurrency.get()));

    for adopted in rest_of_historic {
        let limiter = limiter.clone();
        let timeline = detached.clone();

        tasks.spawn(
            async move {
                let _permit = limiter.acquire().await;
                let owned =
                    remote_copy(&adopted, &timeline, timeline.generation, &timeline.cancel).await?;
                tracing::info!(layer=%owned, "remote copied");
                Ok(owned)
            }
            .in_current_span(),
        );
    }

    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(Ok(owned)) => {
                new_layers.push(owned);
            }
            Ok(Err(failed)) => {
                return Err(failed);
            }
            Err(je) => return Err(Unexpected(je.into())),
        }
    }

    // TODO: fsync directory again if we hardlinked something

    let prepared = PreparedTimelineDetach { layers: new_layers };

    Ok(Progress::Prepared(guard, prepared))
}

fn partition_work(
    ancestor_lsn: Lsn,
    source_layermap: &LayerManager,
) -> (usize, Vec<Layer>, Vec<Layer>) {
    let mut straddling_branchpoint = vec![];
    let mut rest_of_historic = vec![];

    let mut later_by_lsn = 0;

    for desc in source_layermap.layer_map().iter_historic_layers() {
        // off by one chances here:
        // - start is inclusive
        // - end is exclusive
        if desc.lsn_range.start > ancestor_lsn {
            later_by_lsn += 1;
            continue;
        }

        let target = if desc.lsn_range.start <= ancestor_lsn
            && desc.lsn_range.end > ancestor_lsn
            && desc.is_delta
        {
            // TODO: image layer at Lsn optimization
            &mut straddling_branchpoint
        } else {
            &mut rest_of_historic
        };

        target.push(source_layermap.get_from_desc(&desc));
    }

    (later_by_lsn, straddling_branchpoint, rest_of_historic)
}

async fn upload_rewritten_layer(
    end_lsn: Lsn,
    layer: &Layer,
    target: &Arc<Timeline>,
    cancel: &CancellationToken,
    ctx: &RequestContext,
) -> Result<Option<Layer>, Error> {
    use Error::UploadRewritten;
    let copied = copy_lsn_prefix(end_lsn, layer, target, ctx).await?;

    let Some(copied) = copied else {
        return Ok(None);
    };

    // FIXME: better shuttingdown error
    target
        .remote_client
        .upload_layer_file(&copied, cancel)
        .await
        .map_err(UploadRewritten)?;

    Ok(Some(copied.into()))
}

async fn copy_lsn_prefix(
    end_lsn: Lsn,
    layer: &Layer,
    target_timeline: &Arc<Timeline>,
    ctx: &RequestContext,
) -> Result<Option<ResidentLayer>, Error> {
    use Error::{CopyDeltaPrefix, RewrittenDeltaDownloadFailed, ShuttingDown};

    if target_timeline.cancel.is_cancelled() {
        return Err(ShuttingDown);
    }

    tracing::debug!(%layer, %end_lsn, "copying lsn prefix");

    let mut writer = DeltaLayerWriter::new(
        target_timeline.conf,
        target_timeline.timeline_id,
        target_timeline.tenant_shard_id,
        layer.layer_desc().key_range.start,
        layer.layer_desc().lsn_range.start..end_lsn,
        ctx,
    )
    .await
    .map_err(CopyDeltaPrefix)?;

    let resident = layer
        .download_and_keep_resident()
        .await
        // likely shutdown
        .map_err(RewrittenDeltaDownloadFailed)?;

    let records = resident
        .copy_delta_prefix(&mut writer, end_lsn, ctx)
        .await
        .map_err(CopyDeltaPrefix)?;

    drop(resident);

    tracing::debug!(%layer, records, "copied records");

    if records == 0 {
        drop(writer);
        // TODO: we might want to store an empty marker in remote storage for this
        // layer so that we will not needlessly walk `layer` on repeated attempts.
        Ok(None)
    } else {
        // reuse the key instead of adding more holes between layers by using the real
        // highest key in the layer.
        let reused_highest_key = layer.layer_desc().key_range.end;
        let copied = writer
            .finish(reused_highest_key, target_timeline, ctx)
            .await
            .map_err(CopyDeltaPrefix)?;

        tracing::debug!(%layer, %copied, "new layer produced");

        Ok(Some(copied))
    }
}

/// Creates a new Layer instance for the adopted layer, and ensures it is found from the remote
/// storage on successful return without the adopted layer being added to `index_part.json`.
async fn remote_copy(
    adopted: &Layer,
    adoptee: &Arc<Timeline>,
    generation: Generation,
    cancel: &CancellationToken,
) -> Result<Layer, Error> {
    use Error::CopyFailed;

    // depending if Layer::keep_resident we could hardlink

    let mut metadata = adopted.metadata();
    debug_assert!(metadata.generation <= generation);
    metadata.generation = generation;

    let owned = crate::tenant::storage_layer::Layer::for_evicted(
        adoptee.conf,
        adoptee,
        adopted.layer_desc().layer_name(),
        metadata,
    );

    // FIXME: better shuttingdown error
    adoptee
        .remote_client
        .copy_timeline_layer(adopted, &owned, cancel)
        .await
        .map(move |()| owned)
        .map_err(CopyFailed)
}

/// See [`Timeline::complete_detaching_timeline_ancestor`].
pub(super) async fn complete(
    detached: &Arc<Timeline>,
    tenant: &Tenant,
    prepared: PreparedTimelineDetach,
    _ctx: &RequestContext,
) -> Result<Vec<TimelineId>, anyhow::Error> {
    let PreparedTimelineDetach { layers } = prepared;

    let ancestor = detached
        .get_ancestor_timeline()
        .expect("must still have a ancestor");
    let ancestor_lsn = detached.get_ancestor_lsn();

    // publish the prepared layers before we reparent any of the timelines, so that on restart
    // reparented timelines find layers. also do the actual detaching.
    //
    // if we crash after this operation, we will at least come up having detached a timeline, but
    // we cannot go back and reparent the timelines which would had been reparented in normal
    // execution.
    //
    // this is not perfect, but it avoids us a retry happening after a compaction or gc on restart
    // which could give us a completely wrong layer combination.
    detached
        .remote_client
        .schedule_adding_existing_layers_to_index_detach_and_wait(
            &layers,
            (ancestor.timeline_id, ancestor_lsn),
        )
        .await?;

    let mut tasks = tokio::task::JoinSet::new();

    // because we are now keeping the slot in progress, it is unlikely that there will be any
    // timeline deletions during this time. if we raced one, then we'll just ignore it.
    tenant
        .timelines
        .lock()
        .unwrap()
        .values()
        .filter_map(|tl| {
            if Arc::ptr_eq(tl, detached) {
                return None;
            }

            if !tl.is_active() {
                return None;
            }

            let tl_ancestor = tl.ancestor_timeline.as_ref()?;
            let is_same = Arc::ptr_eq(&ancestor, tl_ancestor);
            let is_earlier = tl.get_ancestor_lsn() <= ancestor_lsn;

            let is_deleting = tl
                .delete_progress
                .try_lock()
                .map(|flow| !flow.is_not_started())
                .unwrap_or(true);

            if is_same && is_earlier && !is_deleting {
                Some(tl.clone())
            } else {
                None
            }
        })
        .for_each(|timeline| {
            // important in this scope: we are holding the Tenant::timelines lock
            let span = tracing::info_span!("reparent", reparented=%timeline.timeline_id);
            let new_parent = detached.timeline_id;

            tasks.spawn(
                async move {
                    let res = timeline
                        .remote_client
                        .schedule_reparenting_and_wait(&new_parent)
                        .await;

                    match res {
                        Ok(()) => Some(timeline),
                        Err(e) => {
                            // with the use of tenant slot, we no longer expect these.
                            tracing::warn!("reparenting failed: {e:#}");
                            None
                        }
                    }
                }
                .instrument(span),
            );
        });

    let reparenting_candidates = tasks.len();
    let mut reparented = Vec::with_capacity(tasks.len());

    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(Some(timeline)) => {
                tracing::info!(reparented=%timeline.timeline_id, "reparenting done");
                reparented.push((timeline.ancestor_lsn, timeline.timeline_id));
            }
            Ok(None) => {
                // lets just ignore this for now. one or all reparented timelines could had
                // started deletion, and that is fine.
            }
            Err(je) if je.is_cancelled() => unreachable!("not used"),
            Err(je) if je.is_panic() => {
                // ignore; it's better to continue with a single reparenting failing (or even
                // all of them) in order to get to the goal state.
                //
                // these timelines will never be reparentable, but they can be always detached as
                // separate tree roots.
            }
            Err(je) => tracing::error!("unexpected join error: {je:?}"),
        }
    }

    if reparenting_candidates != reparented.len() {
        tracing::info!("failed to reparent some candidates");
    }

    reparented.sort_unstable();

    let reparented = reparented
        .into_iter()
        .map(|(_, timeline_id)| timeline_id)
        .collect();

    Ok(reparented)
}
