import React from 'react';
import { volumeStateStyle } from '../ui/status';
import { Database, Settings, AlertTriangle, CheckCircle, XCircle } from 'lucide-react';
import { Card } from '../ui/Card';
import { Chip } from '../ui/Chip';
import type { Volume } from '../../hooks/useDashboardData';

interface VolumeStatusChartProps {
  volumes: Volume[];
}

// Part-to-whole of volume states: one horizontal stacked bar (status hues
// from status.ts, 2px surface gaps) plus labeled count chips. Replaces the
// old pie — close values are unreadable as pie slices, and every value here
// is also carried by a labeled chip so color is never the only encoding.
export const VolumeStatusChart: React.FC<VolumeStatusChartProps> = ({ volumes }) => {
  const statusCounts = volumes.reduce((acc, volume) => {
    acc[volume.state] = (acc[volume.state] || 0) + 1;
    return acc;
  }, {} as Record<string, number>);

  // Count volumes with rebuilding replica activity separately
  const volumesWithRebuilding = volumes.filter(v =>
    v.replica_statuses.some(replica =>
      replica.status === 'rebuilding' ||
      replica.rebuild_progress !== null ||
      replica.is_new_replica
    )
  ).length;

  const total = volumes.length;

  // Problems lead: the most broken states sit at the left edge of the bar,
  // healthy volume fills out the right (same priority order the tables use).
  const segments = Object.entries(statusCounts)
    .map(([name, value]) => ({ name, value, style: volumeStateStyle(name) }))
    .sort((a, b) => b.style.priority - a.style.priority);

  return (
    <Card
      icon={Database}
      title="Volume Status"
      subtitle={`${total} PVC volume${total !== 1 ? 's' : ''}`}
      bodyClassName="p-6 space-y-4"
    >
      {total === 0 ? (
        <p className="text-sm text-gray-500">
          No volumes yet — provisioned PVC volumes appear here.
        </p>
      ) : (
        <>
          <div
            className="flex h-6 rounded overflow-hidden"
            role="img"
            aria-label={`Volume states: ${segments.map(s => `${s.value} ${s.name}`).join(', ')}`}
          >
            {segments.map((segment, i) => (
              <div
                key={segment.name}
                className={`h-full ${i > 0 ? 'ml-0.5' : ''}`}
                style={{
                  width: `${(segment.value / total) * 100}%`,
                  backgroundColor: segment.style.hex,
                  minWidth: '3px',
                }}
                title={`${segment.name}: ${segment.value} of ${total} (${Math.round((segment.value / total) * 100)}%)`}
              />
            ))}
          </div>

          <div className="flex flex-wrap gap-2">
            {segments.map(segment => (
              <Chip
                key={segment.name}
                label={`${segment.name} · ${segment.value}`}
                chip={segment.style.chip}
                icon={segment.style.icon}
                title={segment.style.tooltip}
              />
            ))}
          </div>
        </>
      )}

      {/* Rebuilding activity indicator */}
      {volumesWithRebuilding > 0 && (
        <div className="p-3 bg-rebuilding-50 rounded-lg border border-rebuilding-200">
          <div className="flex items-center gap-2">
            <Settings aria-hidden="true" className="w-4 h-4 text-rebuilding-600" />
            <span className="text-sm font-medium text-rebuilding-800">
              Rebuilding Activity: {volumesWithRebuilding} volume{volumesWithRebuilding !== 1 ? 's' : ''}
              {volumesWithRebuilding === 1 ? ' has' : ' have'} rebuilding replicas
            </span>
          </div>
          <div className="text-xs text-rebuilding-700 mt-1">
            Replica recovery operations are in progress to restore full redundancy
          </div>
        </div>
      )}

      {/* Status summary for volume states only */}
      {((statusCounts.Failed ?? 0) > 0 || (statusCounts.Degraded ?? 0) > 0) && (
        <div className="p-3 bg-gray-50 rounded-lg">
          <h4 className="text-sm font-medium text-gray-700 mb-2">Volume Status Summary</h4>
          <div className="space-y-1 text-xs">
            {(statusCounts.Failed ?? 0) > 0 && (
              <div className="flex items-center gap-2 text-failed-700">
                <XCircle aria-hidden="true" className="w-3 h-3" />
                <span>{(statusCounts.Failed ?? 0)} volume{(statusCounts.Failed ?? 0) !== 1 ? 's' : ''} failed - immediate attention required</span>
              </div>
            )}
            {(statusCounts.Degraded ?? 0) > 0 && (
              <div className="flex items-center gap-2 text-degraded-700">
                <AlertTriangle aria-hidden="true" className="w-3 h-3" />
                <span>{(statusCounts.Degraded ?? 0)} volume{(statusCounts.Degraded ?? 0) !== 1 ? 's' : ''} degraded - reduced redundancy but functional</span>
              </div>
            )}
            {(statusCounts.Healthy ?? 0) > 0 && (
              <div className="flex items-center gap-2 text-healthy-700">
                <CheckCircle aria-hidden="true" className="w-3 h-3" />
                <span>{(statusCounts.Healthy ?? 0)} volume{(statusCounts.Healthy ?? 0) !== 1 ? 's' : ''} healthy - all replicas operational</span>
              </div>
            )}
          </div>
        </div>
      )}
    </Card>
  );
};
