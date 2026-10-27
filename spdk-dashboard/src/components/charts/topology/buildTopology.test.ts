// The graph projection is pure — these tests pin the invariants the canvas
// relies on: layer membership, join semantics (consumer-raid slot vs
// unassembled replica), edge encodings (color=state, dash=remote,
// animation=not in sync), and the disk resolution path (node +
// provisioned_volumes). Fixtures are wire-shaped and go through the real
// transform, so a drift from the backend contract is a compile error here.
import { describe, expect, it } from 'vitest';
import { buildTopology, joinMembers, memberVisual, raidHealth } from './buildTopology';
import { memberStateStyle } from '../../ui/status';
import { transformBackendData } from '../../../hooks/useDashboardData';
import type { Disk, Volume } from '../../../hooks/useDashboardData';
import {
  makeConsumerRaid,
  makeDashboardData,
  makeDisk,
  makeReplica,
  makeSyncInfo,
  makeVolume,
} from '../../../test/fixtures';
import type { components } from '../../../api/schema';

type Schemas = components['schemas'];

function toVolume(wire: Schemas['DashboardVolume']): Volume {
  const v = transformBackendData(makeDashboardData({ volumes: [wire] })).volumes[0];
  if (!v) throw new Error('fixture produced no volume');
  return v;
}

const VOL = 'pvc-6ff1cf70-hr-e2e';

function hostingDisk(node: string, volumeIds: string[]): Disk {
  const d = transformBackendData(
    makeDashboardData({
      disks: [
        makeDisk({
          id: `${node}:0000:00:1f.0`,
          node,
          provisioned_volumes: volumeIds.map(volume_id => ({
            volume_id,
            volume_name: 'hr-e2e',
            size: 2147483648,
            provisioned_at: '2026-07-01T00:00:00Z',
            replica_type: 'replica',
            status: 'healthy',
          })),
        }),
      ],
    })
  ).disks[0];
  if (!d) throw new Error('fixture produced no disk');
  return d;
}

// Three legs, assembled on runj-aws-1: slots 0 and 1 configured, slot 2
// failed (SPDK nulls a failed base's name and uuid), and the replica that
// slot backed (runj-aws-3) marked stale in the sync record.
function threeLegDegraded(): Volume {
  return toVolume(
    makeVolume({
      replicas: 3,
      active_replicas: 2,
      state: 'Degraded',
      nodes: ['runj-aws-1', 'runj-aws-2', 'runj-aws-3'],
      replica_statuses: [
        makeReplica({ node: 'runj-aws-1' }),
        makeReplica({ node: 'runj-aws-2' }),
        makeReplica({
          node: 'runj-aws-3',
          status: 'stale',
          sync: makeSyncInfo({
            sync_state: 'stale',
            epoch_lag: 3,
            last_epoch: 'epoch-hr-e2e-1258',
            reason: 'raid leg failed or missing while raid online on runj-aws-1',
          }),
        }),
      ],
      consumer_raids: [
        makeConsumerRaid({
          num_base_bdevs: 3,
          num_base_bdevs_operational: 2,
          base_bdevs: [
            { name: 'leg-0', uuid: 'u0', is_configured: true, replica_node: 'runj-aws-1' },
            { name: 'leg-1', uuid: 'u1', is_configured: true, replica_node: 'runj-aws-2' },
            { name: null, uuid: null, is_configured: false, replica_node: null },
          ],
        }),
      ],
    })
  );
}

