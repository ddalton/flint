import React from 'react';
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip as RechartsTooltip, Legend, ResponsiveContainer } from 'recharts';
import { HardDrive } from 'lucide-react';
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

export const DiskStatusChart: React.FC<DiskStatusChartProps> = ({ disks }) => {
  const nodeData = disks.reduce((acc, disk) => {
    if (!acc[disk.node]) {
      acc[disk.node] = { Healthy: 0, Uninitialized: 0, Unhealthy: 0 };
    }
    const status = getDiskStatus(disk);
    acc[disk.node][status]++;
    return acc;
  }, {} as Record<string, Record<DiskStatus, number>>);

  const chartData = Object.entries(nodeData).map(([node, data]) => ({
    node,
    ...data,
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
          <Bar dataKey="Healthy" stackId="a" fill="#10b981" name="Healthy" />
          <Bar dataKey="Uninitialized" stackId="a" fill="#f59e0b" name="Uninitialized" />
          <Bar dataKey="Unhealthy" stackId="a" fill="#ef4444" name="Unhealthy" />
        </BarChart>
      </ResponsiveContainer>
    </div>
  );
};