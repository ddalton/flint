import React from 'react';
import { HardDrive } from 'lucide-react';
import { Card } from '../ui/Card';
import { VOLUME_STATE_STYLES } from '../ui/status';
import type { Disk } from '../../hooks/useDashboardData';

interface DiskStatusChartProps {
  disks: Disk[];
}

type DiskStatus = 'Healthy' | 'Uninitialized' | 'Unhealthy' | 'System';

// Helper function to determine the status of a single disk. System disks
// (EBS root/boot) get their own neutral bucket — they can never host an
// LVS, so painting them amber "Uninitialized" read as actionable work
// that wasn't (seen live on runm/runn: every node showed a phantom
// to-do segment).
const getDiskStatus = (disk: Disk): DiskStatus => {
  if (disk.is_system_disk) {
    return 'System';
  }
  if (disk.blobstore_initialized) {
    return disk.healthy ? 'Healthy' : 'Unhealthy';
  }
  return 'Uninitialized';
};

// Status hues come from status.ts — the strip can never drift from the
// chips. System is a deliberate neutral gray: informational, never
// actionable.
const DISK_STATUS_HEX: Record<DiskStatus, string> = {
  Healthy: VOLUME_STATE_STYLES.Healthy.hex,
  Uninitialized: VOLUME_STATE_STYLES.Degraded.hex,
  Unhealthy: VOLUME_STATE_STYLES.Failed.hex,
  System: '#9ca3af',
};

const STATUS_ORDER: DiskStatus[] = ['Healthy', 'Uninitialized', 'Unhealthy', 'System'];

// One row per node, one cell per disk, colored by its logical-volume-store
// state. This replaced a stacked bar chart whose bars were all one unit
// tall (one disk per node on every cluster so far): 300 px of axes to say
// what a row of cells says in 20 px (dash1, 2026-10-27).
export const DiskStatusChart: React.FC<DiskStatusChartProps> = ({ disks }) => {
  const byNode = disks.reduce((acc, disk) => {
    (acc[disk.node] ??= []).push(disk);
    return acc;
  }, {} as Record<string, Disk[]>);
  const nodes = Object.keys(byNode).sort();
  const counts = disks.reduce((acc, disk) => {
    acc[getDiskStatus(disk)]++;
    return acc;
  }, { Healthy: 0, Uninitialized: 0, Unhealthy: 0, System: 0 } as Record<DiskStatus, number>);

  return (
    <Card
      icon={HardDrive}
      title="Disk Status by Node"
      subtitle={`logical volume store state across ${disks.length} disk${disks.length !== 1 ? 's' : ''}`}
      bodyClassName="p-6"
    >
      {nodes.length === 0 ? (
        <p className="text-sm text-gray-500">
          No disks discovered yet — node agents report NVMe disks here.
        </p>
      ) : (
        <div className="space-y-4">
          <ul className="space-y-2" aria-label="Disks by node">
            {nodes.map(node => (
              <li key={node} className="flex items-center gap-3">
                <span className="w-32 flex-shrink-0 truncate text-sm font-medium text-gray-800" title={node}>
                  {node}
                </span>
                <span className="flex flex-wrap gap-1">
                  {byNode[node]!.map(disk => {
                    const status = getDiskStatus(disk);
                    return (
                      <span
                        key={disk.id}
                        role="img"
                        aria-label={`${disk.id}: ${status}`}
                        title={`${disk.id} · ${status}${disk.blobstore_initialized ? ` · ${disk.lvol_count} logical volumes` : ''}`}
                        className="h-5 w-5 rounded-sm"
                        style={{ backgroundColor: DISK_STATUS_HEX[status] }}
                      />
                    );
                  })}
                </span>
                <span className="text-xs text-gray-500">
                  {byNode[node]!.length} disk{byNode[node]!.length !== 1 ? 's' : ''}
                </span>
              </li>
            ))}
          </ul>
          <ul className="flex flex-wrap gap-x-4 gap-y-1 text-sm text-gray-700" aria-label="Legend">
            {STATUS_ORDER.map(status => (
              <li key={status} className="flex items-center gap-1.5">
                <span
                  aria-hidden="true"
                  className="inline-block h-3 w-3 rounded-sm"
                  style={{ backgroundColor: DISK_STATUS_HEX[status] }}
                />
                {status}
                <span className="text-gray-400 tabular-nums">· {counts[status]}</span>
              </li>
            ))}
          </ul>
        </div>
      )}
    </Card>
  );
};
