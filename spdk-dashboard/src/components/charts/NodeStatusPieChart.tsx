import React from 'react';
import { PieChart, Pie, Cell, ResponsiveContainer, Tooltip as RechartsTooltip, Legend } from 'recharts';
import { Server, AlertTriangle, CheckCircle, XCircle } from 'lucide-react';

export interface NodeStatusData {
  status: string;
  count: number;
  percentage: number;
  color: string;
}

export interface ClusterHealth {
  status: string;
  total_nodes: number;
  healthy_nodes: number;
  degraded_nodes: number;
  failed_nodes: number;
  node_status_chart: NodeStatusData[];
}

interface NodeStatusPieChartProps {
  clusterHealth: ClusterHealth;
}

const CustomTooltip = ({ active, payload }: any) => {
  if (active && payload && payload.length) {
    const data = payload[0].payload;
    return (
      <div className="bg-white p-3 border border-gray-200 rounded-lg shadow-lg">
        <p className="font-medium">{data.status} Nodes</p>
        <p className="text-sm text-gray-600">
          {data.count} nodes ({data.percentage.toFixed(1)}%)
        </p>
      </div>
    );
  }
  return null;
};

const renderStatusIcon = (status: string) => {
  switch (status.toLowerCase()) {
    case 'healthy':
      return <CheckCircle className="w-4 h-4 text-green-600" />;
    case 'degraded':
      return <AlertTriangle className="w-4 h-4 text-orange-600" />;
    case 'failed':
      return <XCircle className="w-4 h-4 text-red-600" />;
    default:
      return <Server className="w-4 h-4 text-gray-600" />;
  }
};

const getStatusBadgeColor = (status: string) => {
  switch (status.toLowerCase()) {
    case 'healthy':
      return 'bg-green-100 text-green-800 border-green-200';
    case 'degraded':
      return 'bg-orange-100 text-orange-800 border-orange-200';
    case 'critical':
    case 'failed':
      return 'bg-red-100 text-red-800 border-red-200';
    default:
      return 'bg-gray-100 text-gray-800 border-gray-200';
  }
};

export const NodeStatusPieChart: React.FC<NodeStatusPieChartProps> = ({ clusterHealth }) => {
  const { status, total_nodes, healthy_nodes, degraded_nodes, failed_nodes, node_status_chart } = clusterHealth;

  return (
    <div className="bg-white rounded-lg shadow-lg p-6">
      <div className="flex items-center justify-between mb-4">
        <div className="flex items-center">
          <Server className="w-6 h-6 text-blue-600 mr-2" />
          <h3 className="text-lg font-semibold">Cluster Node Health</h3>
        </div>
        <div className={`px-3 py-1 rounded-full text-sm font-medium border ${getStatusBadgeColor(status)}`}>
          {status.charAt(0).toUpperCase() + status.slice(1)}
        </div>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        {/* Pie Chart */}
        <div className="h-64">
          <ResponsiveContainer width="100%" height="100%">
            <PieChart>
              <Pie
                data={node_status_chart}
                cx="50%"
                cy="50%"
                outerRadius={80}
                dataKey="count"
                label={({ percentage }) => `${percentage.toFixed(1)}%`}
                labelLine={false}
              >
                {node_status_chart.map((entry, index) => (
                  <Cell key={`cell-${index}`} fill={entry.color} />
                ))}
              </Pie>
              <RechartsTooltip content={<CustomTooltip />} />
            </PieChart>
          </ResponsiveContainer>
        </div>

        {/* Summary Stats */}
        <div className="space-y-4">
          <div className="grid grid-cols-2 gap-4">
            <div className="text-center p-3 bg-gray-50 rounded-lg">
              <div className="text-2xl font-bold text-gray-900">{total_nodes}</div>
              <div className="text-sm text-gray-600">Total Nodes</div>
            </div>
            <div className="text-center p-3 bg-green-50 rounded-lg">
              <div className="text-2xl font-bold text-green-700">{healthy_nodes}</div>
              <div className="text-sm text-green-600">Healthy</div>
            </div>
          </div>

          {/* Legend with detailed breakdown */}
          <div className="space-y-2">
            {node_status_chart.map((item, index) => (
              <div key={index} className="flex items-center justify-between p-2 rounded hover:bg-gray-50">
                <div className="flex items-center space-x-2">
                  {renderStatusIcon(item.status)}
                  <span className="font-medium">{item.status}</span>
                </div>
                <div className="flex items-center space-x-2">
                  <div 
                    className="w-4 h-4 rounded"
                    style={{ backgroundColor: item.color }}
                  />
                  <span className="text-sm text-gray-600">
                    {item.count} ({item.percentage.toFixed(1)}%)
                  </span>
                </div>
              </div>
            ))}
          </div>

          {/* Quick Actions */}
          {(degraded_nodes > 0 || failed_nodes > 0) && (
            <div className="mt-4 p-3 bg-yellow-50 rounded-lg border border-yellow-200">
              <div className="flex items-center text-yellow-800 text-sm">
                <AlertTriangle className="w-4 h-4 mr-2" />
                <span className="font-medium">
                  {failed_nodes > 0 ? `${failed_nodes} nodes need attention` : 
                   `${degraded_nodes} nodes degraded`}
                </span>
              </div>
              <div className="text-xs text-yellow-700 mt-1">
                Check the Nodes tab for details and alerts
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
