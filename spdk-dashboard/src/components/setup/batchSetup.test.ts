// The committed form of the Phase 2d batch-engine verification (the ad-hoc
// "28-check simulation" that validated bd33b8a). The invariants that matter
// operationally: disks on one node NEVER run concurrently (the agent mutates
// shared host state), the cross-node cap holds, cancellation drains cleanly,
// and one thrown setup call cannot take down the batch.
import { describe, expect, it } from 'vitest';
import {
  type BatchDisk,
  type BatchItem,
  confirmPhrase,
  diskClassLabel,
  groupDisks,
  isBatchEligible,
  isBulkSelectable,
  isFreshCluster,
  rangeBetween,
  runInitBatch,
  TYPE_TO_CONFIRM_THRESHOLD,
  uninitializedDiskCount,
} from './batchSetup';
import { makeNodeDiskStatus } from '../../test/fixtures';
import type { UnimplementedDisk } from '../../hooks/useDashboardData';

const disk = (node: string, pci: string): BatchDisk => ({
  key: `${node}:${pci}`,
  node,
  pci,
  device: 'nvme1n1',
  model: 'test-model',
  serial: `serial-${node}-${pci}`,
  sizeBytes: 118111600640,
});

// A fleet of `disksPerNode` disks on each of `nodes` nodes.
const fleet = (nodes: number, disksPerNode: number): BatchDisk[] =>
  Array.from({ length: nodes }, (_, n) =>
    Array.from({ length: disksPerNode }, (_, d) => disk(`node-${n}`, `0000:00:${d}.0`))
  ).flat();

const nodeStatus = (overrides: Partial<UnimplementedDisk> = {}): UnimplementedDisk =>
  makeNodeDiskStatus(overrides);

describe('runInitBatch', () => {
  it('runs every disk exactly once and reports per-disk outcomes', async () => {
    const calls: string[] = [];
    const items = await runInitBatch(fleet(3, 4), {
      setupOne: async (d) => {
        calls.push(d.key);
        return d.pci === '0000:00:2.0' ? { ok: false, error: 'Disk not found' } : { ok: true };
      },
    });

    expect(calls).toHaveLength(12);
    expect(new Set(calls).size).toBe(12);
    expect(items.filter((i) => i.status === 'ok')).toHaveLength(9);
    const failed = items.filter((i) => i.status === 'failed');
    expect(failed).toHaveLength(3);
    expect(failed.every((i) => i.error === 'Disk not found')).toBe(true);
  });

  it('never runs two disks of the same node concurrently', async () => {
    const inFlightPerNode = new Map<string, number>();
    let violated = false;

    await runInitBatch(fleet(4, 5), {
      nodeConcurrency: 4,
      setupOne: async (d) => {
        const inFlight = (inFlightPerNode.get(d.node) ?? 0) + 1;
        inFlightPerNode.set(d.node, inFlight);
        if (inFlight > 1) violated = true;
        await new Promise((r) => setTimeout(r, 1));
        inFlightPerNode.set(d.node, inFlight - 1);
        return { ok: true };
      },
    });

    expect(violated).toBe(false);
  });

  it('caps cross-node concurrency at nodeConcurrency', async () => {
    let inFlight = 0;
    let peak = 0;

    await runInitBatch(fleet(12, 2), {
      nodeConcurrency: 6,
      setupOne: async () => {
        inFlight += 1;
        peak = Math.max(peak, inFlight);
        await new Promise((r) => setTimeout(r, 1));
        inFlight -= 1;
        return { ok: true };
      },
    });

    expect(peak).toBeLessThanOrEqual(6);
    // With 12 nodes of work the cap should actually be reached, not just
    // respected — otherwise this test can pass on a serial implementation.
    expect(peak).toBe(6);
  });

  it('reports monotonic per-disk progress: pending → running → terminal', async () => {
    const seen = new Map<string, string[]>();

    await runInitBatch(fleet(2, 3), {
      onUpdate: (items: BatchItem[]) => {
        for (const item of items) {
          const history = seen.get(item.disk.key) ?? [];
          if (history[history.length - 1] !== item.status) history.push(item.status);
          seen.set(item.disk.key, history);
        }
      },
      setupOne: async () => ({ ok: true }),
    });

    for (const history of seen.values()) {
      // Each disk's visible history is a prefix-free forward walk; nothing
      // ever moves backwards (e.g. ok → running).
      expect(history).toEqual(['pending', 'running', 'ok'].slice(-history.length));
    }
  });

  it('cancel lets in-flight calls finish and drains the rest as skipped', async () => {
    let cancelled = false;
    let completedAfterCancel = 0;

    const items = await runInitBatch(fleet(1, 5), {
      setupOne: async (d) => {
        await new Promise((r) => setTimeout(r, 1));
        if (d.pci === '0000:00:1.0') cancelled = true;
        if (cancelled) completedAfterCancel += 1;
        return { ok: true };
      },
      isCancelled: () => cancelled,
    });

    // Disk 0 ran clean, disk 1 was in flight when it flipped the flag and
    // still completed; 2–4 were never dispatched.
    expect(completedAfterCancel).toBe(1);
    expect(items.map((i) => i.status)).toEqual(['ok', 'ok', 'skipped', 'skipped', 'skipped']);
  });

  it('contains a thrown setupOne as a failed item without killing the batch', async () => {
    const items = await runInitBatch(fleet(1, 3), {
      setupOne: async (d) => {
        if (d.pci === '0000:00:1.0') throw new Error('agent connection reset');
        return { ok: true };
      },
    });

    expect(items.map((i) => i.status)).toEqual(['ok', 'failed', 'ok']);
    expect(items[1].error).toBe('agent connection reset');
  });

  it('fires onNodeDrained exactly once per node, after its queue empties', async () => {
    const drained: string[] = [];
    const done = new Set<string>();

    await runInitBatch(fleet(3, 2), {
      setupOne: async (d) => {
        done.add(d.key);
        return { ok: true };
      },
      onNodeDrained: (node) => {
        drained.push(node);
        // Every disk of the drained node must already have run.
        expect(done.has(`${node}:0000:00:0.0`)).toBe(true);
        expect(done.has(`${node}:0000:00:1.0`)).toBe(true);
      },
    });

    expect(drained.sort()).toEqual(['node-0', 'node-1', 'node-2']);
  });
});

