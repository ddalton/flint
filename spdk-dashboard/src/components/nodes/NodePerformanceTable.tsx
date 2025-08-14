import React, { useState } from 'react';
import { Activity, HardDrive, Clock, TrendingUp, AlertTriangle, CheckCircle, XCircle } from 'lucide-react';
import type { NodePerformanceMetrics, ClusterPerformanceTotals } from '../../hooks/useDashboardData';

interface NodePerformanceTableProps {
  nodes: NodePerformanceMetrics[];
  clusterTotals?: ClusterPerformanceTotals;
  onNodeClick?: (nodeId: string) => void;
}

type SortField = 'node_id' | 'performance_score' | 'raid_count' | 'total_iops' | 'latency' | 'bandwidth';
type SortDirection = 'asc' | 'desc';

export const NodePerformanceTable: React.FC<NodePerformanceTableProps> = ({
  nodes,
  clusterTotals,
  onNodeClick,
}) => {
  const [sortField, setSortField] = useState<SortField>('performance_score');
  const [sortDirection, setSortDirection] = useState<SortDirection>('desc');

  const handleSort = (field: SortField) => {
    if (sortField === field) {
      setSortDirection(sortDirection === 'asc' ? 'desc' : 'asc');
    } else {
      setSortField(field);
      setSortDirection(field === 'performance_score' ? 'desc' : 'asc');
    }
  };

  const sortedNodes = [...nodes].sort((a, b) => {
    let aVal: number, bVal: number;

    switch (sortField) {
      case 'node_id':
        return sortDirection === 'asc' 
          ? a.node_id.localeCompare(b.node_id)
          : b.node_id.localeCompare(a.node_id);
      case 'performance_score':
        aVal = a.performance_score;
        bVal = b.performance_score;
        break;
      case 'raid_count':
        aVal = a.raid_count;
        bVal = b.raid_count;
        break;
      case 'total_iops':
        aVal = a.total_read_iops + a.total_write_iops;
        bVal = b.total_read_iops + b.total_write_iops;
        break;
      case 'latency':
        aVal = (a.avg_read_latency_ms + a.avg_write_latency_ms) / 2;
        bVal = (b.avg_read_latency_ms + b.avg_write_latency_ms) / 2;
        break;
      case 'bandwidth':
        aVal = a.total_read_bandwidth_mbps + a.total_write_bandwidth_mbps;
        bVal = b.total_read_bandwidth_mbps + b.total_write_bandwidth_mbps;
        break;
      default:
        aVal = a.performance_score;
        bVal = b.performance_score;
    }

    return sortDirection === 'asc' ? aVal - bVal : bVal - aVal;
  });

  const getHealthIcon = (node: NodePerformanceMetrics) => {
    if (!node.spdk_active) {
      return <XCircle className="h-4 w-4 text-gray-400" />;
    }
    if (node.failed_raids > 0) {
      return <XCircle className="h-4 w-4 text-red-500" />;
    }
    if (node.degraded_raids > 0) {
      return <AlertTriangle className="h-4 w-4 text-yellow-500" />;
    }
    return <CheckCircle className="h-4 w-4 text-green-500" />;
  };

  const getPerformanceColor = (score: number) => {
    if (score >= 80) return 'text-green-600 bg-green-50';
    if (score >= 60) return 'text-yellow-600 bg-yellow-50';
    if (score > 0) return 'text-red-600 bg-red-50';
    return 'text-gray-600 bg-gray-50';
  };

  const formatNumber = (num: number) => {
    if (num >= 1000000) return `${(num / 1000000).toFixed(1)}M`;
    if (num >= 1000) return `${(num / 1000).toFixed(1)}K`;
    return num.toString();
  };

  const SortHeader: React.FC<{ field: SortField; children: React.ReactNode }> = ({ field, children }) => (
    <th
      className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-50 select-none"
      onClick={() => handleSort(field)}
    >
      <div className="flex items-center space-x-1">
        <span>{children}</span>
        {sortField === field && (
          <span className="text-gray-400">
            {sortDirection === 'asc' ? '↑' : '↓'}
          </span>
        )}
      </div>
    </th>
  );

  return (
    <div className="space-y-4">
      {/* Cluster Overview */}
      {clusterTotals && (
        <div className="bg-gradient-to-r from-blue-50 to-indigo-50 border border-blue-200 rounded-lg p-4">
          <div className="flex items-center space-x-2 mb-3">
            <Activity className="h-5 w-5 text-blue-600" />
            <h3 className="text-lg font-semibold text-blue-900">Cluster Performance Overview</h3>
          </div>
          <div className="grid grid-cols-2 md:grid-cols-6 gap-4">
            <div className="text-center">
              <div className="text-2xl font-bold text-blue-700">{clusterTotals.total_active_nodes}</div>
              <div className="text-sm text-blue-600">Active Nodes</div>
            </div>
            <div className="text-center">
              <div className="text-2xl font-bold text-blue-700">{clusterTotals.total_raids}</div>
              <div className="text-sm text-blue-600">Total RAIDs</div>
            </div>
            <div className="text-center">
              <div className="text-2xl font-bold text-blue-700">{formatNumber(clusterTotals.total_read_iops)}</div>
              <div className="text-sm text-blue-600">Read IOPS</div>
            </div>
            <div className="text-center">
              <div className="text-2xl font-bold text-blue-700">{formatNumber(clusterTotals.total_write_iops)}</div>
              <div className="text-sm text-blue-600">Write IOPS</div>
            </div>
            <div className="text-center">
              <div className="text-2xl font-bold text-blue-700">{clusterTotals.total_bandwidth_mbps.toFixed(1)}</div>
              <div className="text-sm text-blue-600">Bandwidth (MB/s)</div>
            </div>
            <div className="text-center">
              <div className="text-2xl font-bold text-blue-700">{clusterTotals.avg_cluster_latency_ms.toFixed(1)}</div>
              <div className="text-sm text-blue-600">Avg Latency (ms)</div>
            </div>
          </div>
        </div>
      )}

      {/* Node Performance Table */}
      <div className="bg-white shadow-sm rounded-lg border">
        <div className="px-4 py-3 border-b border-gray-200">
          <h3 className="text-lg font-medium text-gray-900 flex items-center space-x-2">
            <HardDrive className="h-5 w-5" />
            <span>Node Performance Metrics</span>
          </h3>
        </div>
        
        <div className="overflow-x-auto">
          <table className="min-w-full divide-y divide-gray-200">
            <thead className="bg-gray-50">
              <tr>
                <SortHeader field="node_id">Node</SortHeader>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                  Status
                </th>
                <SortHeader field="performance_score">Score</SortHeader>
                <SortHeader field="raid_count">RAIDs</SortHeader>
                <SortHeader field="total_iops">IOPS</SortHeader>
                <SortHeader field="bandwidth">Bandwidth</SortHeader>
                <SortHeader field="latency">Latency</SortHeader>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                  Health
                </th>
              </tr>
            </thead>
            <tbody className="bg-white divide-y divide-gray-200">
              {sortedNodes.map((node) => (
                <tr
                  key={node.node_id}
                  className={`hover:bg-gray-50 ${onNodeClick ? 'cursor-pointer' : ''}`}
                  onClick={() => onNodeClick?.(node.node_id)}
                >
                  <td className="px-4 py-4 whitespace-nowrap">
                    <div className="flex items-center">
                      <div className="flex-shrink-0 h-8 w-8 bg-blue-100 rounded-full flex items-center justify-center">
                        <Activity className="h-4 w-4 text-blue-600" />
                      </div>
                      <div className="ml-3">
                        <div className="text-sm font-medium text-gray-900">{node.node_id}</div>
                        <div className="text-sm text-gray-500">{node.volume_count} volumes</div>
                      </div>
                    </div>
                  </td>
                  
                  <td className="px-4 py-4 whitespace-nowrap">
                    <span className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${
                      node.spdk_active 
                        ? 'bg-green-100 text-green-800' 
                        : 'bg-gray-100 text-gray-800'
                    }`}>
                      <div className={`w-2 h-2 rounded-full mr-1 ${
                        node.spdk_active ? 'bg-green-400' : 'bg-gray-400'
                      }`} />
                      {node.spdk_active ? 'Active' : 'Inactive'}
                    </span>
                  </td>
                  
                  <td className="px-4 py-4 whitespace-nowrap">
                    <div className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-sm font-medium ${getPerformanceColor(node.performance_score)}`}>
                      {node.performance_score > 0 ? node.performance_score.toFixed(1) : '—'}
                    </div>
                  </td>
                  
                  <td className="px-4 py-4 whitespace-nowrap text-sm text-gray-900">
                    <div className="flex items-center space-x-1">
                      <span className="font-medium">{node.raid_count}</span>
                      {node.raid_count > 5 && (
                        <TrendingUp className="h-4 w-4 text-orange-500" />
                      )}
                    </div>
                  </td>
                  
                  <td className="px-4 py-4 whitespace-nowrap text-sm text-gray-900">
                    <div className="space-y-1">
                      <div className="flex items-center space-x-2">
                        <span className="text-green-600">{formatNumber(node.total_read_iops)}R</span>
                        <span className="text-blue-600">{formatNumber(node.total_write_iops)}W</span>
                      </div>
                    </div>
                  </td>
                  
                  <td className="px-4 py-4 whitespace-nowrap text-sm text-gray-900">
                    <div className="space-y-1">
                      <div>{(node.total_read_bandwidth_mbps + node.total_write_bandwidth_mbps).toFixed(1)} MB/s</div>
                    </div>
                  </td>
                  
                  <td className="px-4 py-4 whitespace-nowrap text-sm text-gray-900">
                    <div className="flex items-center space-x-2">
                      <Clock className="h-4 w-4 text-gray-400" />
                      <span className={
                        (node.avg_read_latency_ms + node.avg_write_latency_ms) / 2 > 10 
                          ? 'text-red-600 font-medium' 
                          : 'text-gray-700'
                      }>
                        {((node.avg_read_latency_ms + node.avg_write_latency_ms) / 2).toFixed(1)}ms
                      </span>
                    </div>
                  </td>
                  
                  <td className="px-4 py-4 whitespace-nowrap">
                    <div className="flex items-center space-x-2">
                      {getHealthIcon(node)}
                      <div className="text-sm">
                        {node.healthy_raids > 0 && (
                          <span className="text-green-600">{node.healthy_raids}H </span>
                        )}
                        {node.degraded_raids > 0 && (
                          <span className="text-yellow-600">{node.degraded_raids}D </span>
                        )}
                        {node.failed_raids > 0 && (
                          <span className="text-red-600">{node.failed_raids}F</span>
                        )}
                      </div>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
};

export default NodePerformanceTable;
