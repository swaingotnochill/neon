"""

Tests in this module exercise the pageserver's behavior around generation numbers,
as defined in docs/rfcs/025-generation-numbers.md.  Briefly, the behaviors we require
of the pageserver are:
- Do not start a tenant without a generation number if control_plane_api is set
- Remote objects must be suffixed with generation
- Deletions may only be executed after validating generation
- Updates to remote_consistent_lsn may only be made visible after validating generation
"""

import enum
import os
import re
import time
from typing import Optional

import pytest
from fixtures.common_types import TenantId, TimelineId
from fixtures.log_helper import log
from fixtures.neon_fixtures import (
    NeonEnv,
    NeonEnvBuilder,
    PgBin,
    generate_uploads_and_deletions,
)
from fixtures.pageserver.common_types import parse_layer_file_name
from fixtures.pageserver.http import PageserverApiException
from fixtures.pageserver.utils import (
    assert_tenant_state,
    list_prefix,
    wait_for_last_record_lsn,
    wait_for_upload,
)
from fixtures.remote_storage import (
    RemoteStorageKind,
)
from fixtures.utils import wait_until
from fixtures.workload import Workload

# A tenant configuration that is convenient for generating uploads and deletions
# without a large amount of postgres traffic.
TENANT_CONF = {
    # small checkpointing and compaction targets to ensure we generate many upload operations
    "checkpoint_distance": f"{128 * 1024}",
    "compaction_threshold": "1",
    "compaction_target_size": f"{128 * 1024}",
    # no PITR horizon, we specify the horizon when we request on-demand GC
    "pitr_interval": "0s",
    # disable background compaction and GC. We invoke it manually when we want it to happen.
    "gc_period": "0s",
    "compaction_period": "0s",
    # create image layers eagerly, so that GC can remove some layers
    "image_creation_threshold": "1",
    "image_layer_creation_check_threshold": "0",
}


def read_all(
    env: NeonEnv, tenant_id: Optional[TenantId] = None, timeline_id: Optional[TimelineId] = None
):
    if tenant_id is None:
        tenant_id = env.initial_tenant
    assert tenant_id is not None

    if timeline_id is None:
        timeline_id = env.initial_timeline
    assert timeline_id is not None

    env.pageserver.http_client()
    with env.endpoints.create_start("main", tenant_id=tenant_id) as endpoint:
        endpoint.safe_psql("SELECT SUM(LENGTH(val)) FROM foo;")


def get_metric_or_0(ps_http, metric: str) -> int:
    v = ps_http.get_metric_value(metric)
    return 0 if v is None else int(v)


def get_deletion_queue_executed(ps_http) -> int:
    return get_metric_or_0(ps_http, "pageserver_deletion_queue_executed_total")


def get_deletion_queue_submitted(ps_http) -> int:
    return get_metric_or_0(ps_http, "pageserver_deletion_queue_submitted_total")


def get_deletion_queue_validated(ps_http) -> int:
    return get_metric_or_0(ps_http, "pageserver_deletion_queue_validated_total")


def get_deletion_queue_dropped(ps_http) -> int:
    return get_metric_or_0(ps_http, "pageserver_deletion_queue_dropped_total")


def get_deletion_queue_unexpected_errors(ps_http) -> int:
    return get_metric_or_0(ps_http, "pageserver_deletion_queue_unexpected_errors_total")


def get_deletion_queue_dropped_lsn_updates(ps_http) -> int:
    return get_metric_or_0(ps_http, "pageserver_deletion_queue_dropped_lsn_updates_total")


def get_deletion_queue_depth(ps_http) -> int:
    """
    Queue depth if at least one deletion has been submitted, else None
    """
    submitted = get_deletion_queue_submitted(ps_http)
    executed = get_deletion_queue_executed(ps_http)
    dropped = get_deletion_queue_dropped(ps_http)
    depth = submitted - executed - dropped
    log.info(f"get_deletion_queue_depth: {depth} ({submitted} - {executed} - {dropped})")

    assert depth >= 0
    return int(depth)


