// The graph projection is pure — these tests pin the invariants the canvas
// relies on: layer membership, join semantics (slot vs unassembled), edge
// encodings (color=state, dash=remote, animation=recovery), and the
// disk resolution path (node + provisioned_volumes, NOT the never-sent
// disk_ref).
import { describe, expect, it } from 'vitest';
import { buildTopology, joinMembers, memberVisual } from './buildTopology';
import { memberStateStyle } from '../../ui/status';
import type { Disk, RaidStatus, ReplicaStatus, Volume } from '../../../hooks/useDashboardData';

function mkReplica(overrides: Partial<ReplicaStatus> = {}): ReplicaStatus {
  return {
    access_method: 'local_nvme',
    is_local: true,
    node: 'worker-1',
    raid_member_state: 'online',
    // Explicit null, matching the wire: the backend serializes Option<T> as
    // null, and isReplicaRecovering treats a missing field as recovering.
    rebuild_progress: null,
    status: 'online',
    ...overrides,
  } as ReplicaStatus;
}

function mkRaid(overrides: Partial<RaidStatus> = {}): RaidStatus {
  return {
    auto_rebuild_enabled: true,
    discovered_members: 3,
    members: [
      { name: 'bdev-a', slot: 0, state: 'online' },
      { name: 'bdev-b', slot: 1, state: 'online' },
      { name: 'bdev-c', slot: 2, state: 'online' },
    ],
    num_members: 3,
    operational_members: 3,
    raid_level: 1,
    state: 'online',
    ...overrides,
  } as RaidStatus;
}

function mkVolume(overrides: Partial<Volume> = {}): Volume {
  return {
    access_method: 'ublk',
    active_replicas: 3,
    consumer_raids: [],
    id: 'vol-1',
    local_nvme: true,
    name: 'pvc-demo',
    nodes: ['worker-1'],
    nvmeof_enabled: false,
    nvmeof_targets: [],
    replicas: 3,
    size: '10 GiB',
    spdk_validation_status: { has_spdk_backing: true, validation_severity: 'info' },
    state: 'Healthy',
    target_port: 4420,
    transport_type: 'tcp',
    ublk_device: { id: 42, device_path: '/dev/ublkb42' },
    raid_status: mkRaid(),
    replica_statuses: [
      mkReplica({ node: 'worker-1', raid_member_slot: 0, is_local: true }),
      mkReplica({
        node: 'worker-2',
        raid_member_slot: 1,
        is_local: false,
        access_method: 'nvmeof',
        nvmf_target: {
          nqn: 'nqn.2024-01.io.flint:vol-1:w2',
          target_ip: '10.0.0.2',
          target_port: '4420',
          transport_type: 'TCP',
        },
      }),
      mkReplica({
        node: 'worker-3',
        raid_member_slot: 2,
        is_local: false,
        access_method: 'nvmeof',
      }),
    ],
    ...overrides,
  } as Volume;
}

function mkDisk(node: string, volumeIds: string[], overrides: Partial<Disk> = {}): Disk {
  return {
    allocated_space: 4,
    blobstore_initialized: true,
    brought_online: '2026-07-01T00:00:00Z',
    capacity: 1_000_000_000_000,
    capacity_gb: 931,
    device_type: 'nvme',
    free_space: 900,
    free_space_display: '900 GB',
    healthy: true,
    id: `${node}-nvme0`,
    is_system_disk: false,
    lvol_count: 3,
    model: 'INTEL SSDPE2KX010T8',
    node,
    orphaned_spdk_volumes: [],
    pci_addr: '0000:00:1e.0',
    provisioned_volumes: volumeIds.map(volume_id => ({
      provisioned_at: '2026-07-01T00:00:00Z',
      replica_type: 'raid_member',
      size: 10,
      status: 'active',
      volume_id,
      volume_name: 'pvc-demo',
    })),
    read_iops: 1200,
    read_latency: 90,
    write_iops: 800,
    write_latency: 120,
    ...overrides,
  } as Disk;
}

