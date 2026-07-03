// Bulk disk initialization: pure orchestration and selection logic
// (improvement-plan 2d). No React in here — the DiskSetupTab wires these
// helpers to state; keeping them pure lets Phase 3 put unit tests on the
// batch semantics directly.

import type { UnimplementedDisk } from '../../hooks/useDashboardData';

export type BatchItemStatus = 'pending' | 'running' | 'ok' | 'failed' | 'skipped';

export interface BatchDisk {
  key: string; // `${node}:${pci}` — matches the tab's selection keys
  node: string;
  pci: string;
  device: string;
  model: string;
  serial: string;
  sizeBytes: number;
}

export interface BatchItem {
  disk: BatchDisk;
  status: BatchItemStatus;
  error?: string;
}

export type SetupOneResult = { ok: true } | { ok: false; error: string };

export interface RunInitBatchOptions {
  setupOne: (disk: BatchDisk) => Promise<SetupOneResult>;
  // Max node queues in flight at once. Within a node disks always run
  // serially: the agent mutates shared host state (driver binding,
  // hugepages), so per-node parallelism is not safe.
  nodeConcurrency?: number;
  onUpdate?: (items: BatchItem[]) => void;
  isCancelled?: () => boolean;
  // Fires when a node's queue empties — the caller refreshes that node's
  // disk list once instead of after every disk.
  onNodeDrained?: (node: string) => void;
}

const DEFAULT_NODE_CONCURRENCY = 6;

// Runs one setup call per disk (the agent's /disks/setup loops per-PCI
// anyway, so per-disk calls cost nothing extra and buy a live status per
// disk). Cancellation is checked before each dispatch; disks not yet
// started drain as 'skipped', in-flight calls complete.
export async function runInitBatch(
  disks: BatchDisk[],
  options: RunInitBatchOptions
): Promise<BatchItem[]> {
  const {
    setupOne,
    nodeConcurrency = DEFAULT_NODE_CONCURRENCY,
    onUpdate,
    isCancelled = () => false,
    onNodeDrained,
  } = options;

  const items: BatchItem[] = disks.map(disk => ({ disk, status: 'pending' }));
  const emit = () => onUpdate?.(items.map(item => ({ ...item })));

  const nodeOrder: string[] = [];
  const queues = new Map<string, number[]>();
  items.forEach((item, index) => {
    const node = item.disk.node;
    if (!queues.has(node)) {
      queues.set(node, []);
      nodeOrder.push(node);
    }
    queues.get(node)!.push(index);
  });

  let nextNode = 0;
  const worker = async () => {
    while (nextNode < nodeOrder.length) {
      const node = nodeOrder[nextNode++];
      for (const index of queues.get(node)!) {
        if (isCancelled()) {
          items[index] = { ...items[index], status: 'skipped' };
          emit();
          continue;
        }
        items[index] = { ...items[index], status: 'running' };
        emit();
        let result: SetupOneResult;
        try {
          result = await setupOne(items[index].disk);
        } catch (error) {
          result = { ok: false, error: error instanceof Error ? error.message : String(error) };
        }
        items[index] = result.ok
          ? { ...items[index], status: 'ok', error: undefined }
          : { ...items[index], status: 'failed', error: result.error };
        emit();
      }
      onNodeDrained?.(node);
    }
  };

  const workerCount = Math.max(1, Math.min(nodeConcurrency, nodeOrder.length));
  await Promise.all(Array.from({ length: workerCount }, worker));
  return items;
}

// A disk the bulk-select actions pick up on their own: uninitialized,
// non-system, unmounted. Mounted disks require a deliberate per-disk check
// plus Force Unmount; they are never swept into a group/cluster select.
export function isBulkSelectable(disk: UnimplementedDisk): boolean {
  return !disk.is_system_disk && !disk.blobstore_initialized && disk.mounted_partitions.length === 0;
}

// A selected disk the batch will actually include. Initialized and system
// disks never ride along regardless of selection state.
export function isBatchEligible(disk: UnimplementedDisk, forceUnmount: boolean): boolean {
  if (disk.is_system_disk || disk.blobstore_initialized) return false;
  if (disk.mounted_partitions.length > 0) return forceUnmount;
  return true;
}

export type GroupBy = 'node' | 'class' | 'none';

export interface DiskGroup<T> {
  key: string;
  label: string;
  disks: T[];
}

type GroupableDisk = { nodeName: string; model: string; size_bytes: number };

export function diskClassLabel(disk: { model: string; size_bytes: number }): string {
  const gb = Math.round(disk.size_bytes / (1024 * 1024 * 1024));
  return `${disk.model || 'Unknown model'} — ${gb} GB`;
}

export function groupDisks<T extends GroupableDisk>(disks: T[], groupBy: GroupBy): DiskGroup<T>[] {
  if (groupBy === 'none') {
    return disks.length > 0 ? [{ key: 'all', label: 'All disks', disks }] : [];
  }
  const groups = new Map<string, DiskGroup<T>>();
  for (const disk of disks) {
    const label = groupBy === 'node' ? disk.nodeName : diskClassLabel(disk);
    const key = `${groupBy}:${label}`;
    if (!groups.has(key)) {
      groups.set(key, { key, label, disks: [] });
    }
    groups.get(key)!.disks.push(disk);
  }
  return Array.from(groups.values()).sort((a, b) => a.label.localeCompare(b.label));
}

// Shift-click range selection over the currently rendered order. With no
// usable anchor the range degenerates to the clicked disk.
export function rangeBetween(order: string[], anchorKey: string | null, targetKey: string): string[] {
  const targetIndex = order.indexOf(targetKey);
  if (targetIndex === -1) return [targetKey];
  const anchorIndex = anchorKey ? order.indexOf(anchorKey) : -1;
  if (anchorIndex === -1) return [targetKey];
  const [start, end] = anchorIndex <= targetIndex ? [anchorIndex, targetIndex] : [targetIndex, anchorIndex];
  return order.slice(start, end + 1);
}

// Landing decision (plan Decision 2): a cluster with zero initialized
// lvstores is fresh — the operator's first job is Disk Setup. An empty
// disk list also counts: nothing is initialized yet either way.
export function isFreshCluster(disks: Array<{ blobstore_initialized: boolean }>): boolean {
  return disks.every(disk => !disk.blobstore_initialized);
}

// Badge count: only disks an operator could actually initialize — system
// disks are never candidates, so counting them would pin the badge forever.
export function uninitializedDiskCount(
  disks: Array<{ blobstore_initialized: boolean; is_system_disk?: boolean }>
): number {
  return disks.filter(disk => !disk.blobstore_initialized && !disk.is_system_disk).length;
}

// Type-to-confirm gate: above this many disks the confirmation modal
// requires typing the batch size back.
export const TYPE_TO_CONFIRM_THRESHOLD = 10;

export function confirmPhrase(count: number): string {
  return `initialize ${count} disks`;
}
