import React from 'react';
import {
  CheckCircle, XCircle, Loader2, RotateCcw, MinusCircle, Clock
} from 'lucide-react';
import type { BatchDisk, BatchItem } from './batchSetup';
import { TYPE_TO_CONFIRM_THRESHOLD, confirmPhrase } from './batchSetup';
import { ConfirmModal } from '../ui/ConfirmModal';
import { ProgressBar } from '../ui/ProgressBar';
import { Button } from '../ui/Button';

export interface ExcludedDisk {
  disk: BatchDisk;
  reason: string;
}

const formatCapacity = (bytes: number): string => {
  const gb = bytes / (1024 * 1024 * 1024);
  return gb >= 1024 ? `${(gb / 1024).toFixed(1)} TB` : `${Math.round(gb)} GB`;
};

interface BulkConfirmModalProps {
  disks: BatchDisk[];
  excluded: ExcludedDisk[];
  onConfirm: () => void;
  onCancel: () => void;
}

// Safety rail for bulk initialization: the operator sees exactly what will
// be wiped (node, device, PCI, serial, capacity) before anything runs, and
// large batches additionally require typing the batch size back.
export const BulkConfirmModal: React.FC<BulkConfirmModalProps> = ({
  disks, excluded, onConfirm, onCancel
}) => {
  const needsTypedConfirm = disks.length > TYPE_TO_CONFIRM_THRESHOLD;
  const phrase = confirmPhrase(disks.length);
  const totalBytes = disks.reduce((sum, disk) => sum + disk.sizeBytes, 0);
  const nodeCount = new Set(disks.map(disk => disk.node)).size;

  return (
    <ConfirmModal
      title={`Initialize ${disks.length} disk${disks.length !== 1 ? 's' : ''} for SPDK`}
      subtitle={`${nodeCount} node${nodeCount !== 1 ? 's' : ''} · ${formatCapacity(totalBytes)} total capacity`}
      danger={
        <>
          <strong>All data on the disks below will be destroyed.</strong> Each
          disk gets a new logical volume store; existing partitions and
          filesystems are wiped.
        </>
      }
      confirmLabel={`Initialize ${disks.length} disk${disks.length !== 1 ? 's' : ''}`}
      confirmPhrase={needsTypedConfirm ? phrase : undefined}
      phraseHelp={
        needsTypedConfirm ? (
          <>This is a large batch. To confirm, type:{' '}
            <span className="font-mono font-bold">{phrase}</span></>
        ) : undefined
      }
      onConfirm={onConfirm}
      onCancel={onCancel}
    >
      <div className="overflow-y-auto border border-gray-200 rounded-lg mb-4 flex-1 min-h-0">
        <table className="min-w-full divide-y divide-gray-200 text-sm">
          <thead className="bg-gray-50 sticky top-0">
            <tr>
              <th className="px-3 py-2 text-left text-xs font-medium text-gray-500 uppercase">Node</th>
              <th className="px-3 py-2 text-left text-xs font-medium text-gray-500 uppercase">Device</th>
              <th className="px-3 py-2 text-left text-xs font-medium text-gray-500 uppercase">PCI</th>
              <th className="px-3 py-2 text-left text-xs font-medium text-gray-500 uppercase">Serial</th>
              <th className="px-3 py-2 text-left text-xs font-medium text-gray-500 uppercase">Size</th>
              <th className="px-3 py-2 text-left text-xs font-medium text-gray-500 uppercase">Model</th>
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {disks.map(disk => (
              <tr key={disk.key}>
                <td className="px-3 py-1.5">{disk.node}</td>
                <td className="px-3 py-1.5 font-mono">{disk.device}</td>
                <td className="px-3 py-1.5 font-mono text-xs">{disk.pci}</td>
                <td className="px-3 py-1.5 font-mono text-xs">{disk.serial || '—'}</td>
                <td className="px-3 py-1.5">{formatCapacity(disk.sizeBytes)}</td>
                <td className="px-3 py-1.5 text-gray-500 truncate max-w-[16rem]" title={disk.model}>
                  {disk.model}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {excluded.length > 0 && (
        <div className="p-3 bg-stale-50 border border-stale-200 rounded-lg mb-4 text-sm text-stale-800">
          <p className="font-medium mb-1">
            {excluded.length} selected disk{excluded.length !== 1 ? 's' : ''} will NOT be initialized:
          </p>
          <ul className="space-y-0.5">
            {excluded.map(({ disk, reason }) => (
              <li key={disk.key} className="truncate">
                <span className="font-mono">{disk.node}:{disk.device}</span> — {reason}
              </li>
            ))}
          </ul>
        </div>
      )}
    </ConfirmModal>
  );
};

const STATUS_RENDER: Record<BatchItem['status'], { icon: React.ReactNode; label: string; text: string }> = {
  pending: { icon: <Clock className="w-4 h-4 text-gray-400" />, label: 'Pending', text: 'text-gray-500' },
  running: { icon: <Loader2 className="w-4 h-4 text-blue-600 animate-spin" />, label: 'Running', text: 'text-blue-700' },
  ok: { icon: <CheckCircle className="w-4 h-4 text-green-600" />, label: 'Initialized', text: 'text-green-700' },
  failed: { icon: <XCircle className="w-4 h-4 text-red-600" />, label: 'Failed', text: 'text-red-700' },
  skipped: { icon: <MinusCircle className="w-4 h-4 text-gray-400" />, label: 'Skipped', text: 'text-gray-500' },
};

interface BatchProgressPanelProps {
  items: BatchItem[];
  running: boolean;
  onCancel: () => void;
  onRetryFailed: () => void;
  onDismiss: () => void;
}

// Live per-disk outcome stream for a running/finished batch, grouped by
// node in dispatch order, with a partial-failure summary and retry.
export const BatchProgressPanel: React.FC<BatchProgressPanelProps> = ({
  items, running, onCancel, onRetryFailed, onDismiss
}) => {
  const counts = { pending: 0, running: 0, ok: 0, failed: 0, skipped: 0 };
  items.forEach(item => { counts[item.status]++; });
  const done = counts.ok + counts.failed + counts.skipped;
  const pct = items.length > 0 ? Math.round((done / items.length) * 100) : 0;

  const nodeOrder: string[] = [];
  const byNode = new Map<string, BatchItem[]>();
  items.forEach(item => {
    if (!byNode.has(item.disk.node)) {
      byNode.set(item.disk.node, []);
      nodeOrder.push(item.disk.node);
    }
    byNode.get(item.disk.node)!.push(item);
  });

  return (
    <div className="bg-white rounded-lg shadow p-4">
      <div className="flex items-center justify-between mb-3">
        <div className="flex items-center gap-3">
          <h3 className="text-section">
            {running ? 'Initializing disks…' : 'Bulk initialization finished'}
          </h3>
          <span className="text-sm text-gray-600">
            {counts.ok} ok
            {counts.failed > 0 && <span className="text-red-700"> · {counts.failed} failed</span>}
            {counts.skipped > 0 && <span> · {counts.skipped} skipped</span>}
            {running && <span> · {counts.pending + counts.running} remaining</span>}
          </span>
        </div>
        <div className="flex items-center gap-2">
          {running ? (
            <Button size="sm" onClick={onCancel}>
              Cancel remaining
            </Button>
          ) : (
            <>
              {counts.failed > 0 && (
                <Button variant="danger" size="sm" icon={RotateCcw} onClick={onRetryFailed}>
                  Retry {counts.failed} failed
                </Button>
              )}
              <Button size="sm" onClick={onDismiss}>
                Dismiss
              </Button>
            </>
          )}
        </div>
      </div>

      <ProgressBar
        value={pct}
        label="batch progress"
        valueText={`${counts.ok + counts.failed + counts.skipped} of ${items.length} disks done`}
        tone={counts.failed > 0 ? 'warn' : 'ok'}
        className="w-full mb-4"
      />

      <div className="space-y-3 max-h-96 overflow-y-auto">
        {nodeOrder.map(node => {
          const nodeItems = byNode.get(node)!;
          const nodeOk = nodeItems.filter(i => i.status === 'ok').length;
          return (
            <div key={node}>
              <div className="text-sm font-medium text-gray-700 mb-1">
                {node} <span className="text-gray-400 font-normal">({nodeOk}/{nodeItems.length})</span>
              </div>
              <div className="space-y-0.5">
                {nodeItems.map(item => {
                  const render = STATUS_RENDER[item.status];
                  return (
                    <div key={item.disk.key} className="flex items-center gap-2 text-sm pl-2">
                      {render.icon}
                      <span className="font-mono">{item.disk.device}</span>
                      <span className="font-mono text-xs text-gray-400">{item.disk.pci}</span>
                      <span className={`text-xs ${render.text}`}>{render.label}</span>
                      {item.error && (
                        <span className="text-xs text-red-600 truncate" title={item.error}>
                          — {item.error}
                        </span>
                      )}
                    </div>
                  );
                })}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
};