def assert_deletion_queue(ps_http, size_fn) -> None:
    v = get_deletion_queue_depth(ps_http)
    assert v is not None
    assert size_fn(v) is True


def test_generations_upgrade(neon_env_builder: NeonEnvBuilder):
    """
    Validate behavior when a pageserver is run without generation support enabled,
    then started again after activating it:
    - Before upgrade, no objects should have generation suffixes
    - After upgrade, the bucket should contain a mixture.
    - In both cases, postgres I/O should work.
    """
    neon_env_builder.enable_pageserver_remote_storage(
        RemoteStorageKind.MOCK_S3,
    )

    env = neon_env_builder.init_configs()
    env.broker.try_start()
    for sk in env.safekeepers:
        sk.start()
    env.storage_controller.start()

    # We will start a pageserver with no control_plane_api set, so it won't be able to self-register
    env.storage_controller.node_register(env.pageserver)

    replaced_config = env.pageserver.patch_config_toml_nonrecursive(
        {
            "control_plane_api": "",
        }
    )
    env.pageserver.start()
    env.storage_controller.node_configure(env.pageserver.id, {"availability": "Active"})

    env.neon_cli.create_tenant(
        tenant_id=env.initial_tenant, conf=TENANT_CONF, timeline_id=env.initial_timeline
    )

    generate_uploads_and_deletions(env, pageserver=env.pageserver)

    def parse_generation_suffix(key):
        m = re.match(".+-([0-9a-zA-Z]{8})$", key)
        if m is None:
            return None
        else:
            log.info(f"match: {m}")
            log.info(f"group: {m.group(1)}")
            return int(m.group(1), 16)

    assert neon_env_builder.pageserver_remote_storage is not None
    pre_upgrade_keys = list(
        [
            o["Key"]
            for o in list_prefix(neon_env_builder.pageserver_remote_storage, delimiter="")[
                "Contents"
            ]
        ]
    )
    for key in pre_upgrade_keys:
        assert parse_generation_suffix(key) is None

    env.pageserver.stop()
    # Starting without the override that disabled control_plane_api
    env.pageserver.patch_config_toml_nonrecursive(replaced_config)
    env.pageserver.start()

    generate_uploads_and_deletions(env, pageserver=env.pageserver, init=False)

    legacy_objects: list[str] = []
    suffixed_objects = []
    post_upgrade_keys = list(
        [
            o["Key"]
            for o in list_prefix(neon_env_builder.pageserver_remote_storage, delimiter="")[
                "Contents"
            ]
        ]
    )
    for key in post_upgrade_keys:
        log.info(f"post-upgrade key: {key}")
        if parse_generation_suffix(key) is not None:
            suffixed_objects.append(key)
        else:
            legacy_objects.append(key)

    # Bucket now contains a mixture of suffixed and non-suffixed objects
    assert len(suffixed_objects) > 0
    assert len(legacy_objects) > 0

    # Flush through deletions to get a clean state for scrub: we are implicitly validating
    # that our generations-enabled pageserver was able to do deletions of layers
    # from earlier which don't have a generation.
    env.pageserver.http_client().deletion_queue_flush(execute=True)

    assert get_deletion_queue_unexpected_errors(env.pageserver.http_client()) == 0

    # Having written a mixture of generation-aware and legacy index_part.json,
    # ensure the scrubber handles the situation as expected.
    metadata_summary = env.storage_scrubber.scan_metadata()
    assert metadata_summary["tenant_count"] == 1  # Scrubber should have seen our timeline
    assert metadata_summary["timeline_count"] == 1
    assert metadata_summary["timeline_shard_count"] == 1
    assert not metadata_summary["with_errors"]
    assert not metadata_summary["with_warnings"]


