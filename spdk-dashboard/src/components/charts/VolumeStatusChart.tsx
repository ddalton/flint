import React from 'react';
import { volumeStateStyle } from '../ui/status';
import { Database, RefreshCw } from 'lucide-react';
import { Card } from '../ui/Card';
import { Chip } from '../ui/Chip';
import type { Volume } from '../../hooks/useDashboardData';
import { isVolumeRecovering } from '../../hooks/useDashboardData';

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

  // Recovery is the Tier-2 signal: any replica not in_sync (the same
  // predicate as the Recovering stat card and filter).
  const recovering = volumes.filter(isVolumeRecovering).length;

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

      {/* Recovery activity: replicas catching up (stale → standby → in sync) */}
      {recovering > 0 && (
        <div className="p-3 bg-rebuilding-50 rounded-lg border border-rebuilding-200">
          <div className="flex items-center gap-2">
            <RefreshCw aria-hidden="true" className="w-4 h-4 text-rebuilding-600" />
            <span className="text-sm font-medium text-rebuilding-800">
              Replica recovery: {recovering} volume{recovering !== 1 ? 's' : ''}
              {recovering === 1 ? ' has' : ' have'} a replica catching up
            </span>
          </div>
          <div className="text-xs text-rebuilding-700 mt-1">
            Stale legs are caught up from epoch snapshots to standby, then hot-rejoined into the live raid
          </div>
        </div>
      )}

    </Card>
  );
};
