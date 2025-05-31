import React from 'react';
import { CheckCircle, X, Filter, HardDrive, AlertTriangle, XCircle, Settings } from 'lucide-react';
import type { Volume, VolumeFilter, DiskFilter } from '../../hooks/useDashboardData';

interface VolumesTableProps {
  volumes: Volume[];
  activeFilter?: VolumeFilter;
  diskFilter?: DiskFilter;
  onClearFilter?: () => void;
  onClearDiskFilter?: () => void;
  onReplicaClick?: (volumeId: string, volumeName: string) => void;
}

export const VolumesTable: React.FC<VolumesTableProps> = ({ 
  volumes, 
  activeFilter, 
  diskFilter,
  onClearFilter,
  onClearDiskFilter,
  onReplicaClick
}) => {
  const filteredVolumes = React.useMemo(() => {
    let result = volumes;

    // Apply volume filter first
    if (activeFilter && activeFilter !== 'all') {
      switch (activeFilter) {
        case 'healthy':
          result = result.filter(v => v.state === 'Healthy');
          break;
        case 'degraded':
          result = result.filter(v => v.state === 'Degraded');
          break;
        case 'failed':
          result = result.filter(v => v.state === 'Failed');
          break;
        case 'faulted':
          result = result.filter(v => v.state === 'Degraded' || v.state === 'Failed');
          break;
        case 'rebuilding':
          result = result.filter(v => 
            v.replica_statuses.some(replica => 
              replica.status === 'rebuilding' || 
              replica.rebuild_progress !== null ||
              replica.is_new_replica
            )
          );
          break;
        case 'local-nvme':
          result = result.filter(v => v.local_nvme);
          break;
      }
    }

    // Apply disk filter if present
    if (diskFilter) {
      result = result.filter(volume => {
        // Check if any replica of this volume is on the specified disk
        // This would require matching volume IDs with disk's provisioned_volumes
        // For now, we'll use a simpler approach based on volume naming
        return volume.replica_statuses.some(replica => {
          // We need to find if this volume exists on the specified disk
          // This is a simplified check - in reality, we'd need to cross-reference
          // the volume ID with the disk's provisioned_volumes
          return true; // Placeholder - will be refined based on actual data structure
        });
      });
    }

    return result;
  }, [volumes, activeFilter, diskFilter]);

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

  const getVolumeStateInfo = (state: string) => {
    switch (state) {
      case 'Healthy':
        return {
          badge: 'bg-green-100 text-green-800',
          icon: CheckCircle,
          priority: 0
        };
      case 'Degraded':
        return {
          badge: 'bg-yellow-100 text-yellow-800',
          icon: AlertTriangle,
          priority: 2,
          tooltip: 'Volume has reduced redundancy but is still functional'
        };
      case 'Failed':
        return {
          badge: 'bg-red-100 text-red-800',
          icon: XCircle,
          priority: 3,
          tooltip: 'Volume has completely failed and requires immediate attention'
        };
      default:
        return {
          badge: 'bg-gray-100 text-gray-800',
          icon: X,
          priority: 4
        };
    }
  };

  // Sort volumes by state priority (Failed -> Degraded -> Healthy)
  const sortedVolumes = filteredVolumes.sort((a, b) => {
    const aInfo = getVolumeStateInfo(a.state);
    const bInfo = getVolumeStateInfo(b.state);
    return bInfo.priority - aInfo.priority; // Reverse sort for priority
  });

  // Check if volume has rebuilding activity
  const hasRebuildingActivity = (volume: Volume) => {
    return volume.replica_statuses.some(replica => 
      replica.status === 'rebuilding' || 
      replica.rebuild_progress !== null ||
      replica.is_new_replica
    );
  };

  return (
    <div>
      <div className="space-y-3 mb-4">
        {activeFilter && activeFilter !== 'all' && (
          <div className="p-3 bg-blue-50 border border-blue-200 rounded-lg flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Filter className="w-4 h-4 text-blue-600" />
              <span className="text-sm font-medium text-blue-900">
                Filtered by: {getFilterDisplayName(activeFilter)}
              </span>
              <span className="text-sm text-blue-700">
                ({filteredVolumes.length} of {volumes.length} volumes)
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
        )}

        {diskFilter && (
          <div className="p-3 bg-purple-50 border border-purple-200 rounded-lg flex items-center justify-between">
            <div className="flex items-center gap-2">
              <HardDrive className="w-4 h-4 text-purple-600" />
              <span className="text-sm font-medium text-purple-900">
                Showing volumes on disk: {diskFilter}
              </span>
              <span className="text-sm text-purple-700">
                ({filteredVolumes.length} volumes on this disk)
              </span>
            </div>
            {onClearDiskFilter && (
              <button
                onClick={onClearDiskFilter}
                className="text-purple-600 hover:text-purple-800 text-sm font-medium flex items-center gap-1"
              >
                <X className="w-3 h-3" />
                Clear Disk Filter
              </button>
            )}
          </div>
        )}
      </div>

      <div className="overflow-x-auto">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Volume Name</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Size</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">State</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Replicas</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Local NVMe</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Rebuild Activity</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Nodes</th>
              {diskFilter && (
                <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">On Disk</th>
              )}
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {sortedVolumes.length === 0 ? (
              <tr>
                <td colSpan={diskFilter ? 8 : 7} className="px-6 py-8 text-center text-gray-500">
                  {activeFilter && activeFilter !== 'all' 
                    ? `No volumes match the "${getFilterDisplayName(activeFilter)}" filter.`
                    : diskFilter
                    ? `No volumes found on disk "${diskFilter}".`
                    : 'No volumes found.'
                  }
                </td>
              </tr>
            ) : (
              sortedVolumes.map((volume) => {
                const stateInfo = getVolumeStateInfo(volume.state);
                const StateIcon = stateInfo.icon;
                const rebuildingActivity = hasRebuildingActivity(volume);
                const maxRebuildProgress = volume.replica_statuses
                  .filter(r => r.rebuild_progress !== null)
                  .reduce((max, r) => Math.max(max, r.rebuild_progress!), 0);

                return (
                  <tr key={volume.id} className="hover:bg-gray-50">
                    <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">{volume.name}</td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{volume.size}</td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      <div className="flex items-center gap-2">
                        <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${stateInfo.badge}`}>
                          <StateIcon className="w-3 h-3 mr-1" />
                          {volume.state}
                        </span>
                        {stateInfo.tooltip && (
                          <span className="text-xs text-gray-400" title={stateInfo.tooltip}>
                            ℹ️
                          </span>
                        )}
                      </div>
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                      <button
                        onClick={() => onReplicaClick?.(volume.id, volume.name)}
                        className="text-blue-600 hover:text-blue-800 hover:underline font-medium"
                        title={`Click to see disks with replicas for ${volume.name}`}
                      >
                        {volume.active_replicas}/{volume.replicas}
                      </button>
                      {volume.active_replicas < volume.replicas && (
                        <div className="text-xs text-red-600 mt-1">
                          {volume.replicas - volume.active_replicas} replica{volume.replicas - volume.active_replicas !== 1 ? 's' : ''} down
                        </div>
                      )}
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      {volume.local_nvme ? (
                        <div className="flex items-center gap-1 text-green-600">
                          <CheckCircle className="w-5 h-5" />
                          <span className="text-xs">High Perf</span>
                        </div>
                      ) : (
                        <X className="w-5 h-5 text-gray-400" />
                      )}
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      {rebuildingActivity ? (
                        <div className="flex items-center gap-2">
                          <Settings className="w-4 h-4 text-orange-600 animate-spin" />
                          {maxRebuildProgress > 0 ? (
                            <div className="flex items-center gap-2">
                              <div className="w-16 bg-gray-200 rounded-full h-2">
                                <div 
                                  className="bg-orange-600 h-2 rounded-full transition-all duration-300" 
                                  style={{ width: `${maxRebuildProgress}%` }}
                                />
                              </div>
                              <span className="text-xs text-orange-600 font-medium">{maxRebuildProgress}%</span>
                            </div>
                          ) : (
                            <span className="text-xs text-orange-600 font-medium">Active</span>
                          )}
                        </div>
                      ) : (
                        <span className="text-gray-400 text-sm">-</span>
                      )}
                    </td>
                    <td className="px-6 py-4">
                      <div className="flex flex-wrap gap-1">
                        {volume.nodes.map(node => (
                          <span key={node} className="inline-flex px-2 py-1 text-xs bg-gray-100 text-gray-800 rounded">
                            {node}
                          </span>
                        ))}
                      </div>
                    </td>
                    {diskFilter && (
                      <td className="px-6 py-4 whitespace-nowrap">
                        <span className="inline-flex px-2 py-1 text-xs bg-purple-100 text-purple-800 rounded-full">
                          {diskFilter}
                        </span>
                      </td>
                    )}
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>

      {/* Summary information */}
      {sortedVolumes.length > 0 && (
        <div className="mt-4 p-4 bg-gray-50 rounded-lg">
          <h4 className="text-sm font-medium text-gray-700 mb-2">Volume Summary</h4>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-xs">
            <div className="flex items-center gap-2">
              <CheckCircle className="w-4 h-4 text-green-600" />
              <span>{sortedVolumes.filter(v => v.state === 'Healthy').length} Healthy</span>
            </div>
            <div className="flex items-center gap-2">
              <AlertTriangle className="w-4 h-4 text-yellow-600" />
              <span>{sortedVolumes.filter(v => v.state === 'Degraded').length} Degraded</span>
            </div>
            <div className="flex items-center gap-2">
              <XCircle className="w-4 h-4 text-red-600" />
              <span>{sortedVolumes.filter(v => v.state === 'Failed').length} Failed</span>
            </div>
            <div className="flex items-center gap-2">
              <Settings className="w-4 h-4 text-orange-600" />
              <span>{sortedVolumes.filter(v => hasRebuildingActivity(v)).length} With Rebuilding</span>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};