import React, { useState } from 'react';
import { Server, HardDrive, Database, Zap, Activity, ChevronDown, ChevronRight, Plus, X } from 'lucide-react';
import type { Disk, Volume, VolumeFilter } from '../../hooks/useDashboardData';
import { useDiskSetup } from '../../hooks/useDashboardData';

interface NodeDetailViewProps {
  node: string;
  nodeDisks: Disk[];
  nodeVolumes: Volume[];
  healthyDisks: number;
  totalCapacity: number;
  totalAllocated: number;
  totalFree: number;
  volumeFilter?: VolumeFilter;
  filteredVolumes?: Volume[];
  onDiskVolumeFilter?: (diskId: string) => void;
  onShowMetrics: () => void;
}

export const NodeDetailView: React.FC<NodeDetailViewProps> = ({
  node,
  nodeDisks,
  nodeVolumes,
  healthyDisks,
  totalCapacity,
  totalAllocated,
  totalFree,
  volumeFilter,
  filteredVolumes,
  onDiskVolumeFilter,
  onShowMetrics,
}) => {
  const [expandedDisks, setExpandedDisks] = useState(new Set<string>());
  const [showMemoryDiskModal, setShowMemoryDiskModal] = useState(false);
  const [memoryDiskName, setMemoryDiskName] = useState('');
  const [memoryDiskSize, setMemoryDiskSize] = useState(1);
  const [isCreatingMemoryDisk, setIsCreatingMemoryDisk] = useState(false);
  const [memoryDiskError, setMemoryDiskError] = useState<string | null>(null);

  const { createMemoryDisk } = useDiskSetup();

  const toggleDiskExpansion = (diskId: string) => {
    const newExpanded = new Set(expandedDisks);
    if (newExpanded.has(diskId)) {
      newExpanded.delete(diskId);
    } else {
      newExpanded.add(diskId);
    }
    setExpandedDisks(newExpanded);
  };

  const handleCreateMemoryDisk = async () => {
    if (!memoryDiskName.trim()) {
      setMemoryDiskError('Please enter a name for the memory disk');
      return;
    }

    if (memoryDiskSize < 1 || memoryDiskSize > 64) {
      setMemoryDiskError('Size must be between 1 and 64 GB');
      return;
    }

    setIsCreatingMemoryDisk(true);
    setMemoryDiskError(null);

    try {
      const result = await createMemoryDisk(node, memoryDiskName, memoryDiskSize * 1024);

      if (result.success) {
        // Reset and close modal
        setShowMemoryDiskModal(false);
        setMemoryDiskName('');
        setMemoryDiskSize(1);
      } else {
        setMemoryDiskError(result.error || 'Failed to create memory disk');
      }
    } catch (err) {
      setMemoryDiskError(String(err));
    } finally {
      setIsCreatingMemoryDisk(false);
    }
  };

  // Use filtered volumes if provided, otherwise use all node volumes
  const displayVolumes = filteredVolumes || nodeVolumes;
  const filteredVolumeCount = filteredVolumes ? filteredVolumes.length : nodeVolumes.length;

  // Calculate filtered stats
  const filteredLocalNVMeCount = displayVolumes.filter(v => v.local_nvme).length;

  const getFilterDisplayName = (filter?: VolumeFilter) => {
    switch (filter) {
      case 'healthy': return 'healthy';
      case 'degraded': return 'degraded';
      case 'failed': return 'failed';
      case 'faulted': return 'faulted';
      case 'rebuilding': return 'rebuilding';
      case 'local-nvme': return 'local NVMe';
      default: return '';
    }
  };

  return (
    <div className="bg-gray-50 rounded-lg p-6">
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center">
          <div className="w-12 h-12 bg-blue-100 rounded-full flex items-center justify-center mr-4">
            <Server className="w-7 h-7 text-blue-600" />
          </div>
          <div>
            <h3 className="text-xl font-semibold">{node}</h3>
            <p className="text-sm text-gray-600">
              {totalCapacity}GB total • {totalAllocated}GB allocated • {totalFree}GB free
            </p>
            {volumeFilter && volumeFilter !== 'all' && filteredVolumes && (
              <p className="text-xs text-blue-600 mt-1">
                {filteredVolumeCount} {getFilterDisplayName(volumeFilter)} volume{filteredVolumeCount !== 1 ? 's' : ''} on this node
              </p>
            )}
          </div>
        </div>
        <div className="flex items-center gap-2">
          <span className="px-3 py-1 text-sm bg-green-100 text-green-800 rounded-full">
            Ready
          </span>
          <span className="px-3 py-1 text-sm bg-blue-100 text-blue-800 rounded-full">
            {nodeDisks.length} Disks
          </span>
          {volumeFilter && volumeFilter !== 'all' && (
            <span className="px-3 py-1 text-sm bg-purple-100 text-purple-800 rounded-full">
              {filteredVolumeCount} Filtered
            </span>
          )}
          
          {/* Node Metrics Button */}
          <button
            onClick={onShowMetrics}
            className="px-3 py-1 text-sm bg-indigo-600 text-white rounded-full hover:bg-indigo-700 transition-colors flex items-center gap-1"
            title="View detailed node metrics and SPDK status"
          >
            <Activity className="w-3 h-3" />
            SPDK Metrics
          </button>
          
          {/* Quick Access Button */}
          {volumeFilter && volumeFilter !== 'all' && filteredVolumes && filteredVolumes.length > 0 && (
            <button
              onClick={() => {
                const element = document.getElementById(`filtered-volumes-${node}`);
                element?.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
              }}
              className="px-3 py-1 text-sm bg-blue-600 text-white rounded-full hover:bg-blue-700 transition-colors flex items-center gap-1"
              title={`View ${filteredVolumeCount} ${getFilterDisplayName(volumeFilter)} volumes on this node`}
            >
              <Database className="w-3 h-3" />
              View Details
            </button>
          )}
        </div>
      </div>

      <div className="grid grid-cols-2 md:grid-cols-4 gap-4 mb-6">
        <div className="bg-white rounded-lg p-4">
          <div className="flex items-center">
            <HardDrive className="w-6 h-6 text-gray-500 mr-2" />
            <div>
              <p className="text-sm font-medium">NVMe Disks</p>
              <p className="text-lg font-bold">{healthyDisks}/{nodeDisks.length}</p>
              <p className="text-xs text-gray-500">healthy</p>
            </div>
          </div>
        </div>
        <div className="bg-white rounded-lg p-4">
          <div className="flex items-center">
            <Database className="w-6 h-6 text-gray-500 mr-2" />
            <div>
              <p className="text-sm font-medium">
                {volumeFilter && volumeFilter !== 'all' ? 'Filtered Volumes' : 'Volumes'}
              </p>
              <p className="text-lg font-bold">
                {filteredVolumeCount}
                {volumeFilter && volumeFilter !== 'all' && (
                  <span className="text-sm text-gray-500">/{nodeVolumes.length}</span>
                )}
              </p>
              <p className="text-xs text-gray-500">
                {volumeFilter && volumeFilter !== 'all' ? getFilterDisplayName(volumeFilter) : 'replicas'}
              </p>
            </div>
          </div>
        </div>
        <div className="bg-white rounded-lg p-4">
          <div className="flex items-center">
            <Zap className="w-6 h-6 text-gray-500 mr-2" />
            <div>
              <p className="text-sm font-medium">Local NVMe</p>
              <p className="text-lg font-bold">
                {filteredLocalNVMeCount}
                {volumeFilter && volumeFilter !== 'all' && volumeFilter !== 'local-nvme' && (
                  <span className="text-sm text-gray-500">/{nodeVolumes.filter(v => v.local_nvme).length}</span>
                )}
              </p>
              <p className="text-xs text-gray-500">high perf</p>
            </div>
          </div>
        </div>
        <div className="bg-white rounded-lg p-4">
          <div className="flex items-center">
            <Activity className="w-6 h-6 text-gray-500 mr-2" />
            <div>
              <p className="text-sm font-medium">Utilization</p>
              <p className="text-lg font-bold">{Math.round((totalAllocated / totalCapacity) * 100)}%</p>
              <p className="text-xs text-gray-500">capacity used</p>
            </div>
          </div>
        </div>
      </div>

      {/* Filtered Volumes Summary Section */}
      {volumeFilter && volumeFilter !== 'all' && filteredVolumes && filteredVolumes.length > 0 && (
        <div id={`filtered-volumes-${node}`} className="bg-white rounded-lg p-4 mb-6 border border-blue-200">
          <div className="flex items-center justify-between mb-3">
            <h4 className="text-lg font-semibold flex items-center gap-2">
              <Database className="w-5 h-5 text-blue-600" />
              {getFilterDisplayName(volumeFilter)} Volumes on {node}
              <span className="text-sm font-normal text-gray-600">
                ({filteredVolumes.length} volume{filteredVolumes.length !== 1 ? 's' : ''})
              </span>
            </h4>
          </div>
          
          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
            {filteredVolumes.map((volume) => {
              // Find this volume's replica on this specific node
              const nodeReplica = volume.replica_statuses.find(r => r.node === node);
              
              return (
                <div key={volume.id} className={`p-3 rounded-lg border-2 ${
                  volume.state === 'Healthy' ? 'border-green-200 bg-green-50' :
                  volume.state === 'Degraded' ? 'border-yellow-200 bg-yellow-50' :
                  'border-red-200 bg-red-50'
                }`}>
                  <div className="flex items-center justify-between mb-2">
                    <span className="font-medium text-gray-900">{volume.name}</span>
                    <span className="text-sm text-gray-600">{volume.size}</span>
                  </div>
                  
                  <div className="flex items-center justify-between mb-2">
                    <span className={`px-2 py-1 text-xs rounded-full ${
                      volume.state === 'Healthy' ? 'bg-green-100 text-green-700' :
                      volume.state === 'Degraded' ? 'bg-yellow-100 text-yellow-700' :
                      'bg-red-100 text-red-700'
                    }`}>
                      {volume.state}
                    </span>
                    
                    {nodeReplica && (
                      <span className={`px-2 py-1 text-xs rounded ${
                        nodeReplica.is_local 
                          ? 'bg-blue-100 text-blue-700' 
                          : 'bg-purple-100 text-purple-700'
                      }`}>
                        {nodeReplica.is_local ? 'Local NVMe' : 'NVMe-oF'}
                      </span>
                    )}
                  </div>
                  
                  <div className="text-xs text-gray-500 space-y-1">
                    <div>Replicas: {volume.active_replicas}/{volume.replicas}</div>
                    {nodeReplica && (
                      <div>Node Status: 
                        <span className={`ml-1 font-medium ${
                          nodeReplica.status === 'healthy' ? 'text-green-600' :
                          nodeReplica.status === 'rebuilding' ? 'text-orange-600' :
                          'text-red-600'
                        }`}>
                          {nodeReplica.status}
                        </span>
                      </div>
                    )}
                    
                    {nodeReplica?.rebuild_progress && (
                      <div className="mt-2">
                        <div className="flex justify-between text-xs mb-1">
                          <span>Rebuild Progress:</span>
                          <span>{nodeReplica.rebuild_progress}%</span>
                        </div>
                        <div className="w-full bg-gray-200 rounded-full h-1.5">
                          <div 
                            className="bg-orange-500 h-1.5 rounded-full transition-all duration-300" 
                            style={{ width: `${nodeReplica.rebuild_progress}%` }}
                          />
                        </div>
                      </div>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      )}

      <div className="bg-white rounded-lg overflow-hidden">
        <div className="px-6 py-4 bg-gray-100 border-b flex items-center justify-between">
          <h4 className="text-lg font-semibold flex items-center gap-2">
            <HardDrive className="w-5 h-5" />
            NVMe Disks & Logical Volume Stores on {node}
            {volumeFilter && volumeFilter !== 'all' && (
              <span className="text-sm font-normal text-gray-600">
                (showing disks with {getFilterDisplayName(volumeFilter)} volumes)
              </span>
            )}
          </h4>
          <button
            onClick={() => setShowMemoryDiskModal(true)}
            className="flex items-center gap-2 px-3 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700 transition-colors text-sm"
          >
            <Plus className="w-4 h-4" />
            Create Memory Disk
          </button>
        </div>
        <div className="overflow-x-auto">
          <table className="min-w-full divide-y divide-gray-200">
            <thead className="bg-gray-50">
              <tr>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Disk</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Model</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Capacity</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Allocation</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">LVS Status</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">LVS Initialized</th>
                <th className="px-4 py-3 text-left text-xs font-medium text-gray-500 uppercase">Logical Volumes</th>
              </tr>
            </thead>
            <tbody className="bg-white divide-y divide-gray-200">
              {nodeDisks.map((disk) => {
                // Filter disk's provisioned volumes based on the active filter
                const filteredDiskVolumes = volumeFilter && volumeFilter !== 'all' && filteredVolumes
                  ? disk.provisioned_volumes.filter(pv => 
                      filteredVolumes.some(fv => fv.id === pv.volume_id)
                    )
                  : disk.provisioned_volumes;

                return (
                  <React.Fragment key={disk.id}>
                    <tr className="hover:bg-gray-50">
                      <td className="px-4 py-4">
                        <div>
                          <button
                            onClick={() => onDiskVolumeFilter?.(disk.id)}
                            className="text-blue-600 hover:text-blue-800 hover:underline font-medium"
                            title={`Click to filter volumes on disk ${disk.id}`}
                          >
                            <div className="text-sm font-medium text-gray-900">{disk.id}</div>
                          </button>
                          <div className="text-xs text-gray-500">{disk.pci_addr}</div>
                        </div>
                      </td>
                      {/* Rest of the table cells remain the same */}
                      <td className="px-4 py-4 text-sm text-gray-700">{disk.model}</td>
                      <td className="px-4 py-4">
                        <div className="text-sm">
                          <div className="font-medium">{disk.capacity_gb}GB</div>
                          <div className="text-xs text-gray-500">{disk.free_space}GB free</div>
                        </div>
                      </td>
                      <td className="px-4 py-4">
                        <div className="text-sm">
                          <div className="flex items-center gap-2 mb-1">
                            <div className="w-20 bg-gray-200 rounded-full h-2">
                              <div 
                                className="bg-blue-500 h-2 rounded-full" 
                                style={{ width: `${(disk.allocated_space / disk.capacity) * 100}%` }}
                              />
                            </div>
                            <span className="text-xs text-gray-600">
                              {Math.round((disk.allocated_space / disk.capacity) * 100)}%
                            </span>
                          </div>
                          <div className="text-xs text-gray-500">
                            {Math.round(disk.allocated_space / (1024**3))}GB / {disk.capacity_gb}GB used
                          </div>
                        </div>
                      </td>
                      <td className="px-4 py-4">
                        <div className="space-y-1">
                          <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                            disk.healthy ? 'bg-green-100 text-green-800' : 'bg-red-100 text-red-800'
                          }`}>
                            {disk.healthy ? 'Healthy' : 'Unhealthy'}
                          </span>
                        </div>
                      </td>
                      <td className="px-4 py-4">
                        <div>
                          <span className={`inline-flex px-2 py-1 text-xs rounded-full ${
                            disk.blobstore_initialized ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
                          }`}>
                            {disk.blobstore_initialized ? 'Initialized' : 'Not Initialized'}
                          </span>
                          {disk.blobstore_initialized && (
                            <div className="text-xs text-gray-500 mt-1">
                              {disk.lvol_count} logical volumes
                            </div>
                          )}
                        </div>
                      </td>
                      <td className="px-4 py-4">
                        <div className="flex items-center gap-2">
                          <button
                            onClick={() => onDiskVolumeFilter?.(disk.id)}
                            className="text-blue-600 hover:text-blue-800 hover:underline text-sm"
                            title={`Click to see all volumes on disk ${disk.id}`}
                          >
                            {filteredDiskVolumes.length} volume{filteredDiskVolumes.length !== 1 ? 's' : ''}
                            {volumeFilter && volumeFilter !== 'all' && filteredDiskVolumes.length !== disk.provisioned_volumes.length && (
                              <span className="text-gray-400">/{disk.provisioned_volumes.length}</span>
                            )}
                          </button>
                          {filteredDiskVolumes.length > 0 && (
                            <button
                              onClick={() => toggleDiskExpansion(disk.id)}
                              className="p-1 text-gray-400 hover:text-gray-600 rounded"
                            >
                              {expandedDisks.has(disk.id) ? (
                                <ChevronDown className="w-4 h-4" />
                              ) : (
                                <ChevronRight className="w-4 h-4" />
                              )}
                            </button>
                          )}
                        </div>
                      </td>
                    </tr>
                    
                    {/* Rest of the expanded section remains the same */}
                    {expandedDisks.has(disk.id) && filteredDiskVolumes.length > 0 && (
                      <tr>
                        <td colSpan={7} className="px-4 py-2 bg-gray-50">
                          {/* Existing expanded content */}
                          <div className="space-y-3">
                            <h5 className="text-sm font-medium text-gray-700 flex items-center gap-2">
                              <Database className="w-4 h-4" />
                              {volumeFilter && volumeFilter !== 'all' 
                                ? `${getFilterDisplayName(volumeFilter)} volumes on ${disk.id}` 
                                : `Logical Volumes on ${disk.id}`
                              }
                              {volumeFilter && volumeFilter !== 'all' && filteredDiskVolumes.length !== disk.provisioned_volumes.length && (
                                <span className="text-xs text-gray-500">
                                  ({filteredDiskVolumes.length} of {disk.provisioned_volumes.length} shown)
                                </span>
                              )}
                            </h5>
                            <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                              {filteredDiskVolumes.map((volume, idx) => (
                                <div key={idx} className="p-3 bg-white rounded border border-gray-200 shadow-sm">
                                  <div className="flex items-center justify-between mb-2">
                                    <span className="font-medium text-gray-900">{volume.volume_name}</span>
                                    <div className="flex items-center gap-2">
                                      <span className="text-sm text-gray-600">{volume.size}GB</span>
                                      <span className={`px-2 py-1 text-xs rounded-full ${
                                        volume.status === 'healthy' ? 'bg-green-100 text-green-700' :
                                        volume.status === 'rebuilding' ? 'bg-orange-100 text-orange-700' :
                                        'bg-red-100 text-red-700'
                                      }`}>
                                        {volume.status}
                                      </span>
                                    </div>
                                  </div>
                                  
                                  <div className="flex items-center justify-between text-xs text-gray-500 mb-2">
                                    <span className={`px-2 py-1 rounded ${
                                      volume.replica_type === 'Local NVMe' 
                                        ? 'bg-blue-100 text-blue-700' 
                                        : 'bg-purple-100 text-purple-700'
                                    }`}>
                                      {volume.replica_type}
                                    </span>
                                    <span>
                                      {new Date(volume.provisioned_at).toLocaleDateString()}
                                    </span>
                                  </div>
                                  
                                  <div className="text-xs text-gray-400">
                                    Volume ID: {volume.volume_id}
                                  </div>
                                  <div className="text-xs text-gray-400">
                                    Provisioned {Math.floor((Date.now() - new Date(volume.provisioned_at).getTime()) / (1000 * 60 * 60 * 24))} days ago
                                  </div>
                                  
                                  {volume.replica_type === 'Local NVMe' && (
                                    <div className="mt-2 p-2 bg-blue-50 rounded text-xs">
                                      <div className="flex items-center gap-1 text-blue-700 mb-1">
                                        <Zap className="w-3 h-3" />
                                        <span className="font-medium">High Performance Path</span>
                                      </div>
                                      <div className="text-blue-600">
                                        Direct NVMe access • Zero network latency
                                      </div>
                                    </div>
                                  )}
                                </div>
                              ))}
                            </div>
                          </div>
                        </td>
                      </tr>
                    )}
                  </React.Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>

      {/* Memory Disk Creation Modal */}
      {showMemoryDiskModal && (
        <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
          <div className="bg-white rounded-lg shadow-xl p-6 max-w-md w-full mx-4">
            <div className="flex items-center justify-between mb-4">
              <h3 className="text-lg font-semibold">Create Memory Disk</h3>
              <button
                onClick={() => {
                  setShowMemoryDiskModal(false);
                  setMemoryDiskError(null);
                }}
                className="text-gray-400 hover:text-gray-600"
                disabled={isCreatingMemoryDisk}
              >
                <X className="w-5 h-5" />
              </button>
            </div>

            <div className="space-y-4">
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">
                  Disk Name
                </label>
                <input
                  type="text"
                  value={memoryDiskName}
                  onChange={(e) => setMemoryDiskName(e.target.value)}
                  placeholder="e.g., Malloc0, mem0"
                  className="w-full px-3 py-2 border border-gray-300 rounded-md focus:ring-blue-500 focus:border-blue-500"
                  disabled={isCreatingMemoryDisk}
                />
                <p className="text-xs text-gray-500 mt-1">
                  Name for the memory disk (will be used as SPDK bdev name)
                </p>
              </div>

              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">
                  Size (GB): {memoryDiskSize}
                </label>
                <input
                  type="range"
                  min="1"
                  max="64"
                  value={memoryDiskSize}
                  onChange={(e) => setMemoryDiskSize(parseInt(e.target.value))}
                  className="w-full"
                  disabled={isCreatingMemoryDisk}
                />
                <div className="flex justify-between text-xs text-gray-500 mt-1">
                  <span>1 GB</span>
                  <span>64 GB</span>
                </div>
                <p className="text-xs text-gray-500 mt-2">
                  Memory disks are volatile and data is lost on restart. Ideal for testing and caching.
                </p>
              </div>

              {memoryDiskError && (
                <div className="bg-red-50 border border-red-200 text-red-700 px-3 py-2 rounded text-sm">
                  {memoryDiskError}
                </div>
              )}

              <div className="flex gap-3 justify-end pt-4">
                <button
                  onClick={() => {
                    setShowMemoryDiskModal(false);
                    setMemoryDiskError(null);
                  }}
                  className="px-4 py-2 border border-gray-300 rounded-md text-gray-700 hover:bg-gray-50"
                  disabled={isCreatingMemoryDisk}
                >
                  Cancel
                </button>
                <button
                  onClick={handleCreateMemoryDisk}
                  className="px-4 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700 disabled:bg-gray-400 disabled:cursor-not-allowed flex items-center gap-2"
                  disabled={isCreatingMemoryDisk}
                >
                  {isCreatingMemoryDisk ? (
                    <>
                      <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin" />
                      Creating...
                    </>
                  ) : (
                    <>
                      <Plus className="w-4 h-4" />
                      Create
                    </>
                  )}
                </button>
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};