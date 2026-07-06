import React from 'react';
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip as RechartsTooltip, Legend, ResponsiveContainer } from 'recharts';
import { HardDrive } from 'lucide-react';
import { Card } from '../ui/Card';
import { VOLUME_STATE_STYLES } from '../ui/status';
import type { Disk } from '../../hooks/useDashboardData';

interface DiskStatusChartProps {
  disks: Disk[];
}

type DiskStatus = 'Healthy' | 'Uninitialized' | 'Unhealthy';

// Helper function to determine the status of a single disk
const getDiskStatus = (disk: Disk): DiskStatus => {
  if (disk.blobstore_initialized) {
    return disk.healthy ? 'Healthy' : 'Unhealthy';
  }
  return 'Uninitialized';
};

// Status hues come from status.ts — the chart can never drift from the chips
// (the old version hardcoded the same hexes and would have).
const DISK_STATUS_HEX: Record<DiskStatus, string> = {
  Healthy: VOLUME_STATE_STYLES.Healthy.hex,
  Uninitialized: VOLUME_STATE_STYLES.Degraded.hex,
  Unhealthy: VOLUME_STATE_STYLES.Failed.hex,
};

const AXIS_TICK = { fontSize: 12, fill: '#6b7280' };
const AXIS_LINE = { stroke: '#e5e7eb' };

export const DiskStatusChart: React.FC<DiskStatusChartProps> = ({ disks }) => {
  const nodeData = disks.reduce((acc, disk) => {
    const bucket = acc[disk.node] ?? { Healthy: 0, Uninitialized: 0, Unhealthy: 0 };
    acc[disk.node] = bucket;
    bucket[getDiskStatus(disk)]++;
    return acc;
  }, {} as Record<string, Record<DiskStatus, number>>);

  const chartData = Object.entries(nodeData).map(([node, data]) => ({
    node,
    ...data,
  }));

  return (
    <Card
      icon={HardDrive}
      title="Disk Status by Node"
      subtitle={`logical volume store state across ${disks.length} disk${disks.length !== 1 ? 's' : ''}`}
      bodyClassName="p-6"
    >
      {chartData.length === 0 ? (
        <p className="text-sm text-gray-500">
          No disks discovered yet — node agents report NVMe disks here.
        </p>
      ) : (
        <ResponsiveContainer width="100%" height={300}>
          <BarChart data={chartData}>
            <CartesianGrid vertical={false} stroke="#f3f4f6" />
            <XAxis dataKey="node" tick={AXIS_TICK} tickLine={false} axisLine={AXIS_LINE} />
            <YAxis allowDecimals={false} tick={AXIS_TICK} tickLine={false} axisLine={AXIS_LINE} width={32} />
            <RechartsTooltip cursor={{ fill: 'rgba(0, 0, 0, 0.04)' }} />
            <Legend
              iconType="circle"
              iconSize={10}
              formatter={(value: string) => <span className="text-sm text-gray-700">{value}</span>}
            />
            {/* White segment strokes = the 2px surface gap between stacked fills */}
            <Bar dataKey="Healthy" stackId="a" fill={DISK_STATUS_HEX.Healthy} stroke="#ffffff" strokeWidth={1} maxBarSize={40} name="Healthy" />
            <Bar dataKey="Uninitialized" stackId="a" fill={DISK_STATUS_HEX.Uninitialized} stroke="#ffffff" strokeWidth={1} maxBarSize={40} name="Uninitialized" />
            <Bar dataKey="Unhealthy" stackId="a" fill={DISK_STATUS_HEX.Unhealthy} stroke="#ffffff" strokeWidth={1} maxBarSize={40} name="Unhealthy" />
          </BarChart>
        </ResponsiveContainer>
      )}
    </Card>
  );
};
