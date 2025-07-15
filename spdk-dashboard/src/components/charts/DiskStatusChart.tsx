import React from 'react';
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip as RechartsTooltip, Legend, ResponsiveContainer } from 'recharts';
import { HardDrive } from 'lucide-react';
import type { Disk } from '../../hooks/useDashboardData';

interface DiskStatusChartProps {
  disks: Disk[];
}

export const DiskStatusChart: React.FC<DiskStatusChartProps> = ({ disks }) => {
  const nodeData = disks.reduce((acc, disk) => {
    if (!acc[disk.node]) {
      acc[disk.node] = { total: 0, initialized: 0, healthy: 0 };
    }
    acc[disk.node].total++;
    if (disk.blobstore_initialized) acc[disk.node].initialized++;
    if (disk.healthy) acc[disk.node].healthy++;
    return acc;
  }, {} as Record<string, { total: number; initialized: number; healthy: number }>);

  const chartData = Object.entries(nodeData).map(([node, data]) => ({
    node,
    total: data.total,
    initialized: data.initialized,
    uninitialized: data.total - data.initialized,
    unhealthy: data.total - data.healthy
  }));

  return (
    <div className="bg-white rounded-lg shadow-lg p-6">
      <div className="flex items-center mb-4">
        <HardDrive className="w-6 h-6 text-blue-600 mr-2" />
        <h3 className="text-lg font-semibold">NVMe Logical Volume Store Status by Node</h3>
      </div>
      <ResponsiveContainer width="100%" height={300}>
        <BarChart data={chartData}>
          <CartesianGrid strokeDasharray="3 3" />
          <XAxis dataKey="node" />
          <YAxis />
          <RechartsTooltip />
          <Legend />
          <Bar dataKey="initialized" stackId="a" fill="#10b981" name="LVS Initialized" />
          <Bar dataKey="uninitialized" stackId="a" fill="#f59e0b" name="Uninitialized" />
          <Bar dataKey="unhealthy" stackId="b" fill="#ef4444" name="Unhealthy" />
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
};
