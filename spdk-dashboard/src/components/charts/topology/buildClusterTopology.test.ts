// Cluster projection invariants: node collection unions every data source,
// links pair nodes sharing volume replicas (worst-state coloring, recovery
// animation, densest-first with an explicit — never silent — cap), and the
// grid layout is deterministic.
import { describe, expect, it } from 'vitest';
import {
  buildClusterTopology,
  collectClusterNodes,
  collectNodeLinks,
  MAX_LINK_EDGES,
} from './buildClusterTopology';
import { memberStateStyle } from '../../ui/status';
import type { Disk, ReplicaStatus, Volume } from '../../../hooks/useDashboardData';

function mkReplica(node: string, overrides: Partial<ReplicaStatus> = {}): ReplicaStatus {
  return {
    access_method: 'nvmeof',
    is_local: false,
    node,
    raid_member_state: 'online',
    rebuild_progress: null,
    status: 'healthy',
    ...overrides,
  } as ReplicaStatus;
}

function mkVolume(id: string, nodes: string[], overrides: Partial<Volume> = {}): Volume {
  return {
    access_method: 'nvmeof',
    active_replicas: nodes.length,
    consumer_raids: [],
    id,
    local_nvme: false,
    name: id,
    nodes: [],
    nvmeof_enabled: true,
    nvmeof_targets: [],
    replicas: nodes.length,
    size: '2 GiB',
    spdk_validation_status: { has_spdk_backing: true, validation_severity: 'info' },
    state: 'Healthy',
    target_port: 4420,
    transport_type: 'tcp',
    ublk_device: null,
    raid_status: null,
    replica_statuses: nodes.map(n => mkReplica(n)),
    ...overrides,
  } as Volume;
}

function mkDisk(node: string, overrides: Partial<Disk> = {}): Disk {
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
    provisioned_volumes: [],
    read_iops: 1200,
    read_latency: 90,
    write_iops: 800,
    write_latency: 120,
    ...overrides,
  } as Disk;
}

describe('collectClusterNodes', () => {
  it('unions node names from the node list, disks, and replica placements', () => {
    const nodes = collectClusterNodes(
      [mkVolume('v1', ['w1', 'w2'])],
      [mkDisk('w3')],
      ['cp1'] // control plane: no disks, no replicas
    );
    expect(nodes.map(n => n.name)).toEqual(['cp1', 'w1', 'w2', 'w3']);
    expect(nodes.find(n => n.name === 'cp1')?.disks).toHaveLength(0);
    expect(nodes.find(n => n.name === 'w3')?.disks).toHaveLength(1);
  });

  it('counts replicas and volumes per node', () => {
    const nodes = collectClusterNodes(
      [mkVolume('v1', ['w1', 'w2']), mkVolume('v2', ['w1'])],
      [],
      []
    );
    const w1 = nodes.find(n => n.name === 'w1')!;
    expect(w1.replicaCount).toBe(2);
    expect(w1.volumes.map(v => v.id)).toEqual(['v1', 'v2']);
  });
});

describe('collectNodeLinks', () => {
  it('pairs nodes sharing volumes, densest first, with worst-state coloring', () => {
    const links = collectNodeLinks([
      mkVolume('v1', ['w1', 'w2']),
      mkVolume('v2', ['w1', 'w2'], { state: 'Degraded' }),
      mkVolume('v3', ['w2', 'w3']),
    ]);
    expect(links).toHaveLength(2);
    expect(links[0]).toMatchObject({ a: 'w1', b: 'w2', worstState: 'Degraded' });
    expect(links[0]!.volumes.map(v => v.id)).toEqual(['v1', 'v2']);
    expect(links[1]).toMatchObject({ a: 'w2', b: 'w3', worstState: 'Healthy' });
  });

  it('spans all pairs for 3-replica volumes and flags recovery', () => {
    const links = collectNodeLinks([
      mkVolume('v1', ['w1', 'w2', 'w3'], {
        replica_statuses: [
          mkReplica('w1'),
          mkReplica('w2', { sync: { sync_state: 'stale', epoch_lag: 2 } }),
          mkReplica('w3'),
        ],
      }),
    ]);
    expect(links.map(l => `${l.a}|${l.b}`).sort()).toEqual(['w1|w2', 'w1|w3', 'w2|w3']);
    expect(links.every(l => l.recovering)).toBe(true);
  });

  it('produces no link for single-node volumes', () => {
    expect(collectNodeLinks([mkVolume('v1', ['w1'])])).toHaveLength(0);
  });
});

describe('buildClusterTopology', () => {
  it('lays node cards on a deterministic grid with disk-state ring segments', () => {
    const { nodes } = buildClusterTopology(
      [],
      [mkDisk('w1'), mkDisk('w1', { id: 'w1-nvme1', healthy: false }), mkDisk('w2', { blobstore_initialized: false })],
      ['w1', 'w2']
    );
    const w1 = nodes.find(n => n.id === 'node-w1')!;
    const w2 = nodes.find(n => n.id === 'node-w2')!;
    expect(w1.data.ringHexes).toEqual(['#059669', '#dc2626']);
    expect(w2.data.ringHexes).toEqual(['#9ca3af']);
    // Sorted by name → stable positions across refreshes
    expect(w1.position.x).toBeLessThan(w2.position.x);
  });

  it('colors link edges by worst shared volume state and animates recovery', () => {
    const { edges } = buildClusterTopology(
      [
        mkVolume('v1', ['w1', 'w2'], { state: 'Failed' }),
        mkVolume('v2', ['w2', 'w3'], {
          replica_statuses: [
            mkReplica('w2'),
            mkReplica('w3', { sync: { sync_state: 'standby', epoch_lag: 1 } }),
          ],
        }),
      ],
      [],
      []
    );
    const failedLink = edges.find(e => e.id === 'link-w1-w2')!;
    expect(failedLink.style?.stroke).toBe(memberStateStyle('failed').hex);
    expect(failedLink.animated).toBe(false);
    const recoveringLink = edges.find(e => e.id === 'link-w2-w3')!;
    expect(recoveringLink.style?.stroke).toBe(memberStateStyle('online').hex);
    expect(recoveringLink.animated).toBe(true);
    expect(recoveringLink.label).toBe('1 vol');
  });

  it('caps link edges at MAX_LINK_EDGES keeping the densest, and reports the cut', () => {
    // 40 nodes fully meshed pairwise via 2-replica volumes → 780 pairs
    const volumes: Volume[] = [];
    let seq = 0;
    for (let i = 0; i < 40; i++) {
      for (let j = i + 1; j < 40; j++) {
        volumes.push(mkVolume(`v${seq++}`, [`n${String(i).padStart(2, '0')}`, `n${String(j).padStart(2, '0')}`]));
      }
    }
    // One pair shares a second volume — it must survive the cap at rank 1
    volumes.push(mkVolume('extra', ['n00', 'n01']));

    const { edges, truncatedLinks } = buildClusterTopology(volumes, [], []);
    expect(edges).toHaveLength(MAX_LINK_EDGES);
    expect(truncatedLinks).toBe(780 - MAX_LINK_EDGES);
    expect(edges[0]!.id).toBe('link-n00-n01');
  });
});