def test_deferred_deletion(neon_env_builder: NeonEnvBuilder):
    neon_env_builder.enable_pageserver_remote_storage(
        RemoteStorageKind.MOCK_S3,
    )
    neon_env_builder.num_pageservers = 2
    env = neon_env_builder.init_start(initial_tenant_conf=TENANT_CONF)

    attached_to_id = env.storage_controller.locate(env.initial_tenant)[0]["node_id"]
    main_pageserver = env.get_pageserver(attached_to_id)
    other_pageserver = [p for p in env.pageservers if p.id != attached_to_id][0]

    ps_http = main_pageserver.http_client()

    generate_uploads_and_deletions(env, pageserver=main_pageserver)

    # Flush: pending deletions should all complete
    assert_deletion_queue(ps_http, lambda n: n > 0)
    ps_http.deletion_queue_flush(execute=True)
    assert_deletion_queue(ps_http, lambda n: n == 0)
    assert get_deletion_queue_dropped(ps_http) == 0

    # Our visible remote_consistent_lsn should match projected
    timeline = ps_http.timeline_detail(env.initial_tenant, env.initial_timeline)
    assert timeline["remote_consistent_lsn"] == timeline["remote_consistent_lsn_visible"]
    assert get_deletion_queue_dropped_lsn_updates(ps_http) == 0

    # Now advance the generation in the control plane: subsequent validations
    # from the running pageserver will fail.  No more deletions should happen.
    env.storage_controller.attach_hook_issue(env.initial_tenant, other_pageserver.id)
    generate_uploads_and_deletions(env, init=False, pageserver=main_pageserver)

    assert_deletion_queue(ps_http, lambda n: n > 0)
    queue_depth_before = get_deletion_queue_depth(ps_http)
    executed_before = get_deletion_queue_executed(ps_http)
    ps_http.deletion_queue_flush(execute=True)

    # Queue drains to zero because we dropped deletions
    assert_deletion_queue(ps_http, lambda n: n == 0)
    # The executed counter has not incremented
    assert get_deletion_queue_executed(ps_http) == executed_before
    # The dropped counter has incremented to consume all of the deletions that were previously enqueued
    assert get_deletion_queue_dropped(ps_http) == queue_depth_before

    # Flush to S3 and see that remote_consistent_lsn does not advance: it cannot
    # because generation validation fails.
    timeline = ps_http.timeline_detail(env.initial_tenant, env.initial_timeline)
    assert timeline["remote_consistent_lsn"] != timeline["remote_consistent_lsn_visible"]
    assert get_deletion_queue_dropped_lsn_updates(ps_http) > 0

    # TODO: list bucket and confirm all objects have a generation suffix.

    assert get_deletion_queue_unexpected_errors(ps_http) == 0


class KeepAttachment(str, enum.Enum):
    KEEP = "keep"
    LOSE = "lose"


class ValidateBefore(str, enum.Enum):
    VALIDATE = "validate"
    NO_VALIDATE = "no-validate"