describe('joinMembers', () => {
  it('joins consumer-raid bases with replicas by node and resolves disks via provisioned_volumes', () => {
    const volume = threeLegDegraded();
    const disks = [hostingDisk('runj-aws-1', [VOL]), hostingDisk('runj-aws-2', [VOL, 'pvc-other'])];
    const joins = joinMembers(volume, disks);

    // 3 raid slots + the stale replica the raid no longer holds
    expect(joins).toHaveLength(4);
    expect(joins.map(j => j.slot)).toEqual([0, 1, 2, null]);
    expect(joins[0]!.replica?.node).toBe('runj-aws-1');
    expect(joins[0]!.disk?.id).toBe('runj-aws-1:0000:00:1f.0');
    expect(joins[1]!.disk?.id).toBe('runj-aws-2:0000:00:1f.0');
    // The failed slot carries no replica and no disk
    expect(joins[2]!.member?.is_configured).toBe(false);
    expect(joins[2]!.replica).toBeNull();
    expect(joins[2]!.disk).toBeNull();
    // The stale leg is appended, marked relative to the raid that exists
    expect(joins[3]!.replica?.node).toBe('runj-aws-3');
    expect(joins[3]!.member).toBeNull();
    expect(joins[3]!.raidPresent).toBe(true);
    expect(joins.every(j => j.consumerNode === 'runj-aws-1')).toBe(true);
  });

  it('does not attach a disk from the right node that does not host the volume', () => {
    const joins = joinMembers(threeLegDegraded(), [hostingDisk('runj-aws-1', ['some-other-volume'])]);
    expect(joins[0]!.disk).toBeNull();
  });

  it('builds member rows purely from replicas when no consumer has assembled the raid', () => {
    const volume = toVolume(makeVolume()); // fixture default: consumer_raids = []
    const joins = joinMembers(volume, []);
    expect(joins).toHaveLength(2);
    expect(joins.every(j => j.member === null && j.slot === null)).toBe(true);
    // No raid anywhere → these are plain replicas, never "not assembled"
    expect(joins.every(j => !j.raidPresent)).toBe(true);
    expect(joins.every(j => j.consumerNode === null)).toBe(true);
  });
});

describe('joinMembers — a nulled slot the backend could name', () => {
  it('joins the empty slot with the leg that held it, instead of a separate slot-less card', () => {
    const volume = threeLegDegraded();
    const raid = volume.consumer_raids[0]!;
    // The backend labels a nulled slot by position when the leg is known.
    raid.base_bdevs[2] = { name: null, uuid: null, is_configured: false, replica_node: 'runj-aws-3' };
    const joins = joinMembers(volume, []);

    expect(joins).toHaveLength(3);
    expect(joins[2]!.slot).toBe(2);
    expect(joins[2]!.member?.is_configured).toBe(false);
    expect(joins[2]!.replica?.node).toBe('runj-aws-3');
    // The card carries the leg's own state (stale, catching up), not "failed".
    const visual = memberVisual(joins[2]!);
    expect(visual.state).toBe('stale');
    expect(visual.recovering).toBe(true);
    expect(visual.dashed).toBe(true);
  });
});

describe('memberVisual', () => {
  it('colors a configured base online and a failed slot failed, from the shared tokens', () => {
    const [ok, , failed] = joinMembers(threeLegDegraded(), []).map(memberVisual);
    expect(ok!.hex).toBe(memberStateStyle('online').hex);
    expect(ok!.state).toBe('online');
    expect(failed!.hex).toBe(memberStateStyle('failed').hex);
    expect(failed!.recovering).toBe(false);
  });

  it('dashes legs remote to the consumer and animates a replica that is not in sync', () => {
    const joins = joinMembers(threeLegDegraded(), []);
    expect(memberVisual(joins[0]!).dashed).toBe(false); // on the consumer node
    expect(memberVisual(joins[1]!).dashed).toBe(true); // runj-aws-2, over NVMe-oF
    expect(memberVisual(joins[1]!).recovering).toBe(false);
    const stale = memberVisual(joins[3]!);
    expect(stale.state).toBe('stale');
    expect(stale.hex).toBe(memberStateStyle('stale').hex);
    expect(stale.recovering).toBe(true);
    expect(stale.dashed).toBe(true);
  });

  it('cannot call a leg remote without a consumer to be remote from', () => {
    const joins = joinMembers(toVolume(makeVolume()), []);
    expect(joins.every(j => !memberVisual(j).dashed)).toBe(true);
  });
});

