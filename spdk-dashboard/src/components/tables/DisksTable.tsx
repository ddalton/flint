import React from 'react';
import type { Disk, Volume, VolumeFilter } from '../../hooks/useDashboardData';

interface DisksTableProps {
  disks: Disk[];
  volumes: Volume[]; // Add volumes to cross-reference
  stats: {
    totalDisks: number;
    healthyDisks: number;
    formattedDisks: number;
  };
  volumeFilter?: VolumeFilter;
  onDiskClick?: (diskId: string) => void;
}

export const DisksTable: React.FC<DisksTableProps> = ({ disks, volumes, stats, volumeFilter, onDiskClick }) => {
  // Filter disks based on volume filter
  const getFilteredDisks = () => {
    if (!volumeFilter || volumeFilter === 'all') {
      return disks;
    }

    return disks.filter(disk => {
      // Check if disk has any volumes matching the volume filter by cross-referencing with actual volume data
      return disk.provisioned_volumes.some(diskVolume => {
        // Find the actual volume data
        const actualVolume = volumes.find(v => v.id === diskVolume.volume_id);
        if (!actualVolume) return false;
        
        switch (volumeFilter) {
          case 'faulted':
            return actualVolume.state === 'Degraded' || actualVolume.state === 'Failed';
          case 'rebuilding':
            return actualVolume.state === 'Rebuilding';
          case 'local-nvme':
            return actualVolume.local_nvme;
          default:
            return true;
        }
      });
    });
  };

  const filteredDisks = getFilteredDisks();

  const getFilterDisplayName = (filter: VolumeFilter) => {
    switch (filter) {
      case 'faulted': return 'faulted volumes';
      case 'rebuilding': return 'rebuilding volumes';
      case 'local-nvme': return 'local NVMe volumes';
      default: return 'volumes';
    }
  };

  return (
    <div>
      {volumeFilter && volumeFilter !== 'all' && (
        <div className="mb-4 p-3 bg-blue-50 border border-blue-200 rounded-lg">
          <div className="text-sm font-medium text-blue-900">
            Showing disks with {getFilterDisplayName(volumeFilter)}
          </div>
          <div className="text-sm text-blue-700">
            {filteredDisks.length} of {disks.length} disks have {getFilterDisplayName(volumeFilter)}
          </div>
        </div>
      )}

      <div className="grid grid-cols-1 md:grid-cols-3 gap-4 mb-6">
        <div className="bg-gray-50 rounded-lg p-4">
          <h3 className="text-lg font-semibold">
            {volumeFilter && volumeFilter !== 'all' ? 'Filtered Disks' : 'Total Disks'}
          </h3>
          <p className="text-3xl font-bold text-blue-600">
            {filteredDisks.length}
            {volumeFilter && volumeFilter !== 'all' && (
              <span className="text-lg text-gray-500">/{stats.totalDisks}</span>
            )}
          </p>
        </div>
        <div className="bg-gray-50 rounded-lg p-4">
          <h3 className="text-lg font-semibold">Healthy Disks</h3>
          <p className="text-3xl font-bold text-green-600">
            {filteredDisks.filter(d => d.healthy).length}
          </p>
        </div>
        <div className="bg-gray-50 rounded-lg p-4">
          <h3 className="text-lg font-semibold">LVS Initialized</h3>
          <p className="text-3xl font-bold text-blue-600">
            {filteredDisks.filter(d => d.lvol_store_initialized).length}
          </p>
        </div>
      </div>
      
      <div className="overflow-x-auto">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Disk ID</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Node</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Model</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Capacity</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Free Space</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Status</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">LVS Initialized</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Performance</th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Volumes</th>
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {filteredDisks.length === 0 ? (
              <tr>
                <td colSpan={9} className="px-6 py-8 text-center text-gray-500">
                  {volumeFilter && volumeFilter !== 'all' 
                    ? `No disks have ${getFilterDisplayName(volumeFilter)}.`
                    : 'No disks found.'
                  }
                </td>
              </tr>
            ) : (
              filteredDisks.map((disk) => {
                const filteredVolumes = volumeFilter && volumeFilter !== 'all'
                  ? disk.provisioned_volumes.filter(diskVolume => {
                      // Find the actual volume data to check its state
                      const actualVolume = volumes.find(v => v.id === diskVolume.volume_id);
                      if (!actualVolume) return false;
                      
                      switch (volumeFilter) {
                        case 'faulted':
                          return actualVolume.state === 'Degraded' || actualVolume.state === 'Failed';
                        case 'rebuilding':
                          return actualVolume.state === 'Rebuilding';
                        case 'local-nvme':
                          return actualVolume.local_nvme;
                        default:
                          return true;
                      }
                    })
                  : disk.provisioned_volumes;

                return (
                  <tr key={disk.id} className="hover:bg-gray-50">
                    <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">
                      <button
                        onClick={() => onDiskClick?.(disk.id)}
                        className="text-blue-600 hover:text-blue-800 hover:underline font-medium"
                      >
                        {disk.id}
                      </button>
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.node}</td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.model}</td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.capacity_gb}GB</td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.free_space}GB</td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                        disk.healthy ? 'bg-green-100 text-green-800' : 'bg-red-100 text-red-800'
                      }`}>
                        {disk.healthy ? 'Healthy' : 'Unhealthy'}
                      </span>
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                        disk.lvol_store_initialized ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
                      }`}>
                        {disk.lvol_store_initialized ? 'Yes' : 'No'}
                      </span>
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                      <div>
                        <div>R: {disk.read_iops.toLocaleString()} IOPS</div>
                        <div>W: {disk.write_iops.toLocaleString()} IOPS</div>
                      </div>
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      <button
                        onClick={() => onDiskClick?.(disk.id)}
                        className="text-blue-600 hover:text-blue-800 hover:underline text-sm"
                      >
                        {filteredVolumes.length} volume{filteredVolumes.length !== 1 ? 's' : ''}
                        {volumeFilter && volumeFilter !== 'all' && filteredVolumes.length !== disk.provisioned_volumes.length && (
                          <span className="text-gray-400">/{disk.provisioned_volumes.length}</span>
                        )}
                      </button>
                    </td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>
    </div>
  );
};