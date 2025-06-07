import React from 'react';
import { Filter, X } from 'lucide-react';
import { NodeDetailView } from './NodeDetailView';
import type { DashboardData, VolumeFilter } from '../../hooks/useDashboardData';

interface FilteredNodesViewProps {
  data: DashboardData;
  activeFilter?: VolumeFilter;
  onClearFilter?: () => void;
  onDiskVolumeFilter?: (diskId: string) => void;
}

export const FilteredNodesView: React.FC<FilteredNodesViewProps> = ({ 
  data, 
  activeFilter, 
  onClearFilter,
  onDiskVolumeFilter
}) => {
  // Filter volumes based on the active filter
  const getFilteredVolumes = () => {
    if (!activeFilter || activeFilter === 'all') {
      return data.volumes;
    }

    switch (activeFilter) {
      case 'healthy':
        return data.volumes.filter(v => v.state === 'Healthy');
      case 'degraded':
        return data.volumes.filter(v => v.state === 'Degraded');
      case 'failed':
        return data.volumes.filter(v => v.state === 'Failed');
      case 'faulted':
        return data.volumes.filter(v => v.state === 'Degraded' || v.state === 'Failed');
      case 'rebuilding':
        // Volumes that have any rebuilding replica activity
        return data.volumes.filter(v => 
          v.replica_statuses.some(replica => 
            replica.status === 'rebuilding' || 
            replica.rebuild_progress !== null ||
            replica.is_new_replica
          )
        );
      case 'local-nvme':
        return data.volumes.filter(v => v.local_nvme);
      default:
        return data.volumes;
    }
  };

  // Get nodes that have the filtered volumes with the specific condition
  const getFilteredNodes = () => {
    const filteredVolumes = getFilteredVolumes();
    
    if (filteredVolumes.length === 0) {
      return [];
    }

    // Get nodes based on the specific filter type
    const relevantNodes = new Set<string>();
    
    filteredVolumes.forEach(volume => {
      switch (activeFilter) {
        case 'healthy':
        case 'degraded':
        case 'failed':
        case 'faulted':
          // Include all nodes that have replicas of volumes in these states
          volume.replica_statuses.forEach(replica => {
            relevantNodes.add(replica.node);
          });
          break;
          
        case 'rebuilding':
          // Only include nodes that actually have rebuilding replicas
          volume.replica_statuses.forEach(replica => {
            if (replica.status === 'rebuilding' || 
                replica.rebuild_progress !== null ||
                replica.is_new_replica) {
              relevantNodes.add(replica.node);
            }
          });
          break;
          
        case 'local-nvme':
          // Only include nodes that have local NVMe replicas for this volume
          volume.replica_statuses.forEach(replica => {
            if (replica.is_local) {
              relevantNodes.add(replica.node);
            }
          });
          break;
          
        default:
          // For 'all' or unknown filters, include all nodes with any replica
          volume.nodes.forEach(node => {
            relevantNodes.add(node);
          });
          break;
      }
    });

    return Array.from(relevantNodes);
  };

  const getFilterDisplayName = (filter: VolumeFilter) => {
    switch (filter) {
      case 'healthy': return 'Healthy Volumes';
      case 'degraded': return 'Degraded Volumes';
      case 'failed': return 'Failed Volumes';
      case 'faulted': return 'Faulted Volumes (Degraded + Failed)';
      case 'rebuilding': return 'Volumes with Rebuilding Replicas';
      case 'local-nvme': return 'Local NVMe Volumes';
      default: return 'All Volumes';
    }
  };

  const getNodeFilterDescription = (filter: VolumeFilter) => {
    switch (filter) {
      case 'healthy': return 'nodes with healthy volumes';
      case 'degraded': return 'nodes with degraded volumes';
      case 'failed': return 'nodes with failed volumes';
      case 'faulted': return 'nodes with faulted volumes';
      case 'rebuilding': return 'nodes with rebuilding replica activity';
      case 'local-nvme': return 'nodes with local NVMe volumes';
      default: return 'nodes';
    }
  };

  const getFilterSeverityInfo = (filter: VolumeFilter) => {
    switch (filter) {
      case 'failed':
        return {
          bgColor: 'bg-red-50',
          borderColor: 'border-red-200',
          textColor: 'text-red-900'
        };
      case 'degraded':
        return {
          bgColor: 'bg-yellow-50',
          borderColor: 'border-yellow-200',
          textColor: 'text-yellow-900'
        };
      case 'faulted':
        return {
          bgColor: 'bg-orange-50',
          borderColor: 'border-orange-200',
          textColor: 'text-orange-900'
        };
      case 'rebuilding':
        return {
          bgColor: 'bg-orange-50',
          borderColor: 'border-orange-200',
          textColor: 'text-orange-900'
        };
      case 'healthy':
        return {
          bgColor: 'bg-green-50',
          borderColor: 'border-green-200',
          textColor: 'text-green-900'
        };
      default:
        return {
          bgColor: 'bg-blue-50',
          borderColor: 'border-blue-200',
          textColor: 'text-blue-900'
        };
    }
  };

  const filteredVolumes = getFilteredVolumes();
  const filteredNodes = getFilteredNodes();
  const totalNodes = data.nodes.length;
  const severityInfo = getFilterSeverityInfo(activeFilter);

  return (
    <div>
      {activeFilter && activeFilter !== 'all' && (
        <div className={`mb-6 p-4 ${severityInfo.bgColor} border ${severityInfo.borderColor} rounded-lg`}>
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Filter className="w-4 h-4 text-blue-600" />
              <span className={`text-sm font-medium ${severityInfo.textColor}`}>
                Showing {getNodeFilterDescription(activeFilter)}
              </span>
            </div>
            {onClearFilter && (
              <button
                onClick={onClearFilter}
                className="text-blue-600 hover:text-blue-800 text-sm font-medium flex items-center gap-1"
              >
                <X className="w-3 h-3" />
                Clear Filter
              </button>
            )}
          </div>
          <div className="mt-2 text-sm text-blue-700">
            <div className="flex gap-4">
              <span>{filteredNodes.length} of {totalNodes} {getNodeFilterDescription(activeFilter)}</span>
              <span>•</span>
              <span>{filteredVolumes.length} volume{filteredVolumes.length !== 1 ? 's' : ''} match this filter</span>
            </div>
          </div>
        </div>
      )}

      {filteredNodes.length === 0 && activeFilter && activeFilter !== 'all' ? (
        <div className="text-center py-12">
          <div className="text-gray-500 mb-4">
            <Filter className="w-12 h-12 mx-auto mb-2 opacity-50" />
            <p className="text-lg font-medium">No nodes found</p>
            <p className="text-sm">No nodes have volumes matching the "{getFilterDisplayName(activeFilter)}" filter.</p>
            {filteredVolumes.length > 0 && (
              <p className="text-xs text-yellow-600 mt-2">
                Found {filteredVolumes.length} matching volume{filteredVolumes.length !== 1 ? 's' : ''}, 
                but {activeFilter === 'rebuilding' ? 'no rebuilding activity found on any nodes' : 
                     'no matching node conditions found'}.
              </p>
            )}
          </div>
          {onClearFilter && (
            <button
              onClick={onClearFilter}
              className="mt-4 px-4 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700"
            >
              Clear Filter to Show All Nodes
            </button>
          )}
        </div>
      ) : (
        <div className="space-y-6">
          {(activeFilter && activeFilter !== 'all' ? filteredNodes : data.nodes).map((node) => {
            const nodeDisks = data.disks.filter(d => d.node === node);
            const nodeVolumes = data.volumes.filter(v => v.nodes.includes(node));
            const healthyDisks = nodeDisks.filter(d => d.healthy).length;
            const totalCapacity = nodeDisks.reduce((sum, disk) => sum + disk.capacity_gb, 0);
            const totalAllocated = nodeDisks.reduce((sum, disk) => sum + disk.allocated_space, 0);
            const totalFree = totalCapacity - totalAllocated;
            
            return (
              <NodeDetailView 
                key={node} 
                node={node}
                nodeDisks={nodeDisks}
                nodeVolumes={nodeVolumes}
                healthyDisks={healthyDisks}
                totalCapacity={totalCapacity}
                totalAllocated={totalAllocated}
                totalFree={totalFree}
                volumeFilter={activeFilter}
                filteredVolumes={filteredVolumes.filter(v => {
                  // Include volumes that have replicas on this node matching the filter criteria
                  return v.nodes.includes(node);
                })}
                onDiskVolumeFilter={onDiskVolumeFilter}
              />
            );
          })}
        </div>
      )}
    </div>
  );
};