describe('raidHealth', () => {
  it('reads SPDK state plus the configured-base count', () => {
    expect(raidHealth(makeConsumerRaid())).toBe('healthy');
    expect(
      raidHealth(makeConsumerRaid({ num_base_bdevs: 3, num_base_bdevs_operational: 2 }))
    ).toBe('degraded');
    expect(raidHealth(makeConsumerRaid({ state: 'configuring' }))).toBe('degraded');
    expect(raidHealth(makeConsumerRaid({ state: 'offline' }))).toBe('failed');
  });
});

describe('buildTopology', () => {
  it('lays out the data path: consumer → raid → members → disks', () => {
    const { nodes, edges } = buildTopology(threeLegDegraded(), [hostingDisk('runj-aws-1', [VOL])]);

    const ids = nodes.map(n => n.id);
    expect(ids).toContain('consumer');
    expect(ids).toContain('raid');
    expect(ids).toEqual(
      expect.arrayContaining(['member-0', 'member-1', 'member-2', 'replica-runj-aws-3'])
    );
    expect(ids).toContain('disk-runj-aws-1:0000:00:1f.0');
    expect(ids).not.toContain('access');

    expect(edges.map(e => e.id)).toEqual(
      expect.arrayContaining(['e-consumer-raid', 'e-raid-member-0', 'e-raid-replica-runj-aws-3'])
    );
    // Deterministic left→right layering
    const x = (id: string) => nodes.find(n => n.id === id)!.position.x;
    expect(x('consumer')).toBeLessThan(x('raid'));
    expect(x('raid')).toBeLessThan(x('member-0'));
    expect(x('member-0')).toBeLessThan(x('disk-runj-aws-1:0000:00:1f.0'));
  });

  it('encodes member edges: solid local, dashed NVMe-oF remote, animated while not in sync', () => {
    const { edges } = buildTopology(threeLegDegraded(), []);
    const local = edges.find(e => e.id === 'e-raid-member-0')!;
    const remote = edges.find(e => e.id === 'e-raid-member-1')!;
    const stale = edges.find(e => e.id === 'e-raid-replica-runj-aws-3')!;

    expect(local.style?.strokeDasharray).toBeUndefined();
    expect(local.label).toBe('local');
    expect(local.animated).toBeFalsy();
    expect(remote.style?.strokeDasharray).toBeDefined();
    expect(remote.label).toBe('nvme-of');
    expect(remote.animated).toBeFalsy();
    expect(stale.animated).toBe(true);
  });

  it('never draws a rebuild edge — flint runs no blind raid rebuild', () => {
    const { edges } = buildTopology(threeLegDegraded(), []);
    expect(edges.some(e => e.id === 'e-rebuild')).toBe(false);
  });

  it('renders the consumer and raid nodes without a raid when nothing stages the volume', () => {
    const { nodes, edges } = buildTopology(toVolume(makeVolume()), []);
    const consumer = nodes.find(n => n.id === 'consumer')!;
    expect((consumer.data as { raid: unknown }).raid).toBeNull();
    expect(nodes.filter(n => n.type === 'member')).toHaveLength(2);
    // No consumer → no path label on the leg edges
    expect(edges.filter(e => e.source === 'raid').every(e => e.label === undefined)).toBe(true);
  });

  it('deduplicates a disk shared by two replicas on the same node', () => {
    const volume = toVolume(
      makeVolume({
        replica_statuses: [
          makeReplica({ node: 'runj-aws-1' }),
          makeReplica({ node: 'runj-aws-1' }),
        ],
        consumer_raids: [
          makeConsumerRaid({
            base_bdevs: [
              { name: 'leg-0', uuid: 'u0', is_configured: true, replica_node: 'runj-aws-1' },
              { name: 'leg-1', uuid: 'u1', is_configured: true, replica_node: 'runj-aws-1' },
            ],
          }),
        ],
      })
    );
    const { nodes, edges } = buildTopology(volume, [hostingDisk('runj-aws-1', [VOL])]);
    expect(nodes.filter(n => n.type === 'disk')).toHaveLength(1);
    expect(edges.filter(e => e.target === 'disk-runj-aws-1:0000:00:1f.0')).toHaveLength(2);
  });
});
