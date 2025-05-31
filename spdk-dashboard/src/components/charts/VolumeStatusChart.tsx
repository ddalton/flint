import React from 'react';
import { PieChart, Pie, Cell, ResponsiveContainer, Tooltip as RechartsTooltip } from 'recharts';
import { Database } from 'lucide-react';
import type { Volume } from '../../hooks/useDashboardData';

interface VolumeStatusChartProps {
  volumes: Volume[];
}

export const VolumeStatusChart: React.FC<VolumeStatusChartProps> = ({ volumes }) => {
  const statusCounts = volumes.reduce((acc, volume) => {
    acc[volume.state] = (acc[volume.state] || 0) + 1;
    return acc;
  }, {} as Record<string, number>);

  const data = Object.entries(statusCounts).map(([status, count]) => ({
    name: status,
    value: count,
    color: status === 'Healthy' ? '#10b981' : status === 'Rebuilding' ? '#f59e0b' : '#ef4444'
  }));

  return (
    <div className="bg-white rounded-lg shadow-lg p-6">
      <div className="flex items-center mb-4">
        <Database className="w-6 h-6 text-blue-600 mr-2" />
        <h3 className="text-lg font-semibold">PVC Volume Status Distribution</h3>
      </div>
      <ResponsiveContainer width="100%" height={300}>
        <PieChart>
          <Pie
            data={data}
            cx="50%"
            cy="50%"
            outerRadius={80}
            dataKey="value"
            label={({name, value}) => `${name}: ${value}`}
          >
            {data.map((entry, index) => (
              <Cell key={`cell-${index}`} fill={entry.color} />
            ))}
          </Pie>
          <RechartsTooltip />
        </PieChart>
      </ResponsiveContainer>
      
      <div className="mt-4 flex flex-wrap gap-2">
        {data.map((item) => (
          <span
            key={item.name}
            className="px-3 py-1 rounded-full text-sm text-white"
            style={{ backgroundColor: item.color }}
          >
            {item.name}: {item.value}
          </span>
        ))}
      </div>
    </div>
  );
};