describe('joinMembers', () => {
  it('joins RAID slots with replicas and resolves backing disks via provisioned_volumes', () => {
    const volume = mkVolume();
    const disks = [mkDisk('worker-1', ['vol-1']), mkDisk('worker-2', ['vol-1', 'vol-9'])];
    const joins = joinMembers(volume, disks);

    expect(joins).toHaveLength(3);
    expect(joins.map(j => j.slot)).toEqual([0, 1, 2]);
    expect(joins[0]!.replica?.node).toBe('worker-1');
    expect(joins[0]!.disk?.id).toBe('worker-1-nvme0');
    expect(joins[1]!.disk?.id).toBe('worker-2-nvme0');
    // worker-3 has no disk hosting this volume
    expect(joins[2]!.disk).toBeNull();
  });

  it('does not attach a disk from the right node that does not host the volume', () => {
    const volume = mkVolume();
    const joins = joinMembers(volume, [mkDisk('worker-1', ['some-other-volume'])]);
    expect(joins[0]!.disk).toBeNull();
  });

  it('appends unassembled replicas (no matching RAID slot) after the slots', () => {
    const volume = mkVolume({
      replica_statuses: [
        mkReplica({ node: 'worker-1', raid_member_slot: 0 }),
        mkReplica({
          node: 'worker-4',
          raid_member_slot: null,
          status: 'standby',
          is_local: false,
          sync: { sync_state: 'standby', epoch_lag: 2 },
        }),
      ],
    });
    const joins = joinMembers(volume, []);
    const standby = joins[joins.length - 1]!;
    expect(standby.slot).toBeNull();
    expect(standby.member).toBeNull();
    expect(standby.replica?.node).toBe('worker-4');
    // A RAID exists, so this replica really is "not assembled"
    expect(standby.raidPresent).toBe(true);
  });

  it('builds member rows purely from replicas when raid_status is absent', () => {
    const volume = mkVolume({ raid_status: null });
    const joins = joinMembers(volume, []);
    expect(joins).toHaveLength(3);
    expect(joins.every(j => j.member === null)).toBe(true);
    // No RAID reported → these are plain replicas, never "not assembled"
    expect(joins.every(j => !j.raidPresent)).toBe(true);
  });
});

describe('memberVisual', () => {
  it('encodes state color from the shared status tokens', () => {
    const volume = mkVolume({
      raid_status: mkRaid({
        members: [
          { name: 'bdev-a', slot: 0, state: 'online' },
          { name: 'bdev-b', slot: 1, state: 'failed' },
        ],
        num_members: 2,
        operational_members: 1,
        state: 'degraded',
      }),
    });
    const [ok, failed] = joinMembers(volume, []).slice(0, 2).map(memberVisual) as [
      ReturnType<typeof memberVisual>,
      ReturnType<typeof memberVisual>,
    ];
    expect(ok.hex).toBe(memberStateStyle('online').hex);
    expect(failed.hex).toBe(memberStateStyle('failed').hex);
    expect(failed.recovering).toBe(false);
  });

  it('dashes remote replicas and animates Tier-2 recovery', () => {
    const volume = mkVolume({
      replica_statuses: [
        mkReplica({ node: 'worker-1', raid_member_slot: 0 }),
        mkReplica({
          node: 'worker-2',
          raid_member_slot: 1,
          is_local: false,
          sync: { sync_state: 'stale', epoch_lag: 3 },
        }),
        mkReplica({ node: 'worker-3', raid_member_slot: 2, is_local: false }),
      ],
    });
    const joins = joinMembers(volume, []);
    expect(memberVisual(joins[0]!).dashed).toBe(false);
    expect(memberVisual(joins[1]!).dashed).toBe(true);
    expect(memberVisual(joins[1]!).recovering).toBe(true);
    expect(memberVisual(joins[2]!).recovering).toBe(false);
  });
});

