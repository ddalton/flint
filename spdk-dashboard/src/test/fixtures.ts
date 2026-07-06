// Typed test fixtures. Every builder returns the GENERATED wire type from
// api/openapi.json, so a fixture that drifts from the backend contract is a
// compile error, not a quietly-lying test. Values are modeled on real runj
// payloads (the hr-e2e drill volume and its HotRejoinSucceeded window).
import type { components } from '../api/schema';

type Schemas = components['schemas'];

export function makeSyncInfo(
  overrides: Partial<Schemas['ReplicaSyncInfo']> = {}
): Schemas['ReplicaSyncInfo'] {
  return {
    sync_state: 'in_sync',
    last_epoch: 'epoch-hr-e2e-1261',
    epoch_lag: 0,
    since: '2026-07-02T22:36:53Z',
    reason: null,
    hot_rejoin: null,
    ...overrides,
  };
}

export function makeReplica(
  overrides: Partial<Schemas['DashboardReplicaStatus']> = {}
): Schemas['DashboardReplicaStatus'] {
  return {
    node: 'runj-aws-1',
    status: 'active',
    access_method: 'nvmeof',
    is_local: false,
    raid_member_state: 'online',
    raid_member_slot: 0,
    rebuild_progress: null,
    rebuild_target: null,
    is_new_replica: false,
    last_io_timestamp: null,
    nvmf_target: null,
    sync: makeSyncInfo(),
    ...overrides,
  };
}

export function makeVolume(
  overrides: Partial<Schemas['DashboardVolume']> = {}
): Schemas['DashboardVolume'] {
  return {
    id: 'pvc-6ff1cf70-hr-e2e',
    name: 'hr-e2e',
    state: 'Healthy',
    size: '2Gi',
    replicas: 2,
    active_replicas: 2,
    access_method: 'nvmeof',
    local_nvme: false,
    nvmeof_enabled: true,
    nvmeof_targets: [],
    nodes: ['runj-aws-1', 'runj-aws-2'],
    transport_type: 'tcp',
    target_port: 4420,
    current_epoch: 'epoch-hr-e2e-1261',
    consumer_raids: [],
    replica_statuses: [
      makeReplica(),
      makeReplica({ node: 'runj-aws-2', raid_member_slot: 1 }),
    ],
    spdk_validation_status: {
      has_spdk_backing: true,
      validation_severity: 'info',
      validation_message: null,
    },
    raid_status: null,
    rebuild_progress: null,
    pvc_info: null,
    ublk_device: null,
    ...overrides,
  };
}

export function makeDisk(
  overrides: Partial<Schemas['DashboardDisk']> = {}
): Schemas['DashboardDisk'] {
  return {
    id: 'runj-aws-1:0000:00:1f.0',
    node: 'runj-aws-1',
    pci_addr: '0000:00:1f.0',
    model: 'Amazon EC2 NVMe Instance Storage',
    device_type: 'nvme',
    capacity: 118111600640,
    capacity_gb: 110,
    allocated_space: 4,
    free_space: 106,
    free_space_display: '106GB',
    healthy: true,
    blobstore_initialized: true,
    is_system_disk: false,
    lvol_count: 2,
    brought_online: '2026-06-30T10:00:00Z',
    provisioned_volumes: [],
    orphaned_spdk_volumes: [],
    read_iops: 0,
    write_iops: 0,
    read_latency: 0,
    write_latency: 0,
    ...overrides,
  };
}

export function makeDashboardData(
  overrides: Partial<Schemas['DashboardData']> = {}
): Schemas['DashboardData'] {
  return {
    volumes: [makeVolume()],
    raw_volumes: [],
    disks: [makeDisk(), makeDisk({ id: 'runj-aws-2:0000:00:1f.0', node: 'runj-aws-2' })],
    nodes: ['runj-aws-1', 'runj-aws-2', 'runj-aws-3'],
    node_info: {},
    ...overrides,
  };
}