@pytest.mark.parametrize("keep_attachment", [KeepAttachment.KEEP, KeepAttachment.LOSE])
@pytest.mark.parametrize("validate_before", [ValidateBefore.VALIDATE, ValidateBefore.NO_VALIDATE])
def test_deletion_queue_recovery(
    neon_env_builder: NeonEnvBuilder,
    pg_bin: PgBin,
    keep_attachment: KeepAttachment,
    validate_before: ValidateBefore,
):
    """
    :param keep_attachment: whether to re-attach after restart.  Else, we act as if some other
    node took the attachment while we were restarting.
    :param validate_before: whether to wait for deletions to be validated before restart.  This
    makes them elegible to be executed after restart, if the same node keeps the attachment.
    """
    neon_env_builder.enable_pageserver_remote_storage(
        RemoteStorageKind.MOCK_S3,
    )
    neon_env_builder.num_pageservers = 2
    env = neon_env_builder.init_start(initial_tenant_conf=TENANT_CONF)

    attached_to_id = env.storage_controller.locate(env.initial_tenant)[0]["node_id"]
    main_pageserver = env.get_pageserver(attached_to_id)
    other_pageserver = [p for p in env.pageservers if p.id != attached_to_id][0]

    ps_http = main_pageserver.http_client()

    failpoints = [
        # Prevent deletion lists from being executed, to build up some backlog of deletions
        ("deletion-queue-before-execute", "return"),
    ]

    if validate_before == ValidateBefore.NO_VALIDATE:
        failpoints.append(
            # Prevent deletion lists from being validated, we will test that they are
            # dropped properly during recovery.  This is such a long sleep as to be equivalent to "never"
            ("control-plane-client-validate", "return(3600000)")
        )

    ps_http.configure_failpoints(failpoints)

    generate_uploads_and_deletions(env, pageserver=main_pageserver)

    # There should be entries in the deletion queue
    assert_deletion_queue(ps_http, lambda n: n > 0)
    ps_http.deletion_queue_flush()
    before_restart_depth = get_deletion_queue_depth(ps_http)

    assert get_deletion_queue_unexpected_errors(ps_http) == 0
    assert get_deletion_queue_dropped_lsn_updates(ps_http) == 0

    if validate_before == ValidateBefore.VALIDATE:
        # At this point, one or more DeletionLists have been written.  We have set a failpoint
        # to prevent them successfully executing, but we want to see them get validated.
        #
        # We await _some_ validations instead of _all_ validations, because our execution failpoint
        # will prevent validation proceeding for any but the first DeletionList.  Usually the workload
        # just generates one, but if it generates two due to timing, then we must not expect that the
        # second one will be validated.
        def assert_some_validations():
            assert get_deletion_queue_validated(ps_http) > 0

        wait_until(20, 1, assert_some_validations)

        # The validatated keys statistic advances before the header is written, so we
        # also wait to see the header hit the disk: this seems paranoid but the race
        # can really happen on a heavily overloaded test machine.
        def assert_header_written():
            assert (main_pageserver.workdir / "deletion" / "header-01").exists()

        wait_until(20, 1, assert_header_written)

        # If we will lose attachment, then our expectation on restart is that only the ones
        # we already validated will execute.  Act like only those were present in the queue.
        if keep_attachment == KeepAttachment.LOSE:
            before_restart_depth = get_deletion_queue_validated(ps_http)

    log.info(f"Restarting pageserver with {before_restart_depth} deletions enqueued")
    main_pageserver.stop(immediate=True)

    if keep_attachment == KeepAttachment.LOSE:
        some_other_pageserver = other_pageserver.id
        env.storage_controller.attach_hook_issue(env.initial_tenant, some_other_pageserver)

    main_pageserver.start()

    def assert_deletions_submitted(n: int) -> None:
        assert ps_http.get_metric_value("pageserver_deletion_queue_submitted_total") == n

    # After restart, issue a flush to kick the deletion frontend to do recovery.
    # It should recover all the operations we submitted before the restart.
    ps_http.deletion_queue_flush(execute=False)
    wait_until(20, 0.25, lambda: assert_deletions_submitted(before_restart_depth))

    # The queue should drain through completely if we flush it
    ps_http.deletion_queue_flush(execute=True)
    wait_until(10, 1, lambda: assert_deletion_queue(ps_http, lambda n: n == 0))

    if keep_attachment == KeepAttachment.KEEP:
        # - If we kept the attachment, then our pre-restart deletions should execute
        #   because on re-attach they were from the immediately preceding generation
        assert get_deletion_queue_executed(ps_http) == before_restart_depth
    elif validate_before == ValidateBefore.VALIDATE:
        # - If we validated before restart, then we should execute however many keys were
        #   validated before restart.
        assert get_deletion_queue_executed(ps_http) == before_restart_depth
    else:
        # If we lost the attachment, we should have dropped our pre-restart deletions.
        assert get_deletion_queue_dropped(ps_http) == before_restart_depth

    assert get_deletion_queue_unexpected_errors(ps_http) == 0
    assert get_deletion_queue_dropped_lsn_updates(ps_http) == 0

    # Restart again
    main_pageserver.stop(immediate=True)
    main_pageserver.start()

    # No deletion lists should be recovered: this demonstrates that deletion lists
    # were cleaned up after being executed or dropped in the previous process lifetime.
    time.sleep(1)
    assert_deletion_queue(ps_http, lambda n: n == 0)

    assert get_deletion_queue_unexpected_errors(ps_http) == 0
    assert get_deletion_queue_dropped_lsn_updates(ps_http) == 0


