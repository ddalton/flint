import React, { useState } from 'react';
import {
  AlertTriangle, CheckCircle, XCircle, Loader2, Play, RotateCcw, MinusCircle, Clock
} from 'lucide-react';
import type { BatchDisk, BatchItem } from './batchSetup';
import { TYPE_TO_CONFIRM_THRESHOLD, confirmPhrase } from './batchSetup';

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
  const [confirmText, setConfirmText] = useState('');
  const needsTypedConfirm = disks.length > TYPE_TO_CONFIRM_THRESHOLD;
  const phrase = confirmPhrase(disks.length);
  const confirmEnabled = !needsTypedConfirm || confirmText.trim() === phrase;
  const totalBytes = disks.reduce((sum, disk) => sum + disk.sizeBytes, 0);
  const nodeCount = new Set(disks.map(disk => disk.node)).size;

  return (
    <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
      <div className="bg-white rounded-lg p-6 max-w-3xl w-full mx-4 max-h-[85vh] flex flex-col">
        <div className="flex items-center gap-3 mb-4">
          <AlertTriangle className="w-8 h-8 text-red-600" />
          <div>
            <h3 className="text-lg font-bold text-gray-900">
              Initialize {disks.length} disk{disks.length !== 1 ? 's' : ''} for SPDK
            </h3>
            <p className="text-sm text-gray-600">
              {nodeCount} node{nodeCount !== 1 ? 's' : ''} · {formatCapacity(totalBytes)} total capacity
            </p>
          </div>
        </div>

        <div className="p-3 bg-red-50 border border-red-200 rounded-lg mb-4">
          <p className="text-sm text-red-800">
            <strong>All data on the disks below will be destroyed.</strong> Each disk gets a
            new logical volume store; existing partitions and filesystems are wiped.
          </p>
        </div>

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
          <div className="p-3 bg-amber-50 border border-amber-200 rounded-lg mb-4 text-sm text-amber-800">
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

        {needsTypedConfirm && (
          <div className="mb-4">
            <label className="block text-sm font-medium text-gray-700 mb-2">
              This is a large batch. To confirm, type:{' '}
              <span className="font-mono font-bold">{phrase}</span>
            </label>
            <input
              type="text"
              value={confirmText}
              onChange={(e) => setConfirmText(e.target.value)}
              placeholder={phrase}
              className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-red-500"
            />
          </div>
        )}

        <div className="flex gap-3 justify-end">
          <button
            onClick={onCancel}
            className="px-4 py-2 border border-gray-300 text-gray-700 rounded hover:bg-gray-50"
          >
            Cancel
          </button>
          <button
            onClick={onConfirm}
            disabled={!confirmEnabled}
            className="px-4 py-2 bg-red-600 text-white rounded hover:bg-red-700 disabled:opacity-50 disabled:cursor-not-allowed flex items-center gap-2"
          >
            <Play className="w-4 h-4" />
            Initialize {disks.length} disk{disks.length !== 1 ? 's' : ''}
          </button>
        </div>
      </div>
    </div>
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
          <h3 className="text-lg font-semibold">
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
            <button
              onClick={onCancel}
              className="px-3 py-1.5 text-sm border border-gray-300 text-gray-700 rounded hover:bg-gray-50"
            >
              Cancel remaining
            </button>
          ) : (
            <>
              {counts.failed > 0 && (
                <button
                  onClick={onRetryFailed}
                  className="px-3 py-1.5 text-sm bg-red-600 text-white rounded hover:bg-red-700 flex items-center gap-1.5"
                >
                  <RotateCcw className="w-4 h-4" />
                  Retry {counts.failed} failed
                </button>
              )}
              <button
                onClick={onDismiss}
                className="px-3 py-1.5 text-sm border border-gray-300 text-gray-700 rounded hover:bg-gray-50"
              >
                Dismiss
              </button>
            </>
          )}
        </div>
      </div>

      <div className="w-full bg-gray-200 rounded-full h-2 mb-4">
        <div
          className={`h-2 rounded-full transition-all ${counts.failed > 0 ? 'bg-amber-500' : 'bg-green-500'}`}
          style={{ width: `${pct}%` }}
        />
      </div>

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
