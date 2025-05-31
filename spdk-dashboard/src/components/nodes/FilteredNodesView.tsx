import React from 'react';
import { Filter, X } from 'lucide-react';
import { NodeDetailView } from './NodeDetailView';
import type { DashboardData, VolumeFilter } from '../../hooks/useDashboardData';

interface FilteredNodesViewProps {
  data: DashboardData;
  activeFilter?: VolumeFilter;
  onClearFilter?: () => void;
}

export const FilteredNodesView: React.FC<FilteredNodesViewProps> = ({ 
  data, 
  activeFilter, 
  onClearFilter 
}) => {
  // Filter volumes based on the active filter
  const getFilteredVolumes = () => {
    if (!activeFilter || activeFilter === 'all') {
      return data.volumes;
    }

    switch (activeFilter) {
      case 'faulted':
        return data.volumes.filter(v => v.state === 'Degraded' || v.state === 'Failed');
      case 'rebuilding':
        return data.volumes.filter(v => v.state === 'Rebuilding');
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
        case 'faulted':
          // Only include nodes that have failed/degraded replicas for this volume
          volume.replica_statuses.forEach(replica => {
            if (replica.status === 'failed' || replica.status === 'degraded' || replica.status === 'Failed' || replica.status === 'Degraded') {
              relevantNodes.add(replica.node);
            }
          });
          break;
          
        case 'rebuilding':
          // Only include nodes that have rebuilding replicas for this volume
          volume.replica_statuses.forEach(replica => {
            if (replica.status === 'rebuilding' || replica.status === 'Rebuilding' || replica.rebuild_progress !== null) {
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
      case 'faulted': return 'Faulted Replicas';
      case 'rebuilding': return 'Rebuilding Replicas';
      case 'local-nvme': return 'Local NVMe Replicas';
      default: return 'All Volumes';
    }
  };

  const filteredVolumes = getFilteredVolumes();
  const filteredNodes = getFilteredNodes();
  const totalNodes = data.nodes.length;

  return (
    <div>
      {activeFilter && activeFilter !== 'all' && (
        <div className="mb-6 p-4 bg-blue-50 border border-blue-200 rounded-lg">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Filter className="w-4 h-4 text-blue-600" />
              <span className="text-sm font-medium text-blue-900">
                Showing nodes with: {getFilterDisplayName(activeFilter)}
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
              <span>{filteredNodes.length} of {totalNodes} nodes have {getFilterDisplayName(activeFilter).toLowerCase()}</span>
              <span>•</span>
              <span>{filteredVolumes.length} volume{filteredVolumes.length !== 1 ? 's' : ''} match this filter</span>
            </div>
          </div>
        </div>
      )}

      {/* Debug Information Panel */}
      {activeFilter && activeFilter !== 'all' && (
        <div className="mt-6 p-4 bg-yellow-50 rounded-lg border border-yellow-200 mb-6">
          <h5 className="font-medium text-yellow-800 mb-2">Debug Information</h5>
          <div className="text-sm text-yellow-700 space-y-1">
            <div>Active Filter: {activeFilter}</div>
            <div>Filtered Volumes: {filteredVolumes.length}</div>
            <div>Filtered Nodes: {filteredNodes.length}</div>
            {filteredVolumes.length > 0 && (
              <div>
                <div className="font-medium">Volume Details:</div>
                {filteredVolumes.slice(0, 2).map((vol, idx) => (
                  <div key={idx} className="ml-2 text-xs">
                    • {vol.name} (State: {vol.state}) - Replicas: {vol.replica_statuses.map(r => `${r.node}:${r.status}`).join(', ')}
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      )}

      {filteredNodes.length === 0 && activeFilter && activeFilter !== 'all' ? (
        <div className="text-center py-12">
          <div className="text-gray-500 mb-4">
            <Filter className="w-12 h-12 mx-auto mb-2 opacity-50" />
            <p className="text-lg font-medium">No nodes found</p>
            <p className="text-sm">No nodes have replicas matching the "{getFilterDisplayName(activeFilter)}" filter.</p>
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
                  // Only include volumes that have replicas matching the filter on this specific node
                  switch (activeFilter) {
                    case 'faulted':
                      return v.replica_statuses.some(r => r.node === node && (r.status === 'failed' || r.status === 'degraded' || r.status === 'Failed' || r.status === 'Degraded'));
                    case 'rebuilding':
                      return v.replica_statuses.some(r => r.node === node && (r.status === 'rebuilding' || r.status === 'Rebuilding' || r.rebuild_progress !== null));
                    case 'local-nvme':
                      return v.replica_statuses.some(r => r.node === node && r.is_local);
                    default:
                      return v.nodes.includes(node);
                  }
                })}
              />
            );
          })}
        </div>
      )}
    </div>
  );
};