def test_emergency_mode(neon_env_builder: NeonEnvBuilder, pg_bin: PgBin):
    neon_env_builder.enable_pageserver_remote_storage(
        RemoteStorageKind.MOCK_S3,
    )
    env = neon_env_builder.init_start(initial_tenant_conf=TENANT_CONF)

    ps_http = env.pageserver.http_client()

    generate_uploads_and_deletions(env, pageserver=env.pageserver)

    env.pageserver.allowed_errors.extend(
        [
            # When the pageserver can't reach the control plane, it will complain
            ".*calling control plane generation validation API failed.*",
            # Emergency mode is a big deal, we log errors whenever it is used.
            ".*Emergency mode!.*",
        ]
    )

    # Simulate a major incident: the control plane goes offline
    env.storage_controller.stop()

    # Remember how many validations had happened before the control plane went offline
    validated = get_deletion_queue_validated(ps_http)

    generate_uploads_and_deletions(env, init=False, pageserver=env.pageserver)

    # The running pageserver should stop progressing deletions
    time.sleep(10)
    assert get_deletion_queue_validated(ps_http) == validated

    # Restart the pageserver: ordinarily we would _avoid_ doing this during such an
    # incident, but it might be unavoidable: if so, we want to be able to start up
    # and serve clients.
    env.pageserver.stop()  # Non-immediate: implicitly checking that shutdown doesn't hang waiting for CP
    replaced = env.pageserver.patch_config_toml_nonrecursive(
        {
            "control_plane_emergency_mode": True,
        }
    )
    env.pageserver.start()

    # The pageserver should provide service to clients
    generate_uploads_and_deletions(env, init=False, pageserver=env.pageserver)

    # The pageserver should neither validate nor execute any deletions, it should have
    # loaded the DeletionLists from before though
    time.sleep(10)
    assert get_deletion_queue_depth(ps_http) > 0
    assert get_deletion_queue_validated(ps_http) == 0
    assert get_deletion_queue_executed(ps_http) == 0

    # When the control plane comes back up, normal service should resume
    env.storage_controller.start()

    ps_http.deletion_queue_flush(execute=True)
    assert get_deletion_queue_depth(ps_http) == 0
    assert get_deletion_queue_validated(ps_http) > 0
    assert get_deletion_queue_executed(ps_http) > 0

    # The pageserver should work fine when subsequently restarted in non-emergency mode
    env.pageserver.stop()  # Non-immediate: implicitly checking that shutdown doesn't hang waiting for CP
    env.pageserver.patch_config_toml_nonrecursive(replaced)
    env.pageserver.start()

    generate_uploads_and_deletions(env, init=False, pageserver=env.pageserver)
    ps_http.deletion_queue_flush(execute=True)
    assert get_deletion_queue_depth(ps_http) == 0
    assert get_deletion_queue_validated(ps_http) > 0
    assert get_deletion_queue_executed(ps_http) > 0


def evict_all_layers(env: NeonEnv, tenant_id: TenantId, timeline_id: TimelineId):
    client = env.pageserver.http_client()

    layer_map = client.layer_map_info(tenant_id, timeline_id)

    for layer in layer_map.historic_layers:
        if layer.remote:
            log.info(
                f"Skipping trying to evict remote layer {tenant_id}/{timeline_id} {layer.layer_file_name}"
            )
            continue
        log.info(f"Evicting layer {tenant_id}/{timeline_id} {layer.layer_file_name}")
        client.evict_layer(
            tenant_id=tenant_id, timeline_id=timeline_id, layer_name=layer.layer_file_name
        )


