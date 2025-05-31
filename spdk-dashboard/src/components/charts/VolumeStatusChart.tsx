import React from 'react';
import { PieChart, Pie, Cell, ResponsiveContainer, Tooltip as RechartsTooltip } from 'recharts';
import { Database, CheckCircle, AlertTriangle, XCircle, Settings } from 'lucide-react';
import type { Volume } from '../../hooks/useDashboardData';

interface VolumeStatusChartProps {
  volumes: Volume[];
}

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

  const getStateColor = (status: string) => {
    switch (status) {
      case 'Healthy': return '#10b981';
      case 'Degraded': return '#f59e0b';
      case 'Failed': return '#ef4444';
      default: return '#6b7280';
    }
  };

  const data = Object.entries(statusCounts).map(([status, count]) => ({
    name: status,
    value: count,
    color: getStateColor(status)
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
            className="px-3 py-1 rounded-full text-sm text-white flex items-center gap-1"
            style={{ backgroundColor: item.color }}
          >
            {item.name === 'Failed' && <XCircle className="w-3 h-3" />}
            {item.name === 'Degraded' && <AlertTriangle className="w-3 h-3" />}
            {item.name === 'Healthy' && <CheckCircle className="w-3 h-3" />}
            {item.name}: {item.value}
          </span>
        ))}
      </div>
      
      {/* Add rebuilding activity indicator */}
      {volumesWithRebuilding > 0 && (
        <div className="mt-4 p-3 bg-orange-50 rounded-lg border border-orange-200">
          <div className="flex items-center gap-2">
            <Settings className="w-4 h-4 text-orange-600" />
            <span className="text-sm font-medium text-orange-800">
              Rebuilding Activity: {volumesWithRebuilding} volume{volumesWithRebuilding !== 1 ? 's' : ''} 
              {volumesWithRebuilding === 1 ? ' has' : ' have'} rebuilding replicas
            </span>
          </div>
          <div className="text-xs text-orange-700 mt-1">
            Replica recovery operations are in progress to restore full redundancy
          </div>
        </div>
      )}
      
      {/* Status summary for volume states only */}
      {(statusCounts.Failed > 0 || statusCounts.Degraded > 0) && (
        <div className="mt-4 p-3 bg-gray-50 rounded-lg">
          <h4 className="text-sm font-medium text-gray-700 mb-2">Volume Status Summary</h4>
          <div className="space-y-1 text-xs">
            {statusCounts.Failed > 0 && (
              <div className="flex items-center gap-2 text-red-700">
                <XCircle className="w-3 h-3" />
                <span>{statusCounts.Failed} volume{statusCounts.Failed !== 1 ? 's' : ''} failed - immediate attention required</span>
              </div>
            )}
            {statusCounts.Degraded > 0 && (
              <div className="flex items-center gap-2 text-yellow-700">
                <AlertTriangle className="w-3 h-3" />
                <span>{statusCounts.Degraded} volume{statusCounts.Degraded !== 1 ? 's' : ''} degraded - reduced redundancy but functional</span>
              </div>
            )}
            {statusCounts.Healthy > 0 && (
              <div className="flex items-center gap-2 text-green-700">
                <CheckCircle className="w-3 h-3" />
                <span>{statusCounts.Healthy} volume{statusCounts.Healthy !== 1 ? 's' : ''} healthy - all replicas operational</span>
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
};