describe('buildTopology', () => {
  it('lays out the full data path: app → access → raid → members → disks', () => {
    const { nodes, edges } = buildTopology(mkVolume(), [mkDisk('worker-1', ['vol-1'])]);

    const ids = nodes.map(n => n.id);
    expect(ids).toContain('app');
    expect(ids).toContain('access');
    expect(ids).toContain('raid');
    expect(ids).toEqual(expect.arrayContaining(['member-0', 'member-1', 'member-2']));
    expect(ids).toContain('disk-worker-1-nvme0');

    expect(edges.map(e => e.id)).toEqual(
      expect.arrayContaining(['e-app-access', 'e-access-raid', 'e-raid-member-0'])
    );
    // Deterministic left→right layering
    const x = (id: string) => nodes.find(n => n.id === id)!.position.x;
    expect(x('app')).toBeLessThan(x('access'));
    expect(x('access')).toBeLessThan(x('raid'));
    expect(x('raid')).toBeLessThan(x('member-0'));
    expect(x('member-0')).toBeLessThan(x('disk-worker-1-nvme0'));
  });

  it('encodes member edges: solid local, dashed remote with transport label', () => {
    const { edges } = buildTopology(mkVolume(), []);
    const local = edges.find(e => e.id === 'e-raid-member-0')!;
    const remote = edges.find(e => e.id === 'e-raid-member-1')!;

    expect(local.style?.strokeDasharray).toBeUndefined();
    expect(local.label).toBe('local');
    expect(remote.style?.strokeDasharray).toBeDefined();
    expect(remote.label).toBe('nvme-of/tcp');
    expect(local.animated).toBeFalsy();
  });

  it('adds an animated rebuild edge from source slot to target slot', () => {
    const volume = mkVolume({
      raid_status: mkRaid({
        members: [
          { name: 'bdev-a', slot: 0, state: 'online' },
          { name: 'bdev-b', slot: 1, state: 'online' },
          { name: 'bdev-c', slot: 2, state: 'rebuilding' },
        ],
        operational_members: 2,
        state: 'degraded',
        rebuild_info: {
          blocks_remaining: 500,
          blocks_total: 1000,
          progress_percentage: 50,
          source_slot: 0,
          target_slot: 2,
          state: 'rebuilding',
        },
      }),
    });
    const { nodes, edges } = buildTopology(volume, []);

    const rebuild = edges.find(e => e.id === 'e-rebuild')!;
    expect(rebuild.source).toBe('member-0');
    expect(rebuild.target).toBe('member-2');
    expect(rebuild.animated).toBe(true);
    expect(rebuild.label).toBe('rebuild 50.0%');

    // The target member's own edge animates too, and its node carries the pct
    expect(edges.find(e => e.id === 'e-raid-member-2')!.animated).toBe(true);
    const target = nodes.find(n => n.id === 'member-2')!;
    expect((target.data as { rebuildPct: number | null }).rebuildPct).toBe(50);
  });

  it('skips the access layer when there is no ublk device and no NVMe-oF target', () => {
    const volume = mkVolume({ ublk_device: null, nvmeof_targets: [] });
    const { nodes, edges } = buildTopology(volume, []);
    expect(nodes.some(n => n.id === 'access')).toBe(false);
    expect(edges.some(e => e.id === 'e-app-raid')).toBe(true);
  });

  it('renders replica-only graphs when raid_status is absent', () => {
    const volume = mkVolume({ raid_status: null });
    const { nodes, edges } = buildTopology(volume, []);
    expect(nodes.filter(n => n.type === 'member')).toHaveLength(3);
    expect(edges.some(e => e.id === 'e-rebuild')).toBe(false);
  });

  it('deduplicates a disk shared by two replicas on the same node', () => {
    const volume = mkVolume({
      replica_statuses: [
        mkReplica({ node: 'worker-1', raid_member_slot: 0 }),
        mkReplica({ node: 'worker-1', raid_member_slot: 1 }),
        mkReplica({ node: 'worker-3', raid_member_slot: 2, is_local: false }),
      ],
    });
    const { nodes, edges } = buildTopology(volume, [mkDisk('worker-1', ['vol-1'])]);
    expect(nodes.filter(n => n.type === 'disk')).toHaveLength(1);
    expect(edges.filter(e => e.target === 'disk-worker-1-nvme0')).toHaveLength(2);
  });
});