// The verbatim shape of the 7b-4 drill window: 1730 ms, inline fenced-delta
// path, 26 MiB estimator.
export function makeWindow(
  overrides: Partial<Schemas['HotRejoinWindow']> = {}
): Schemas['HotRejoinWindow'] {
  return {
    volume: 'pvc-6ff1cf70-hr-e2e',
    node: 'runj-aws-2',
    raid: 'raid_pvc-6ff1cf70-hr-e2e',
    epoch: 'epoch-hr-e2e-1262',
    path: 'inline',
    window_ms: 1730,
    estimator_bytes: 27262976,
    steps: [
      { name: 'quiesce', ms: 102 },
      { name: 'fenced_delta_copy', ms: 1416 },
      { name: 'flip', ms: 88 },
      { name: 'unquiesce', ms: 124 },
    ],
    timestamp: '2026-07-01T23:05:12Z',
    ...overrides,
  };
}

export function makeEvent(
  overrides: Partial<Schemas['DashboardEvent']> = {}
): Schemas['DashboardEvent'] {
  return {
    category: 'hot_rejoin',
    reason: 'HotRejoinSucceeded',
    event_type: 'Normal',
    message: 'hot rejoin window 1730 ms (inline)',
    volume: 'pvc-6ff1cf70-hr-e2e',
    reporting_instance: 'flint-csi-controller',
    timestamp: '2026-07-01T23:05:12Z',
    ...overrides,
  };
}

export function makeEventsResponse(
  overrides: Partial<Schemas['EventsResponse']> = {}
): Schemas['EventsResponse'] {
  return {
    events: [makeEvent()],
    windows: [makeWindow()],
    ...overrides,
  };
}

export function makeNodeDiskStatus(
  overrides: Partial<Schemas['NodeDiskStatus']> = {}
): Schemas['NodeDiskStatus'] {
  return {
    pci_address: '0000:00:1f.0',
    device_name: 'nvme1n1',
    device_id: '0xcd00',
    vendor_id: '0x1d0f',
    subsystem_device_id: '0xcd00',
    subsystem_vendor_id: '0x1d0f',
    model: 'Amazon EC2 NVMe Instance Storage',
    serial: 'AWS62A1E921C0E5378D8',
    firmware_version: '0',
    namespace_id: 1,
    numa_node: 0,
    size_bytes: 118111600640,
    free_space: 0,
    blobstore_initialized: false,
    is_system_disk: false,
    mounted_partitions: [],
    healthy: true,
    driver: 'nvme',
    driver_ready: true,
    spdk_ready: false,
    error_count: 0,
    temperature: null,
    filesystem_type: null,
    discovered_at: '2026-07-01T00:00:00Z',
    ...overrides,
  };
}

// /api/nodes fleet rollup fixture — matches makeDashboardData's three
// nodes: one warning (out-of-sync replica), two ok, one of which has an
// uninitialized spare disk.
export function makeNodeSummary(
  overrides: Partial<Schemas['NodeSummary']> = {}
): Schemas['NodeSummary'] {
  return {
    name: 'runj-aws-1',
    disks_total: 1,
    disks_healthy: 1,
    disks_uninitialized: 0,
    volumes_total: 1,
    local_nvme_volumes: 1,
    replicas_out_of_sync: 0,
    volumes_not_healthy: 0,
    capacity_gb: 110.0,
    allocated_gb: 30.0,
    health: 'ok',
    ...overrides,
  };
}

export function makeNodesResponse(
  overrides: Partial<Schemas['NodesResponse']> = {}
): Schemas['NodesResponse'] {
  return {
    nodes: [
      makeNodeSummary(),
      makeNodeSummary({
        name: 'runj-aws-2',
        replicas_out_of_sync: 1,
        volumes_not_healthy: 1,
        health: 'warning',
      }),
      makeNodeSummary({ name: 'runj-aws-3', disks_uninitialized: 1, disks_total: 2 }),
    ],
    ...overrides,
  };
}