def test_eviction_across_generations(neon_env_builder: NeonEnvBuilder):
    """
    Eviction and on-demand downloads exercise a particular code path where RemoteLayer is constructed
    and must be constructed using the proper generation for the layer, which may not be the same generation
    that the tenant is running in.
    """
    neon_env_builder.enable_pageserver_remote_storage(
        RemoteStorageKind.MOCK_S3,
    )
    env = neon_env_builder.init_start(initial_tenant_conf=TENANT_CONF)
    env.pageserver.http_client()
    tenant_id = env.initial_tenant
    timeline_id = env.initial_timeline

    generate_uploads_and_deletions(env, pageserver=env.pageserver)

    read_all(env, tenant_id, timeline_id)
    evict_all_layers(env, tenant_id, timeline_id)
    read_all(env, tenant_id, timeline_id)

    # This will cause the generation to increment
    env.pageserver.stop()
    env.pageserver.start()

    # Now we are running as generation 2, but must still correctly remember that the layers
    # we are evicting and downloading are from generation 1.
    read_all(env, tenant_id, timeline_id)
    evict_all_layers(env, tenant_id, timeline_id)
    read_all(env, tenant_id, timeline_id)


def test_multi_attach(
    neon_env_builder: NeonEnvBuilder,
    pg_bin: PgBin,
):
    neon_env_builder.num_pageservers = 3
    neon_env_builder.enable_pageserver_remote_storage(
        remote_storage_kind=RemoteStorageKind.MOCK_S3,
    )
    env = neon_env_builder.init_start(initial_tenant_conf=TENANT_CONF)

    pageservers = env.pageservers
    http_clients = list([p.http_client() for p in pageservers])
    tenant_id = env.initial_tenant
    timeline_id = env.initial_timeline

    # Initially, the tenant will be attached to the first pageserver (first is default in our test harness)
    wait_until(10, 0.2, lambda: assert_tenant_state(http_clients[0], tenant_id, "Active"))
    _detail = http_clients[0].timeline_detail(tenant_id, timeline_id)
    with pytest.raises(PageserverApiException):
        http_clients[1].timeline_detail(tenant_id, timeline_id)
    with pytest.raises(PageserverApiException):
        http_clients[2].timeline_detail(tenant_id, timeline_id)

    workload = Workload(env, tenant_id, timeline_id)
    workload.init(pageservers[0].id)
    workload.write_rows(1000, pageservers[0].id)

    # Attach the tenant to the other two pageservers
    pageservers[1].tenant_attach(env.initial_tenant)
    pageservers[2].tenant_attach(env.initial_tenant)

    wait_until(10, 0.2, lambda: assert_tenant_state(http_clients[1], tenant_id, "Active"))
    wait_until(10, 0.2, lambda: assert_tenant_state(http_clients[2], tenant_id, "Active"))

    # Now they all have it attached
    _details = list([c.timeline_detail(tenant_id, timeline_id) for c in http_clients])
    _detail = http_clients[1].timeline_detail(tenant_id, timeline_id)
    _detail = http_clients[2].timeline_detail(tenant_id, timeline_id)

    # The endpoint can use any pageserver to service its reads
    for pageserver in pageservers:
        workload.validate(pageserver.id)

    # If we write some more data, all the nodes can see it, including stale ones
    wrote_lsn = workload.write_rows(1000, pageservers[0].id)
    for ps_http in http_clients:
        wait_for_last_record_lsn(ps_http, tenant_id, timeline_id, wrote_lsn)

    # ...and indeed endpoints can see it via any of the pageservers
    for pageserver in pageservers:
        workload.validate(pageserver.id)

    # Prompt all the pageservers, including stale ones, to upload ingested layers to remote storage
    for ps_http in http_clients:
        ps_http.timeline_checkpoint(tenant_id, timeline_id)
        wait_for_upload(ps_http, tenant_id, timeline_id, wrote_lsn)

    # Now, the contents of remote storage will be a set of layers from each pageserver, but with unique
    # generation numbers
    # TODO: validate remote storage contents

    # Stop all pageservers
    for ps in pageservers:
        ps.stop()

    # Returning to a normal healthy state: all pageservers will start, but only the one most
    # recently attached via the control plane will re-attach on startup
    for ps in pageservers:
        ps.start()

    with pytest.raises(PageserverApiException):
        _detail = http_clients[0].timeline_detail(tenant_id, timeline_id)
    with pytest.raises(PageserverApiException):
        _detail = http_clients[1].timeline_detail(tenant_id, timeline_id)
    _detail = http_clients[2].timeline_detail(tenant_id, timeline_id)

    # All data we wrote while multi-attached remains readable
    workload.validate(pageservers[2].id)


