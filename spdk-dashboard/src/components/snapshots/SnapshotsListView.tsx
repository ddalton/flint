import React, { useState } from 'react';
import { Camera, ChevronDown, ChevronRight, Eye } from 'lucide-react';
import type { SnapshotDetails } from './types';
import { IconButton } from '../ui/Button';
import { Chip } from '../ui/Chip';
import { snapshotKindLabel } from './types';

interface SnapshotsListViewProps {
  snapshots: SnapshotDetails[];
  onSnapshotSelect: (snapshot: SnapshotDetails) => void;
  formatSize: (bytes: number) => string;
  formatTime: (timeString: string) => string;
  getSnapshotTypeIcon: (type: string) => React.ReactNode;
}

// One compact row per logical snapshot; the per-node copies unfold on
// demand. The earlier card layout spent ~700 px per snapshot on the same
// three bdev names plus boilerplate ("High Availability Snapshot",
// "Consistency Guarantee") that said nothing the wire did not — eight
// snapshots made a 6,000 px page on dash1 (2026-10-27).
export const SnapshotsListView: React.FC<SnapshotsListViewProps> = ({
  snapshots,
  onSnapshotSelect,
  formatSize,
  getSnapshotTypeIcon
}) => {
  const [open, setOpen] = useState<Set<string>>(() => new Set());
  const toggle = (id: string) =>
    setOpen(prev => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  if (snapshots.length === 0) {
    return (
      <div className="text-center py-12">
        <Camera className="w-16 h-16 text-gray-400 mx-auto mb-4" />
        <h3 className="text-lg font-medium text-gray-900 mb-2">No snapshots found</h3>
        <p className="text-gray-500">
          Try adjusting your filters to see more results or create some snapshots.
        </p>
      </div>
    );
  }

  return (
    <div className="bg-white border border-gray-200 rounded-lg overflow-x-auto">
      <table className="min-w-full divide-y divide-gray-200">
        <thead className="bg-gray-50">
          <tr>
            {['', 'Snapshot', 'Kind', 'Source volume', 'Size', 'Copies', 'Status', ''].map((h, i) => (
              <th
                key={i}
                scope="col"
                className="px-3 py-2 text-left text-xs font-medium text-gray-500 uppercase tracking-wider"
              >
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody className="divide-y divide-gray-100">
          {snapshots.map((snapshot) => {
            const expanded = open.has(snapshot.snapshot_id);
            const copies = snapshot.replica_bdev_details;
            const name = copies[0]?.name ?? snapshot.snapshot_id;
            return (
              <React.Fragment key={snapshot.snapshot_id}>
                <tr className="hover:bg-gray-50">
                  <td className="px-3 py-2 w-8">
                    <button
                      type="button"
                      onClick={() => toggle(snapshot.snapshot_id)}
                      aria-expanded={expanded}
                      aria-label={`${expanded ? 'Hide' : 'Show'} the copies of ${name}`}
                      className="text-gray-500 hover:text-gray-800"
                    >
                      {expanded ? <ChevronDown className="w-4 h-4" /> : <ChevronRight className="w-4 h-4" />}
                    </button>
                  </td>
                  <td className="px-3 py-2 min-w-0">
                    <div className="flex items-center gap-2">
                      {getSnapshotTypeIcon(snapshot.snapshot_type)}
                      <span className="font-mono text-xs text-gray-900 truncate max-w-[26rem]" title={name}>
                        {name}
                      </span>
                    </div>
                  </td>
                  <td className="px-3 py-2 whitespace-nowrap">
                    <Chip
                      label={snapshotKindLabel(snapshot.snapshot_type)}
                      chip={snapshot.snapshot_type === 'user'
                        ? 'bg-brand-100 text-brand-800 border-brand-200'
                        : 'bg-gray-100 text-gray-700 border-gray-200'}
                    />
                  </td>
                  <td className="px-3 py-2">
                    <span className="font-mono text-xs text-gray-600 truncate block max-w-[14rem]" title={snapshot.source_volume_id}>
                      {snapshot.source_volume_id}
                    </span>
                  </td>
                  <td className="px-3 py-2 whitespace-nowrap text-sm text-gray-900">
                    {formatSize(snapshot.size_bytes)}
                  </td>
                  <td className="px-3 py-2">
                    <div className="flex flex-wrap gap-1">
                      {copies.map(c => (
                        <span key={c.node} className="inline-flex px-2 py-0.5 text-xs bg-gray-100 text-gray-800 rounded">
                          {c.node}
                        </span>
                      ))}
                    </div>
                  </td>
                  <td className="px-3 py-2 whitespace-nowrap">
                    <Chip
                      label={snapshot.ready_to_use ? 'Ready' : 'Creating'}
                      chip={snapshot.ready_to_use
                        ? 'bg-healthy-100 text-healthy-800 border-healthy-200'
                        : 'bg-degraded-100 text-degraded-800 border-degraded-200'}
                    />
                  </td>
                  <td className="px-3 py-2 whitespace-nowrap text-right">
                    <IconButton
                      icon={Eye}
                      aria-label="View snapshot details"
                      onClick={() => onSnapshotSelect(snapshot)}
                    />
                  </td>
                </tr>
                {expanded && (
                  <tr className="bg-gray-50">
                    <td />
                    <td colSpan={7} className="px-3 py-3">
                      {/* The per-node copies: one lvol per leg from one
                          coordinated cut. SPDK stores no creation time for
                          an lvol; the Timeline view has the real one. */}
                      <table className="min-w-full text-xs">
                        <thead>
                          <tr className="text-gray-500">
                            <th scope="col" className="text-left font-medium pr-4 pb-1">Node</th>
                            <th scope="col" className="text-left font-medium pr-4 pb-1">Snapshot bdev</th>
                            <th scope="col" className="text-left font-medium pr-4 pb-1">Source bdev</th>
                            <th scope="col" className="text-left font-medium pb-1">Driver</th>
                          </tr>
                        </thead>
                        <tbody>
                          {copies.map(c => (
                            <tr key={`${c.node}-${c.name}`}>
                              <td className="pr-4 py-0.5 font-medium text-gray-800">{c.node}</td>
                              <td className="pr-4 py-0.5 font-mono text-gray-700 break-all">{c.name}</td>
                              <td className="pr-4 py-0.5 font-mono text-gray-600 break-all">{c.snapshot_source_bdev || '—'}</td>
                              <td className="py-0.5 text-gray-600">{c.driver}</td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                      <p className="mt-2 text-xs text-gray-500">
                        {copies.length} cop{copies.length === 1 ? 'y' : 'ies'} from one coordinated cut; created time: see the Timeline view.
                      </p>
                    </td>
                  </tr>
                )}
              </React.Fragment>
            );
          })}
        </tbody>
      </table>
    </div>
  );
};
