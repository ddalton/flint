import React from 'react';
import { CheckCircle, X, Filter } from 'lucide-react';
import type { Volume, VolumeFilter } from '../../hooks/useDashboardData';

interface VolumesTableProps {
  volumes: Volume[];
  activeFilter?: VolumeFilter;
  onClearFilter?: () => void;
}

export const VolumesTable: React.FC<VolumesTableProps> = ({ volumes, activeFilter, onClearFilter }) => {
  const filteredVolumes = React.useMemo(() => {
    if (!activeFilter || activeFilter === 'all') {
      return volumes;
    }

    switch (activeFilter) {
      case 'faulted':
        return volumes.filter(v => v.state === 'Degraded' || v.state === 'Failed');
      case 'rebuilding':
        return volumes.filter(v => v.state === 'Rebuilding');
      case 'local-nvme':
        return volumes.filter(v => v.local_nvme);
      default:
        return volumes;
    }
  }, [volumes, activeFilter]);

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
      {activeFilter && activeFilter !== 'all' && (
        <div className="mb-4 p-3 bg-blue-50 border border-blue-200 rounded-lg flex items-center justify-between">
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
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {filteredVolumes.length === 0 ? (
              <tr>
                <td colSpan={7} className="px-6 py-8 text-center text-gray-500">
                  {activeFilter && activeFilter !== 'all' 
                    ? `No volumes match the "${getFilterDisplayName(activeFilter)}" filter.`
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
                </tr>
              ))
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
};