// Flat /api/snapshots fixture — logical snapshots with per-node copies
// merged backend-side. Pre-merge, a 2-replica snapshot appeared once per
// node (the "Total Snapshots" chip double-counted) and replica_bdev_details
// was never sent (the "Replica Snapshots" chip always read 0).
export function makeSnapshotList(): Schemas['DashboardSnapshot'][] {
  const volume = 'pvc-93edc114-bec7-43a0-8273-5812c2c52d13';
  const snap = (
    seq: number,
    uuid: string,
    nodes: string[]
  ): Schemas['DashboardSnapshot'] => {
    const name = `snap_${volume}_6836626352724501${seq}`;
    return {
      snapshot_uuid: uuid,
      snapshot_name: name,
      source_volume_id: volume,
      lvs_name: 'lvs_runk-aws-1_0000-00-1f-0',
      size_bytes: 2147483648,
      creation_time: '2026-07-05T00:00:00Z',
      ready_to_use: true,
      node: nodes[0] ?? '',
      replica_bdev_details: nodes.map(node => ({
        node,
        name,
        aliases: [name],
        driver: 'lvol',
        snapshot_source_bdev: `vol_${volume}`,
      })),
    };
  };
  return [
    snap(1, 'uuid-1', ['runk-aws-1', 'runk-aws-2']),
    snap(2, 'uuid-2', ['runk-aws-1']),
  ];
}

// Snapshot timeline fixture — the runk 2026-07-04 fixture volume shape:
// three user VolumeSnapshots (real CR times) over a six-epoch retained
// window, two in-sync replicas. Times are relative to `now` so domain
// math stays realistic no matter when the test runs.
export function makeSnapshotTimeline(
  overrides: Partial<Schemas['SnapshotTimelineResponse']> = {}
): Schemas['SnapshotTimelineResponse'] {
  const volume = 'pvc-93edc114-bec7-43a0-8273-5812c2c52d13';
  const now = Date.now();
  const at = (secsAgo: number) => new Date(now - secsAgo * 1000).toISOString();
  const epoch = (seq: number, secsAgo: number): Schemas['SnapshotTimelineEvent'] => ({
    id: `epoch-${volume}-${seq}`,
    kind: 'epoch',
    name: `epoch-${volume}-${seq}`,
    spdk_name: `epoch-${volume}-${seq}`,
    created_at: at(secsAgo),
    size_bytes: 2147483648,
    ready: true,
    nodes: ['runk-aws-1', 'runk-aws-2'],
    epoch_seq: seq,
    orphan: false,
  });
  const user = (
    n: number,
    secsAgo: number,
    extra: Partial<Schemas['SnapshotTimelineEvent']> = {}
  ): Schemas['SnapshotTimelineEvent'] => ({
    id: `snapcontent-${n}`,
    kind: 'user',
    name: `snap-demo-${n}`,
    spdk_name: `snap_${volume}_6836626352724501${n}`,
    created_at: at(secsAgo),
    size_bytes: 2147483648,
    ready: true,
    nodes: ['runk-aws-1', 'runk-aws-2'],
    vs_namespace: 'default',
    vs_name: `snap-demo-${n}`,
    vsc_name: `snapcontent-${n}`,
    orphan: false,
    ...extra,
  });
  return {
    volume_id: volume,
    now: new Date(now).toISOString(),
    current_epoch: `epoch-${volume}-9`,
    replicas: [
      { node: 'runk-aws-1', sync_state: 'in_sync', last_epoch: `epoch-${volume}-9` },
      { node: 'runk-aws-2', sync_state: 'in_sync', last_epoch: `epoch-${volume}-9` },
    ],
    events: [
      epoch(4, 310), epoch(5, 250), epoch(6, 190), epoch(7, 130), epoch(8, 70), epoch(9, 10),
      user(1, 280), user(2, 160), user(3, 45),
    ],
    untracked_epochs: 1,
    ...overrides,
  };
}