def test_upgrade_generationless_local_file_paths(
    neon_env_builder: NeonEnvBuilder,
):
    """
    Test pageserver behavior when startup up with local layer paths without
    generation numbers: it should accept these layer files, and avoid doing
    a delete/download cycle on them.
    """
    neon_env_builder.num_pageservers = 2
    env = neon_env_builder.init_configs()
    env.start()

    tenant_id = TenantId.generate()
    timeline_id = TimelineId.generate()
    env.neon_cli.create_tenant(
        tenant_id, timeline_id, conf=TENANT_CONF, placement_policy='{"Attached":1}'
    )

    workload = Workload(env, tenant_id, timeline_id)
    workload.init()
    workload.write_rows(1000)

    attached_pageserver = env.get_tenant_pageserver(tenant_id)
    secondary_pageserver = list([ps for ps in env.pageservers if ps.id != attached_pageserver.id])[
        0
    ]

    attached_pageserver.http_client().tenant_heatmap_upload(tenant_id)
    secondary_pageserver.http_client().tenant_secondary_download(tenant_id)

    # Rename the local paths to legacy format, to simulate what
    # we would see when upgrading.  Do this on both attached and secondary locations, as we will
    # test the behavior of both.
    for pageserver in env.pageservers:
        pageserver.stop()
        timeline_dir = pageserver.timeline_dir(tenant_id, timeline_id)
        files_renamed = 0
        for filename in os.listdir(timeline_dir):
            path = os.path.join(timeline_dir, filename)
            log.info(f"Found file {path}")
            if path.endswith("-v1-00000001"):
                new_path = path[:-12]
                os.rename(path, new_path)
                log.info(f"Renamed {path} -> {new_path}")
                files_renamed += 1

        assert files_renamed > 0

        pageserver.start()

    workload.validate()

    # Assert that there were no on-demand downloads
    assert (
        attached_pageserver.http_client().get_metric_value(
            "pageserver_remote_ondemand_downloaded_layers_total"
        )
        == 0
    )

    # Do a secondary download and ensure there were no layer downloads
    secondary_pageserver.http_client().tenant_secondary_download(tenant_id)
    assert (
        secondary_pageserver.http_client().get_metric_value(
            "pageserver_secondary_download_layer_total"
        )
        == 0
    )

    # Check that when we evict and promote one of the legacy-named layers, everything works as
    # expected
    local_layers = list(
        (
            parse_layer_file_name(path.name),
            os.path.join(attached_pageserver.timeline_dir(tenant_id, timeline_id), path),
        )
        for path in attached_pageserver.list_layers(tenant_id, timeline_id)
    )
    (victim_layer_name, victim_path) = local_layers[0]
    assert os.path.exists(victim_path)

    attached_pageserver.http_client().evict_layer(
        tenant_id, timeline_id, victim_layer_name.to_str()
    )
    assert not os.path.exists(victim_path)

    attached_pageserver.http_client().download_layer(
        tenant_id, timeline_id, victim_layer_name.to_str()
    )
    # We should download into the same local path we started with
    assert os.path.exists(victim_path)