describe('selection and eligibility', () => {
  it('bulk-select skips system, initialized, and mounted disks', () => {
    expect(isBulkSelectable(nodeStatus())).toBe(true);
    expect(isBulkSelectable(nodeStatus({ is_system_disk: true }))).toBe(false);
    expect(isBulkSelectable(nodeStatus({ blobstore_initialized: true }))).toBe(false);
    expect(isBulkSelectable(nodeStatus({ mounted_partitions: ['/mnt/docker-data'] }))).toBe(false);
  });

  it('batch eligibility admits mounted disks only under force-unmount, never system/initialized', () => {
    expect(isBatchEligible(nodeStatus(), false)).toBe(true);
    expect(isBatchEligible(nodeStatus({ mounted_partitions: ['/data'] }), false)).toBe(false);
    expect(isBatchEligible(nodeStatus({ mounted_partitions: ['/data'] }), true)).toBe(true);
    // Force-unmount must not override the hard exclusions.
    expect(isBatchEligible(nodeStatus({ is_system_disk: true }), true)).toBe(false);
    expect(isBatchEligible(nodeStatus({ blobstore_initialized: true }), true)).toBe(false);
  });

  it('groups by node and by disk class with sorted labels', () => {
    const disks = [
      { nodeName: 'node-b', model: 'ModelX', size_bytes: 2 * 1024 ** 3 },
      { nodeName: 'node-a', model: 'ModelX', size_bytes: 2 * 1024 ** 3 },
      { nodeName: 'node-a', model: 'ModelY', size_bytes: 4 * 1024 ** 3 },
    ];

    const byNode = groupDisks(disks, 'node');
    expect(byNode.map((g) => g.label)).toEqual(['node-a', 'node-b']);
    expect(byNode[0].disks).toHaveLength(2);

    const byClass = groupDisks(disks, 'class');
    expect(byClass.map((g) => g.label)).toEqual(['ModelX — 2 GB', 'ModelY — 4 GB']);
    expect(byClass[0].disks).toHaveLength(2);

    expect(groupDisks([], 'none')).toEqual([]);
    expect(groupDisks(disks, 'none')).toHaveLength(1);
  });

  it('labels a disk class from model and size', () => {
    expect(diskClassLabel({ model: '', size_bytes: 1024 ** 3 })).toBe('Unknown model — 1 GB');
  });

  it('shift-click range selection follows the rendered order and degenerates without an anchor', () => {
    const order = ['a', 'b', 'c', 'd', 'e'];
    expect(rangeBetween(order, 'b', 'd')).toEqual(['b', 'c', 'd']);
    expect(rangeBetween(order, 'd', 'b')).toEqual(['b', 'c', 'd']);
    expect(rangeBetween(order, null, 'c')).toEqual(['c']);
    expect(rangeBetween(order, 'gone', 'c')).toEqual(['c']);
    expect(rangeBetween(order, 'b', 'gone')).toEqual(['gone']);
  });
});

describe('landing and badge inputs (plan Decision 2)', () => {
  it('a cluster is fresh only while zero lvstores are initialized', () => {
    expect(isFreshCluster([])).toBe(true);
    expect(isFreshCluster([{ blobstore_initialized: false }])).toBe(true);
    expect(
      isFreshCluster([{ blobstore_initialized: false }, { blobstore_initialized: true }])
    ).toBe(false);
  });

  it('the nav badge never counts system disks', () => {
    const count = uninitializedDiskCount([
      { blobstore_initialized: false },
      { blobstore_initialized: false, is_system_disk: true },
      { blobstore_initialized: true },
    ]);
    expect(count).toBe(1);
  });

  it('type-to-confirm phrase matches the modal contract', () => {
    expect(TYPE_TO_CONFIRM_THRESHOLD).toBe(10);
    expect(confirmPhrase(42)).toBe('initialize 42 disks');
  });
});
