import React from 'react';
import { CheckCircle, X, Filter, HardDrive } from 'lucide-react';
import type { Volume, VolumeFilter, DiskFilter } from '../../hooks/useDashboardData';

interface VolumesTableProps {
  volumes: Volume[];
  activeFilter?: VolumeFilter;
  diskFilter?: DiskFilter;
  onClearFilter?: () => void;
  onClearDiskFilter?: () => void;
}

export const VolumesTable: React.FC<VolumesTableProps> = ({ 
  volumes, 
  activeFilter, 
  diskFilter,
  onClearFilter,
  onClearDiskFilter 
}) => {
  const filteredVolumes = React.useMemo(() => {
    let result = volumes;

    // Apply volume filter first
    if (activeFilter && activeFilter !== 'all') {
      switch (activeFilter) {
        case 'faulted':
          result = result.filter(v => v.state === 'Degraded' || v.state === 'Failed');
          break;
        case 'rebuilding':
          result = result.filter(v => v.state === 'Rebuilding');
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
      case 'faulted': return 'Faulted Volumes';
      case 'rebuilding': return 'Rebuilding Volumes';
      case 'local-nvme': return 'Local NVMe Volumes';
      default: return 'All Volumes';
    }
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
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Rebuild Progress</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Nodes</th>
              {diskFilter && (
                <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">On Disk</th>
              )}
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {filteredVolumes.length === 0 ? (
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
              filteredVolumes.map((volume) => (
                <tr key={volume.id} className="hover:bg-gray-50">
                  <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">{volume.name}</td>
                  <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{volume.size}</td>
                  <td className="px-6 py-4 whitespace-nowrap">
                    <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                      volume.state === 'Healthy' ? 'bg-green-100 text-green-800' :
                      volume.state === 'Rebuilding' ? 'bg-yellow-100 text-yellow-800' : 'bg-red-100 text-red-800'
                    }`}>
                      {volume.state}
                    </span>
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{volume.active_replicas}/{volume.replicas}</td>
                  <td className="px-6 py-4 whitespace-nowrap">
                    {volume.local_nvme ? (
                      <CheckCircle className="w-5 h-5 text-green-500" />
                    ) : (
                      <X className="w-5 h-5 text-gray-400" />
                    )}
                  </td>
                  <td className="px-6 py-4 whitespace-nowrap">
                    {volume.rebuild_progress ? (
                      <div className="flex items-center gap-2">
                        <div className="w-20 bg-gray-200 rounded-full h-2">
                          <div 
                            className="bg-blue-600 h-2 rounded-full" 
                            style={{ width: `${volume.rebuild_progress}%` }}
                          />
                        </div>
                        <span className="text-sm text-gray-600">{volume.rebuild_progress}%</span>
                      </div>
                    ) : (
                      <span className="text-gray-400">-</span>
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
              ))
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
};