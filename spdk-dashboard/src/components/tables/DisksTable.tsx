import React, { useState, useMemo } from 'react';
import {
  X, HardDrive, Search, Filter, ChevronDown, SortAsc, SortDesc,
  Server, Database, CheckCircle, Activity, AlertTriangle, Trash2, Plus, Shield, Cloud,
  Zap, Link, AlertCircle, Unlock
} from 'lucide-react';
import type { Disk, Volume, VolumeFilter, VolumeReplicaFilter } from '../../hooks/useDashboardData';

interface DisksTableProps {
  disks: Disk[];
  volumes: Volume[];
  stats: {
    totalDisks: number;
    healthyDisks: number;
    formattedDisks: number;
  };
  volumeFilter?: VolumeFilter;
  volumeReplicaFilter?: VolumeReplicaFilter;
  onDiskClick?: (diskId: string) => void;
  onClearVolumeReplicaFilter?: () => void;
  onDiskVolumeFilter?: (diskId: string) => void;
  embedInSetup?: boolean; // when true, render NVMe-oF pane for Disk Setup (no pane switcher)
  showOnlyTable?: boolean; // when true, show only the table without any headers or actions (for RAID tab)
  statusCardType?: 'nvmeof' | 'raid' | 'default'; // controls which status cards to show
}

type DiskHealthFilter = 'all' | 'healthy' | 'unhealthy';
type DiskLVSFilter = 'all' | 'initialized' | 'uninitialized';
type DiskUtilizationFilter = 'all' | 'low' | 'medium' | 'high' | 'full';
type DiskSortField = 'id' | 'node' | 'capacity' | 'utilization' | 'free_space' | 'read_iops' | 'write_iops' | 'volumes';
type DiskSortOrder = 'asc' | 'desc';

type Pane = 'nvmeof' | 'raid';

export const DisksTable: React.FC<DisksTableProps> = ({ 
  disks, 
  volumes, 
  stats, 
  volumeFilter, 
  volumeReplicaFilter,
  onDiskClick,
  onClearVolumeReplicaFilter,
  onDiskVolumeFilter,
  embedInSetup = false,
  showOnlyTable = false,
  statusCardType = 'default'
}) => {
  const [activePane, setActivePane] = useState<Pane>('nvmeof');
  const [selectedDiskIds, setSelectedDiskIds] = useState<Set<string>>(new Set());
  const [showCreateRaid, setShowCreateRaid] = useState(false);
  const [raidName, setRaidName] = useState('');
  const [raidLevel, setRaidLevel] = useState('1');
  
  // Add Remote NVMe-oF state
  const [showAddRemoteNVMeoF, setShowAddRemoteNVMeoF] = useState(false);
  const [remoteNVMeoFForm, setRemoteNVMeoFForm] = useState({
    name: '',
    nqn: '',
    transport: 'tcp',
    address: '',
    port: '4420',
    nsid: '',
    authMethod: 'none',
    username: '',
    password: ''
  });
  const [raidNode, setRaidNode] = useState('');
  // Disk-specific filters
  const [searchTerm, setSearchTerm] = useState('');
  const [selectedNodes, setSelectedNodes] = useState<string[]>([]);
  const [healthFilter, setHealthFilter] = useState<DiskHealthFilter>('all');
  const [lvsFilter, setLVSFilter] = useState<DiskLVSFilter>('all');
  const [utilizationFilter, setUtilizationFilter] = useState<DiskUtilizationFilter>('all');
  const [capacityRange, setCapacityRange] = useState({ min: '', max: '' });
  const [sortField, setSortField] = useState<DiskSortField>('id');
  const [sortOrder, setSortOrder] = useState<DiskSortOrder>('asc');
  const [showFilters, setShowFilters] = useState(false);
  
  // NVMe-oF specific filters
  const [nvmeofTransportFilter, setNvmeofTransportFilter] = useState<'all' | 'tcp' | 'rdma' | 'fc'>('all');
  const [nvmeofLocationFilter, setNvmeofLocationFilter] = useState<'all' | 'local' | 'remote'>('all');
  const [nvmeofDriverFilter, setNvmeofDriverFilter] = useState<'all' | 'bound' | 'unbound'>('all');
  
  // RAID specific filters
  const [raidLevelFilter, setRaidLevelFilter] = useState<'all' | '0' | '1'>('all');
  const [raidStatusFilter, setRaidStatusFilter] = useState<'all' | 'healthy' | 'degraded' | 'failed'>('all');

  // Delete orphaned volume state
  const [showDeleteDialog, setShowDeleteDialog] = useState(false);
  const [volumeToDelete, setVolumeToDelete] = useState<{
    volume: any;
    diskNode: string;
  } | null>(null);
  const [deleteConfirmText, setDeleteConfirmText] = useState('');
  const [isDeleting, setIsDeleting] = useState(false);

  // Get unique nodes for filter dropdown
  const availableNodes = useMemo(() => {
    return Array.from(new Set(disks.map(disk => disk.node))).sort();
  }, [disks]);

  // Apply all filters
  const filteredDisks = useMemo(() => {
    let result = disks;

    // Apply volume replica filter first (most specific)
    if (volumeReplicaFilter) {
      result = result.filter(disk => {
        return disk.provisioned_volumes.some(diskVolume => 
          diskVolume.volume_id === volumeReplicaFilter
        );
      });
    }
    // Apply general volume filter
    else if (volumeFilter && volumeFilter !== 'all') {
      result = result.filter(disk => {
        if (disk.provisioned_volumes.length === 0) {
          return true; // Show empty disks
        }
        
        return disk.provisioned_volumes.some(diskVolume => {
          const actualVolume = volumes.find(v => v.id === diskVolume.volume_id);
          if (!actualVolume) return false;
          
          switch (volumeFilter) {
            case 'healthy':
              return actualVolume.state === 'Healthy';
            case 'degraded':
              return actualVolume.state === 'Degraded';
            case 'failed':
              return actualVolume.state === 'Failed';
            case 'faulted':
              return actualVolume.state === 'Degraded' || actualVolume.state === 'Failed';
            case 'rebuilding':
              return actualVolume.replica_statuses.some(replica => 
                replica.status === 'rebuilding' || 
                replica.rebuild_progress !== null ||
                replica.is_new_replica
              );
            case 'local-nvme':
              return actualVolume.local_nvme;
            default:
              return true;
          }
        });
      });
    }

    // Apply search filter
    if (searchTerm) {
      const searchLower = searchTerm.toLowerCase();
      result = result.filter(disk => 
        disk.id.toLowerCase().includes(searchLower) ||
        disk.node.toLowerCase().includes(searchLower) ||
        disk.model.toLowerCase().includes(searchLower) ||
        disk.pci_addr.toLowerCase().includes(searchLower)
      );
    }

    // Apply node filter
    if (selectedNodes.length > 0) {
      result = result.filter(disk => selectedNodes.includes(disk.node));
    }

    // Apply health filter
    if (healthFilter !== 'all') {
      result = result.filter(disk => 
        healthFilter === 'healthy' ? disk.healthy : !disk.healthy
      );
    }

    // Apply LVS filter
    if (lvsFilter !== 'all') {
      result = result.filter(disk => 
        lvsFilter === 'initialized' ? disk.blobstore_initialized : !disk.blobstore_initialized
      );
    }

    // Apply utilization filter
    if (utilizationFilter !== 'all') {
      result = result.filter(disk => {
        const utilization = (disk.allocated_space / disk.capacity_gb) * 100;
        switch (utilizationFilter) {
          case 'low': return utilization < 25;
          case 'medium': return utilization >= 25 && utilization < 75;
          case 'high': return utilization >= 75 && utilization < 95;
          case 'full': return utilization >= 95;
          default: return true;
        }
      });
    }

    // Apply NVMe-oF specific filters (when in nvmeof context)
    if (statusCardType === 'nvmeof') {
      if (nvmeofLocationFilter !== 'all') {
        result = result.filter(disk => 
          nvmeofLocationFilter === 'remote' ? disk.is_remote : !disk.is_remote
        );
      }
      
      if (nvmeofDriverFilter !== 'all') {
        result = result.filter(disk => 
          nvmeofDriverFilter === 'bound' ? disk.blobstore_initialized : !disk.blobstore_initialized
        );
      }
      
      if (nvmeofTransportFilter !== 'all') {
        result = result.filter(disk => {
          if (!disk.nvmeof_endpoint) return false;
          const transport = typeof disk.nvmeof_endpoint === 'string' ? 'tcp' : 
                           disk.nvmeof_endpoint && typeof disk.nvmeof_endpoint === 'object' ? 
                           (disk.nvmeof_endpoint as any).transport : 'tcp';
          return transport === nvmeofTransportFilter;
        }
        );
      }
    }

    // Apply RAID specific filters (when in raid context)
    if (statusCardType === 'raid') {
      if (raidLevelFilter !== 'all') {
        // Assuming raid level is stored in disk metadata - adjust as needed
        result = result.filter(disk => 
          String(disk.raid_level) === raidLevelFilter
        );
      }
      
      if (raidStatusFilter !== 'all') {
        result = result.filter(disk => {
          switch (raidStatusFilter) {
            case 'healthy': return disk.healthy && disk.blobstore_initialized;
            case 'degraded': return disk.healthy && !disk.blobstore_initialized;
            case 'failed': return !disk.healthy;
            default: return true;
          }
        });
      }
    }

    // Apply capacity range filter
    if (capacityRange.min || capacityRange.max) {
      result = result.filter(disk => {
        const capacity = disk.capacity_gb;
        const min = capacityRange.min ? parseInt(capacityRange.min) : 0;
        const max = capacityRange.max ? parseInt(capacityRange.max) : Infinity;
        return capacity >= min && capacity <= max;
      });
    }

    // Apply sorting
    result.sort((a, b) => {
      let aValue: any, bValue: any;
      
      switch (sortField) {
        case 'id':
          aValue = a.id;
          bValue = b.id;
          break;
        case 'node':
          aValue = a.node;
          bValue = b.node;
          break;
        case 'capacity':
          aValue = a.capacity_gb;
          bValue = b.capacity_gb;
          break;
        case 'utilization':
          aValue = (a.allocated_space / a.capacity_gb) * 100;
          bValue = (b.allocated_space / b.capacity_gb) * 100;
          break;
        case 'free_space':
          aValue = a.free_space;
          bValue = b.free_space;
          break;
        case 'read_iops':
          aValue = a.read_iops;
          bValue = b.read_iops;
          break;
        case 'write_iops':
          aValue = a.write_iops;
          bValue = b.write_iops;
          break;
        case 'volumes':
          aValue = a.provisioned_volumes.length;
          bValue = b.provisioned_volumes.length;
          break;
        default:
          aValue = a.id;
          bValue = b.id;
      }

      if (typeof aValue === 'string') {
        return sortOrder === 'asc' 
          ? aValue.localeCompare(bValue)
          : bValue.localeCompare(aValue);
      } else {
        return sortOrder === 'asc' ? aValue - bValue : bValue - aValue;
      }
    });

    return result;
  }, [
    disks, volumes, volumeFilter, volumeReplicaFilter, searchTerm, selectedNodes,
    healthFilter, lvsFilter, utilizationFilter, capacityRange, sortField, sortOrder,
    statusCardType, nvmeofTransportFilter, nvmeofLocationFilter, nvmeofDriverFilter,
    raidLevelFilter, raidStatusFilter
  ]);

  const handleSort = (field: DiskSortField) => {
    if (sortField === field) {
      setSortOrder(sortOrder === 'asc' ? 'desc' : 'asc');
    } else {
      setSortField(field);
      setSortOrder('asc');
    }
  };

  const clearAllFilters = () => {
    setSearchTerm('');
    setSelectedNodes([]);
    setHealthFilter('all');
    setLVSFilter('all');
    setUtilizationFilter('all');
    setCapacityRange({ min: '', max: '' });
    setSortField('id');
    setSortOrder('asc');
  };

  const getActiveFilterCount = () => {
    let count = 0;
    if (searchTerm) count++;
    if (selectedNodes.length > 0) count++;
    if (healthFilter !== 'all') count++;
    if (lvsFilter !== 'all') count++;
    if (utilizationFilter !== 'all') count++;
    if (capacityRange.min || capacityRange.max) count++;
    
    // Add context-specific filters
    if (statusCardType === 'nvmeof') {
      if (nvmeofTransportFilter !== 'all') count++;
      if (nvmeofLocationFilter !== 'all') count++;
      if (nvmeofDriverFilter !== 'all') count++;
    }
    
    if (statusCardType === 'raid') {
      if (raidLevelFilter !== 'all') count++;
      if (raidStatusFilter !== 'all') count++;
    }
    
    return count;
  };

  const targetVolume = volumeReplicaFilter ? volumes.find(v => v.id === volumeReplicaFilter) : null;
  const activeFilterCount = getActiveFilterCount();

  const getFilterDisplayName = (filter: VolumeFilter) => {
    switch (filter) {
      case 'faulted': return 'faulted volumes';
      case 'rebuilding': return 'rebuilding volumes';
      case 'local-nvme': return 'local NVMe volumes';
      default: return 'volumes';
    }
  };

  const SortIcon = ({ field }: { field: DiskSortField }) => {
    if (sortField !== field) return null;
    return sortOrder === 'asc' ? 
      <SortAsc className="w-4 h-4" /> : 
      <SortDesc className="w-4 h-4" />;
  };

  // Delete orphaned volume handlers
  const handleDeleteOrphanedVolume = (volume: any, diskNode: string) => {
    setVolumeToDelete({ volume, diskNode });
    setDeleteConfirmText('');
    setShowDeleteDialog(true);
  };

  const confirmDeleteOrphanedVolume = async () => {
    if (!volumeToDelete || deleteConfirmText !== 'DELETE') {
      return;
    }

    setIsDeleting(true);
    try {
      const response = await fetch('/api/volumes/orphaned', {
        method: 'DELETE',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          node: volumeToDelete.diskNode,
          volume_name: volumeToDelete.volume.spdk_volume_name,
          volume_uuid: volumeToDelete.volume.spdk_volume_uuid,
          reason: `User deleted orphaned volume via dashboard`
        })
      });

      const result = await response.json();
      
      if (result.success) {
        console.log('✅ Successfully deleted orphaned volume:', result.deleted_volume);
        // Reset dialog state
        setShowDeleteDialog(false);
        setVolumeToDelete(null);
        setDeleteConfirmText('');
        
        // TODO: Refresh dashboard data to show the change
        // For now, user can refresh the page manually
        alert(`Successfully deleted orphaned volume '${volumeToDelete.volume.spdk_volume_name}' (${result.deleted_volume?.size_gb?.toFixed(2)}GB)`);
      } else {
        console.error('❌ Failed to delete orphaned volume:', result.message);
        alert(`Failed to delete orphaned volume: ${result.message}`);
      }
    } catch (error) {
      console.error('❌ Error deleting orphaned volume:', error);
      alert(`Error deleting orphaned volume: ${error}`);
    } finally {
      setIsDeleting(false);
    }
  };

  const cancelDeleteOrphanedVolume = () => {
    setShowDeleteDialog(false);
    setVolumeToDelete(null);
    setDeleteConfirmText('');
  };

  return (
    <div>
      {/* Header / Actions - only show when not in showOnlyTable mode */}
      {!showOnlyTable && (
        <div className="mb-4 flex items-center justify-between">
          {!embedInSetup && (
            <div className="flex items-center gap-2">
              <span className="text-sm text-gray-600">View:</span>
              <div className="inline-flex rounded-md border border-gray-300 overflow-hidden">
                <button
                  onClick={() => setActivePane('nvmeof')}
                  className={`px-3 py-1 text-sm ${activePane === 'nvmeof' ? 'bg-blue-600 text-white' : 'bg-white text-gray-700'}`}
                >
                  NVMe-oF Disks
                </button>
                <button
                  onClick={() => setActivePane('raid')}
                  className={`px-3 py-1 text-sm ${activePane === 'raid' ? 'bg-blue-600 text-white' : 'bg-white text-gray-700'}`}
                >
                  RAID Disks
                </button>
              </div>
            </div>
          )}
          <div className="flex items-center gap-2">
            {(statusCardType === 'nvmeof' || statusCardType === 'default') && (
              <button
                onClick={() => setShowAddRemoteNVMeoF(true)}
                className="inline-flex items-center gap-2 px-4 py-2 text-sm bg-gradient-to-r from-indigo-500 to-purple-600 text-white border-0 rounded-lg shadow-lg hover:shadow-xl hover:from-indigo-600 hover:to-purple-700 transform hover:scale-105 transition-all duration-200 font-medium"
                title="Add remote NVMe-oF disk"
              >
                <Cloud className="w-4 h-4" /> Add Remote NVMe-oF
              </button>
            )}
            {(statusCardType === 'raid' || statusCardType === 'default') && (
              <button
                onClick={() => setShowCreateRaid(true)}
                className="inline-flex items-center gap-2 px-4 py-2 text-sm bg-blue-600 text-white rounded-lg shadow-lg hover:shadow-xl hover:bg-blue-700 transform hover:scale-105 transition-all duration-200 font-medium"
                title="Create RAID disk from selected NVMe-oF local disks"
              >
                <Plus className="w-4 h-4" /> Create RAID Disk
              </button>
            )}
          </div>
        </div>
      )}
      {/* Volume-based filters (existing) */}
      {volumeFilter && volumeFilter !== 'all' && !volumeReplicaFilter && (
        <div className="mb-4 p-3 bg-blue-50 border border-blue-200 rounded-lg">
          <div className="text-sm font-medium text-blue-900">
            Showing disks with {getFilterDisplayName(volumeFilter)}
          </div>
          <div className="text-sm text-blue-700">
            {filteredDisks.length} of {disks.length} disks have {getFilterDisplayName(volumeFilter)}
          </div>
        </div>
      )}

      {volumeReplicaFilter && targetVolume && (
        <div className="mb-4 p-3 bg-green-50 border border-green-200 rounded-lg flex items-center justify-between">
          <div className="flex items-center gap-2">
            <HardDrive className="w-4 h-4 text-green-600" />
            <span className="text-sm font-medium text-green-900">
              Showing disks with replicas for volume: {targetVolume.name}
            </span>
            <span className="text-sm text-green-700">
              ({filteredDisks.length} disk{filteredDisks.length !== 1 ? 's' : ''} contain{filteredDisks.length === 1 ? 's' : ''} replicas)
            </span>
          </div>
          {onClearVolumeReplicaFilter && (
            <button
              onClick={onClearVolumeReplicaFilter}
              className="text-green-600 hover:text-green-800 text-sm font-medium flex items-center gap-1"
            >
              <X className="w-3 h-3" />
              Clear Volume Filter
            </button>
          )}
        </div>
      )}

      {/* Enhanced Disk Filtering Controls */}
      <div className="mb-6 bg-white border border-gray-200 rounded-lg shadow-sm">
        {/* Filter Header */}
        <div className="px-4 py-3 border-b border-gray-200 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <Filter className="w-5 h-5 text-gray-600" />
            <h3 className="text-lg font-medium text-gray-900">Disk Filters</h3>
            {activeFilterCount > 0 && (
              <span className="px-2 py-1 text-xs bg-blue-100 text-blue-800 rounded-full">
                {activeFilterCount} active
              </span>
            )}
          </div>
          <div className="flex items-center gap-2">
            {activeFilterCount > 0 && (
              <button
                onClick={clearAllFilters}
                className="text-sm text-gray-600 hover:text-gray-800 flex items-center gap-1"
              >
                <X className="w-4 h-4" />
                Clear All
              </button>
            )}
            <button
              onClick={() => setShowFilters(!showFilters)}
              className="flex items-center gap-1 text-sm text-gray-600 hover:text-gray-800"
            >
              <ChevronDown className={`w-4 h-4 transition-transform ${showFilters ? 'rotate-180' : ''}`} />
              {showFilters ? 'Hide' : 'Show'} Filters
            </button>
          </div>
        </div>

        {/* Search Bar (always visible) */}
        <div className="px-4 py-3 bg-gray-50">
          <div className="relative">
            <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
            <input
              type="text"
              placeholder="Search disks by ID, node, model, or PCI address..."
              value={searchTerm}
              onChange={(e) => setSearchTerm(e.target.value)}
              className="w-full pl-10 pr-4 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-blue-500"
            />
          </div>
        </div>

        {/* Advanced Filters (collapsible) */}
        {showFilters && (
          <div className="px-4 py-4 border-t border-gray-200 space-y-4">
            <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
              {/* Node Filter */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">
                  Nodes ({selectedNodes.length} selected)
                </label>
                <div className="space-y-1 max-h-32 overflow-y-auto border border-gray-300 rounded p-2">
                  {availableNodes.map(node => (
                    <label key={node} className="flex items-center text-sm">
                      <input
                        type="checkbox"
                        checked={selectedNodes.includes(node)}
                        onChange={(e) => {
                          if (e.target.checked) {
                            setSelectedNodes([...selectedNodes, node]);
                          } else {
                            setSelectedNodes(selectedNodes.filter(n => n !== node));
                          }
                        }}
                        className="mr-2 rounded"
                      />
                      {node}
                    </label>
                  ))}
                </div>
              </div>

              {/* Common Filters for all types */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">Health Status</label>
                <select
                  value={healthFilter}
                  onChange={(e) => setHealthFilter(e.target.value as DiskHealthFilter)}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="all">All Disks</option>
                  <option value="healthy">Healthy Only</option>
                  <option value="unhealthy">Unhealthy Only</option>
                </select>
              </div>

              {/* Context-specific filters */}
              {statusCardType === 'nvmeof' && (
                <>
                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">Location</label>
                    <select
                      value={nvmeofLocationFilter}
                      onChange={(e) => setNvmeofLocationFilter(e.target.value as 'all' | 'local' | 'remote')}
                      className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                    >
                      <option value="all">All Locations</option>
                      <option value="local">Local Disks</option>
                      <option value="remote">Remote Disks</option>
                    </select>
                  </div>

                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">Driver Status</label>
                    <select
                      value={nvmeofDriverFilter}
                      onChange={(e) => setNvmeofDriverFilter(e.target.value as 'all' | 'bound' | 'unbound')}
                      className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                    >
                      <option value="all">All Drivers</option>
                      <option value="bound">Driver Bound</option>
                      <option value="unbound">Driver Unbound</option>
                    </select>
                  </div>

                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">Transport</label>
                    <select
                      value={nvmeofTransportFilter}
                      onChange={(e) => setNvmeofTransportFilter(e.target.value as 'all' | 'tcp' | 'rdma' | 'fc')}
                      className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                    >
                      <option value="all">All Transports</option>
                      <option value="tcp">TCP</option>
                      <option value="rdma">RDMA</option>
                      <option value="fc">Fibre Channel</option>
                    </select>
                  </div>
                </>
              )}

              {statusCardType === 'raid' && (
                <>
                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">RAID Level</label>
                    <select
                      value={raidLevelFilter}
                      onChange={(e) => setRaidLevelFilter(e.target.value as 'all' | '0' | '1')}
                      className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                    >
                      <option value="all">All RAID Levels</option>
                      <option value="0">RAID-0 (Stripe)</option>
                      <option value="1">RAID-1 (Mirror)</option>
                    </select>
                  </div>

                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">RAID Status</label>
                    <select
                      value={raidStatusFilter}
                      onChange={(e) => setRaidStatusFilter(e.target.value as 'all' | 'healthy' | 'degraded' | 'failed')}
                      className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                    >
                      <option value="all">All Statuses</option>
                      <option value="healthy">Healthy</option>
                      <option value="degraded">Degraded</option>
                      <option value="failed">Failed</option>
                    </select>
                  </div>
                </>
              )}

              {statusCardType === 'default' && (
                <>
                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">LVS Status</label>
                    <select
                      value={lvsFilter}
                      onChange={(e) => setLVSFilter(e.target.value as DiskLVSFilter)}
                      className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                    >
                      <option value="all">All Disks</option>
                      <option value="initialized">Initialized Only</option>
                      <option value="uninitialized">Uninitialized Only</option>
                    </select>
                  </div>

                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-2">Utilization</label>
                    <select
                      value={utilizationFilter}
                      onChange={(e) => setUtilizationFilter(e.target.value as DiskUtilizationFilter)}
                      className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                    >
                      <option value="all">All Levels</option>
                      <option value="low">Low (&lt; 25%)</option>
                      <option value="medium">Medium (25-75%)</option>
                      <option value="high">High (75-95%)</option>
                      <option value="full">Nearly Full (&gt; 95%)</option>
                    </select>
                  </div>
                </>
              )}

              {/* Capacity Range */}
              <div className="md:col-span-2">
                <label className="block text-sm font-medium text-gray-700 mb-2">Capacity Range (GB)</label>
                <div className="flex items-center gap-2">
                  <input
                    type="number"
                    placeholder="Min"
                    value={capacityRange.min}
                    onChange={(e) => setCapacityRange({ ...capacityRange, min: e.target.value })}
                    className="flex-1 border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                  />
                  <span className="text-gray-500">to</span>
                  <input
                    type="number"
                    placeholder="Max"
                    value={capacityRange.max}
                    onChange={(e) => setCapacityRange({ ...capacityRange, max: e.target.value })}
                    className="flex-1 border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                  />
                </div>
              </div>
            </div>
          </div>
        )}
      </div>

      {/* Status Cards - conditional based on pane type */}
      {!showOnlyTable && (
        <div className="grid grid-cols-2 md:grid-cols-4 gap-3 mb-6">
          {statusCardType === 'nvmeof' && (
            <>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <Unlock className="w-6 h-6 text-green-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">Free</h3>
                    <p className="text-xl font-bold text-green-600">
                      {filteredDisks.filter(d => !d.is_system_disk && !d.blobstore_initialized).length}
                    </p>
                  </div>
                </div>
              </div>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <Zap className="w-6 h-6 text-blue-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">Driver Bound</h3>
                    <p className="text-xl font-bold text-blue-600">
                      {filteredDisks.filter(d => d.blobstore_initialized).length}
                    </p>
                  </div>
                </div>
              </div>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <AlertCircle className="w-6 h-6 text-orange-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">Needs Unmount</h3>
                    <p className="text-xl font-bold text-orange-600">
                      {filteredDisks.filter(d => d.mounted).length}
                    </p>
                  </div>
                </div>
              </div>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <Cloud className="w-6 h-6 text-purple-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">Remote</h3>
                    <p className="text-xl font-bold text-purple-600">
                      {filteredDisks.filter(d => d.is_remote).length}
                    </p>
                  </div>
                </div>
              </div>
            </>
          )}
          {statusCardType === 'raid' && (
            <>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <Unlock className="w-6 h-6 text-green-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">Free</h3>
                    <p className="text-xl font-bold text-green-600">
                      {filteredDisks.filter(d => !d.blobstore_initialized).length}
                    </p>
                  </div>
                </div>
              </div>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <Database className="w-6 h-6 text-indigo-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">LVS Ready</h3>
                    <p className="text-xl font-bold text-indigo-600">
                      {filteredDisks.filter(d => d.blobstore_initialized).length}
                    </p>
                  </div>
                </div>
              </div>
            </>
          )}
          {statusCardType === 'default' && (
            <>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <HardDrive className="w-6 h-6 text-blue-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">
                      {activeFilterCount > 0 || volumeFilter || volumeReplicaFilter ? 'Filtered Disks' : 'Total Disks'}
                    </h3>
                    <p className="text-xl font-bold text-blue-600">
                      {filteredDisks.length}
                      {(activeFilterCount > 0 || volumeFilter || volumeReplicaFilter) && (
                        <span className="text-sm text-gray-500">/{stats.totalDisks}</span>
                      )}
                    </p>
                  </div>
                </div>
              </div>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <CheckCircle className="w-6 h-6 text-green-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">Healthy Disks</h3>
                    <p className="text-xl font-bold text-green-600">
                      {filteredDisks.filter(d => d.healthy).length}
                    </p>
                  </div>
                </div>
              </div>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <Database className="w-6 h-6 text-indigo-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">LVS Initialized</h3>
                    <p className="text-xl font-bold text-indigo-600">
                      {filteredDisks.filter(d => d.blobstore_initialized).length}
                    </p>
                  </div>
                </div>
              </div>
              <div className="bg-gray-50 rounded-lg p-3">
                <div className="flex items-center">
                  <Activity className="w-6 h-6 text-purple-600 mr-2" />
                  <div>
                    <h3 className="text-sm font-semibold">Avg Utilization</h3>
                    <p className="text-xl font-bold text-purple-600">
                      {filteredDisks.length > 0 ? 
                        Math.round(filteredDisks.reduce((sum, disk) => 
                          sum + (disk.allocated_space / disk.capacity_gb) * 100, 0
                        ) / filteredDisks.length) : 0}%
                    </p>
                  </div>
                </div>
              </div>
            </>
          )}
        </div>
      )}
      
      {/* Disks Table */}
      <div className="overflow-x-auto">
        <table className="min-w-full divide-y divide-gray-200">
          <thead className="bg-gray-50">
            <tr>
              {!showOnlyTable && (
                <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                  Select
                </th>
              )}
              <th 
                className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-100"
                onClick={() => handleSort('id')}
              >
                <div className="flex items-center gap-1">
                  Disk ID
                  <SortIcon field="id" />
                </div>
              </th>
              <th 
                className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-100"
                onClick={() => handleSort('node')}
              >
                <div className="flex items-center gap-1">
                  Node
                  <SortIcon field="node" />
                </div>
              </th>
              
              {/* Contextual columns based on disk type */}
              {statusCardType === 'nvmeof' && (
                <>
                  <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Model</th>
                  <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Transport</th>
                  <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Location</th>
                </>
              )}
              
              {statusCardType === 'raid' && (
                <>
                  <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">RAID Level</th>
                  <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Members</th>
                </>
              )}
              
              {statusCardType === 'default' && (
                <>
                  <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Model</th>
                </>
              )}
              
              <th 
                className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-100"
                onClick={() => handleSort('capacity')}
              >
                <div className="flex items-center gap-1">
                  Capacity
                  <SortIcon field="capacity" />
                </div>
              </th>
              <th 
                className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-100"
                onClick={() => handleSort('free_space')}
              >
                <div className="flex items-center gap-1">
                  Free Space
                  <SortIcon field="free_space" />
                </div>
              </th>
              <th 
                className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-100"
                onClick={() => handleSort('utilization')}
              >
                <div className="flex items-center gap-1">
                  Utilization
                  <SortIcon field="utilization" />
                </div>
              </th>
              <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Status</th>
              
              {/* Context-specific rightmost columns */}
              {statusCardType === 'nvmeof' && (
                <>
                  <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">RAID Membership</th>
                  <th 
                    className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-100"
                    onClick={() => handleSort('read_iops')}
                  >
                    <div className="flex items-center gap-1">
                      Performance
                      <SortIcon field="read_iops" />
                    </div>
                  </th>
                </>
              )}
              
              {statusCardType === 'raid' && (
                <>
                  <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">LVS Status</th>
                  <th 
                    className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-100"
                    onClick={() => handleSort('volumes')}
                  >
                    <div className="flex items-center gap-1">
                      Volumes
                      <SortIcon field="volumes" />
                    </div>
                  </th>
                </>
              )}
              
              {statusCardType === 'default' && (
                <>
                  <th className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">LVS Initialized</th>
                  <th 
                    className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-100"
                    onClick={() => handleSort('read_iops')}
                  >
                    <div className="flex items-center gap-1">
                      Performance
                      <SortIcon field="read_iops" />
                    </div>
                  </th>
                  <th 
                    className="px-6 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider cursor-pointer hover:bg-gray-100"
                    onClick={() => handleSort('volumes')}
                  >
                    <div className="flex items-center gap-1">
                      Volumes
                      <SortIcon field="volumes" />
                    </div>
                  </th>
                </>
              )}
            </tr>
          </thead>
          <tbody className="bg-white divide-y divide-gray-200">
            {filteredDisks.length === 0 ? (
              <tr>
                <td colSpan={
                  showOnlyTable ? 
                    (statusCardType === 'nvmeof' ? 9 : statusCardType === 'raid' ? 9 : 10) : 
                    (statusCardType === 'nvmeof' ? 10 : statusCardType === 'raid' ? 10 : 11)
                } className="px-6 py-8 text-center text-gray-500">
                  {volumeReplicaFilter && targetVolume
                    ? `No disks contain replicas for volume "${targetVolume.name}".`
                    : volumeFilter && volumeFilter !== 'all' 
                    ? `No disks have ${getFilterDisplayName(volumeFilter)}.`
                    : activeFilterCount > 0
                    ? 'No disks match the current filters.'
                    : 'No disks found.'
                  }
                </td>
              </tr>
            ) : (
              filteredDisks.map((disk) => {
                let displayVolumes = disk.provisioned_volumes;
                
                // Apply volume replica filter
                if (volumeReplicaFilter) {
                  displayVolumes = disk.provisioned_volumes.filter(diskVolume => 
                    diskVolume.volume_id === volumeReplicaFilter
                  );
                }
                // Apply general volume filter if no specific volume replica filter
                else if (volumeFilter && volumeFilter !== 'all') {
                  displayVolumes = disk.provisioned_volumes.filter(diskVolume => {
                    const actualVolume = volumes.find(v => v.id === diskVolume.volume_id);
                    if (!actualVolume) return false;
                    
                    switch (volumeFilter) {
                      case 'faulted':
                        return actualVolume.state === 'Degraded' || actualVolume.state === 'Failed';
                      case 'rebuilding':
                        return actualVolume.replica_statuses.some(replica => 
                          replica.status === 'rebuilding' || 
                          replica.rebuild_progress !== null ||
                          replica.is_new_replica
                        );
                      case 'local-nvme':
                        return actualVolume.local_nvme;
                      default:
                        return true;
                    }
                  });
                }

                const utilization = Math.round((disk.allocated_space / disk.capacity_gb) * 100);

                const isDisabledForRaidSelection = disk.is_remote || disk.is_system_disk;
                const selected = selectedDiskIds.has(disk.id);
                return (
                  <tr key={disk.id} className="hover:bg-gray-50">
                    {!showOnlyTable && (
                      <td className="px-6 py-4 whitespace-nowrap text-sm">
                        <input
                          type="checkbox"
                          disabled={activePane !== 'raid' || isDisabledForRaidSelection}
                          checked={selected}
                          onChange={(e) => {
                            const next = new Set(selectedDiskIds);
                            if (e.target.checked) next.add(disk.id); else next.delete(disk.id);
                            setSelectedDiskIds(next);
                          }}
                        />
                      </td>
                    )}
                    <td className="px-6 py-4 whitespace-nowrap text-sm font-medium text-gray-900">
                      <button
                        onClick={() => {
                          if (onDiskVolumeFilter) {
                            onDiskVolumeFilter(disk.id);
                          } else {
                            onDiskClick?.(disk.id);
                          }
                        }}
                        className="text-blue-600 hover:text-blue-800 hover:underline font-medium"
                      >
                        {disk.id}
                      </button>
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                      <div className="flex items-center gap-1">
                        <Server className="w-4 h-4 text-gray-400" />
                        {disk.node}
                      </div>
                    </td>
                    
                    {/* Contextual columns based on disk type */}
                    {statusCardType === 'nvmeof' && (
                      <>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.model}</td>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                          {(() => {
                            const endpoint = disk.nvmeof_endpoint;
                            if (typeof endpoint === 'string') return 'tcp';
                            if (endpoint && typeof endpoint === 'object') return (endpoint as any).transport || 'tcp';
                            return 'Local';
                          })()}
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                          <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                            disk.is_remote ? 'bg-purple-100 text-purple-800' : 'bg-green-100 text-green-800'
                          }`}>
                            {disk.is_remote ? 'Remote' : 'Local'}
                          </span>
                        </td>
                      </>
                    )}
                    
                    {statusCardType === 'raid' && (
                      <>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                          <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                            disk.raid_level === 0 ? 'bg-blue-100 text-blue-800' : 
                            disk.raid_level === 1 ? 'bg-green-100 text-green-800' : 
                            'bg-gray-100 text-gray-800'
                          }`}>
                            {disk.raid_level ? `RAID-${disk.raid_level}` : 'Unknown'}
                          </span>
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                          {disk.raid_members?.length || 0} members
                        </td>
                      </>
                    )}
                    
                    {statusCardType === 'default' && (
                      <>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.model}</td>
                      </>
                    )}
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.capacity_gb}GB</td>
                    <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">{disk.free_space}GB</td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      <div className="flex items-center gap-2">
                        <div className="w-20 bg-gray-200 rounded-full h-2">
                          <div 
                            className={`h-2 rounded-full ${
                              utilization < 25 ? 'bg-green-500' :
                              utilization < 75 ? 'bg-yellow-500' :
                              utilization < 95 ? 'bg-orange-500' :
                              'bg-red-500'
                            }`}
                            style={{ width: `${utilization}%` }}
                          />
                        </div>
                        <span className="text-xs text-gray-600 min-w-[3rem]">{utilization}%</span>
                      </div>
                    </td>
                    <td className="px-6 py-4 whitespace-nowrap">
                      <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                        disk.healthy ? 'bg-green-100 text-green-800' : 'bg-red-100 text-red-800'
                      }`}>
                        {disk.healthy ? 'Healthy' : 'Unhealthy'}
                      </span>
                    </td>
                    
                    {/* Context-specific rightmost columns */}
                    {statusCardType === 'nvmeof' && (
                      <>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                          {disk.raid_membership ? (
                            <button
                              onClick={() => onDiskClick?.(typeof disk.raid_membership === 'string' ? disk.raid_membership : 'unknown')}
                              className="text-blue-600 hover:text-blue-800 hover:underline"
                            >
                              {typeof disk.raid_membership === 'string' ? disk.raid_membership : 'RAID Array'}
                            </button>
                          ) : (
                            <span className="text-gray-400">Not in RAID</span>
                          )}
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                          <div className="space-y-1">
                            <div className="flex items-center gap-1">
                              <span className="text-xs text-green-600">R:</span>
                              <span className="text-xs">{disk.read_iops.toLocaleString()} IOPS</span>
                            </div>
                            <div className="flex items-center gap-1">
                              <span className="text-xs text-blue-600">W:</span>
                              <span className="text-xs">{disk.write_iops.toLocaleString()} IOPS</span>
                            </div>
                            <div className="text-xs text-gray-400">
                              {disk.read_latency}μs / {disk.write_latency}μs
                            </div>
                          </div>
                        </td>
                      </>
                    )}
                    
                    {statusCardType === 'raid' && (
                      <>
                        <td className="px-6 py-4 whitespace-nowrap">
                          <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                            disk.blobstore_initialized ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
                          }`}>
                            {disk.blobstore_initialized ? 'Ready' : 'Not Ready'}
                          </span>
                          {disk.blobstore_initialized && (
                            <div className="text-xs text-gray-500 mt-1">
                              <div className="flex items-center gap-1">
                                <span>{disk.lvol_count} logical volumes</span>
                                {disk.orphaned_spdk_volumes && disk.orphaned_spdk_volumes.length > 0 && (
                                  <span 
                                    className="text-orange-500 flex items-center gap-1" 
                                    title={`${disk.orphaned_spdk_volumes.length} orphaned SPDK volume${disk.orphaned_spdk_volumes.length !== 1 ? 's' : ''}`}
                                  >
                                    <AlertTriangle className="w-3 h-3" />
                                    <span className="text-xs">+{disk.orphaned_spdk_volumes.length} orphaned</span>
                                  </span>
                                )}
                              </div>
                            </div>
                          )}
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap">
                          <button
                            onClick={() => onDiskClick?.(disk.id)}
                            className="text-blue-600 hover:text-blue-800 hover:underline text-sm"
                          >
                            {displayVolumes.length} volume{displayVolumes.length !== 1 ? 's' : ''}
                            {((volumeFilter && volumeFilter !== 'all') || volumeReplicaFilter) && 
                             displayVolumes.length !== disk.provisioned_volumes.length && (
                              <span className="text-gray-400">/{disk.provisioned_volumes.length}</span>
                            )}
                          </button>
                          {displayVolumes.length > 0 && (
                            <div className="mt-1 space-y-1">
                              {displayVolumes.slice(0, 2).map((vol, idx) => (
                                <div key={idx} className="text-xs text-gray-500 flex items-center gap-1">
                                  <div className={`w-2 h-2 rounded-full ${
                                    vol.status === 'healthy' ? 'bg-green-500' :
                                    vol.status === 'rebuilding' ? 'bg-orange-500' :
                                    'bg-red-500'
                                  }`}></div>
                                  <span className="truncate max-w-[8rem]">{vol.volume_name}</span>
                                  <span className="text-gray-400">({vol.size}GB)</span>
                                </div>
                              ))}
                              {displayVolumes.length > 2 && (
                                <div className="text-xs text-gray-400">
                                  +{displayVolumes.length - 2} more...
                                </div>
                              )}
                            </div>
                          )}
                        </td>
                      </>
                    )}
                    
                    {statusCardType === 'default' && (
                      <>
                        <td className="px-6 py-4 whitespace-nowrap">
                          <span className={`inline-flex px-2 py-1 text-xs font-semibold rounded-full ${
                            disk.blobstore_initialized ? 'bg-blue-100 text-blue-800' : 'bg-gray-100 text-gray-800'
                          }`}>
                            {disk.blobstore_initialized ? 'Yes' : 'No'}
                          </span>
                          {disk.blobstore_initialized && (
                            <div className="text-xs text-gray-500 mt-1">
                              <div className="flex items-center gap-1">
                                <span>{disk.lvol_count} logical volumes</span>
                                {disk.orphaned_spdk_volumes && disk.orphaned_spdk_volumes.length > 0 && (
                                  <span 
                                    className="text-orange-500 flex items-center gap-1" 
                                    title={`${disk.orphaned_spdk_volumes.length} orphaned SPDK volume${disk.orphaned_spdk_volumes.length !== 1 ? 's' : ''}`}
                                  >
                                    <AlertTriangle className="w-3 h-3" />
                                    <span className="text-xs">+{disk.orphaned_spdk_volumes.length} orphaned</span>
                                  </span>
                                )}
                              </div>
                            </div>
                          )}
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap text-sm text-gray-500">
                          <div className="space-y-1">
                            <div className="flex items-center gap-1">
                              <span className="text-xs text-green-600">R:</span>
                              <span className="text-xs">{disk.read_iops.toLocaleString()} IOPS</span>
                            </div>
                            <div className="flex items-center gap-1">
                              <span className="text-xs text-blue-600">W:</span>
                              <span className="text-xs">{disk.write_iops.toLocaleString()} IOPS</span>
                            </div>
                            <div className="text-xs text-gray-400">
                              {disk.read_latency}μs / {disk.write_latency}μs
                            </div>
                          </div>
                        </td>
                        <td className="px-6 py-4 whitespace-nowrap">
                          <button
                            onClick={() => onDiskClick?.(disk.id)}
                            className="text-blue-600 hover:text-blue-800 hover:underline text-sm"
                          >
                            {displayVolumes.length} volume{displayVolumes.length !== 1 ? 's' : ''}
                            {((volumeFilter && volumeFilter !== 'all') || volumeReplicaFilter) && 
                             displayVolumes.length !== disk.provisioned_volumes.length && (
                              <span className="text-gray-400">/{disk.provisioned_volumes.length}</span>
                            )}
                          </button>
                          {displayVolumes.length > 0 && (
                            <div className="mt-1 space-y-1">
                              {displayVolumes.slice(0, 2).map((vol, idx) => (
                                <div key={idx} className="text-xs text-gray-500 flex items-center gap-1">
                                  <div className={`w-2 h-2 rounded-full ${
                                    vol.status === 'healthy' ? 'bg-green-500' :
                                    vol.status === 'rebuilding' ? 'bg-orange-500' :
                                    'bg-red-500'
                                  }`}></div>
                                  <span className="truncate max-w-[8rem]">{vol.volume_name}</span>
                                  <span className="text-gray-400">({vol.size}GB)</span>
                                </div>
                              ))}
                              {displayVolumes.length > 2 && (
                                <div className="text-xs text-gray-400">
                                  +{displayVolumes.length - 2} more...
                                </div>
                              )}
                            </div>
                          )}
                        </td>
                      </>
                    )}
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      </div>

      {/* Create RAID Modal */}
      {showCreateRaid && (
        <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
          <div className="bg-white rounded-lg p-6 w-full max-w-lg">
            <h3 className="text-lg font-semibold mb-4">Create RAID Disk</h3>
            <div className="space-y-3">
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">Name</label>
                <input
                  value={raidName}
                  onChange={(e) => setRaidName(e.target.value)}
                  className="w-full border border-gray-300 rounded px-3 py-2"
                  placeholder="raid-001"
                />
              </div>
              <div className="grid grid-cols-2 gap-3">
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">RAID Level</label>
                  <select
                    value={raidLevel}
                    onChange={(e) => setRaidLevel(e.target.value)}
                    className="w-full border border-gray-300 rounded px-3 py-2"
                  >
                    <option value="0">RAID-0 (Stripe)</option>
                    <option value="1">RAID-1 (Mirror)</option>
                  </select>
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">Node</label>
                  <input
                    value={raidNode}
                    onChange={(e) => setRaidNode(e.target.value)}
                    className="w-full border border-gray-300 rounded px-3 py-2"
                    placeholder="worker-node-1"
                  />
                </div>
              </div>
              <div className="text-sm text-gray-600">
                Members selected: <span className="font-semibold">{selectedDiskIds.size}</span>
              </div>
            </div>
            <div className="mt-5 flex justify-end gap-2">
              <button onClick={() => setShowCreateRaid(false)} className="px-3 py-1 text-sm rounded border">Cancel</button>
              <button
                onClick={async () => {
                  const members = Array.from(selectedDiskIds);
                  const payload = { name: raidName, raid_level: raidLevel, members, created_on_node: raidNode };
                  const resp = await fetch('/api/raiddisks', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(payload) });
                  if (resp.ok) {
                    setShowCreateRaid(false);
                    setSelectedDiskIds(new Set());
                  } else {
                    alert('Failed to create RAID disk');
                  }
                }}
                disabled={selectedDiskIds.size < (raidLevel === '1' ? 2 : 1) || !raidName || !raidNode}
                className="px-3 py-1 text-sm bg-blue-600 text-white rounded disabled:opacity-50"
              >
                Create
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Add Remote NVMe-oF Modal */}
      {showAddRemoteNVMeoF && (
        <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
          <div className="bg-white rounded-lg p-6 w-full max-w-2xl">
            <h3 className="text-lg font-semibold mb-4">Add Remote NVMe-oF Target</h3>
            <div className="space-y-4">
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">Name</label>
                <input
                  type="text"
                  value={remoteNVMeoFForm.name}
                  onChange={(e) => setRemoteNVMeoFForm(prev => ({ ...prev, name: e.target.value }))}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                  placeholder="Storage array name"
                />
              </div>
              
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">NQN (NVMe Qualified Name)</label>
                <input
                  type="text"
                  value={remoteNVMeoFForm.nqn}
                  onChange={(e) => setRemoteNVMeoFForm(prev => ({ ...prev, nqn: e.target.value }))}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                  placeholder="nqn.2023.io.storage:target1"
                />
              </div>
              
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">Transport</label>
                <select
                  value={remoteNVMeoFForm.transport}
                  onChange={(e) => setRemoteNVMeoFForm(prev => ({ ...prev, transport: e.target.value }))}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="tcp">TCP</option>
                  <option value="rdma">RDMA</option>
                  <option value="fc">Fibre Channel</option>
                </select>
              </div>
              
              <div className="grid grid-cols-2 gap-4">
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">Address</label>
                  <input
                    type="text"
                    value={remoteNVMeoFForm.address}
                    onChange={(e) => setRemoteNVMeoFForm(prev => ({ ...prev, address: e.target.value }))}
                    className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                    placeholder="192.168.1.100"
                  />
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700 mb-1">Port</label>
                  <input
                    type="text"
                    value={remoteNVMeoFForm.port}
                    onChange={(e) => setRemoteNVMeoFForm(prev => ({ ...prev, port: e.target.value }))}
                    className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                    placeholder="4420"
                  />
                </div>
              </div>
              
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">Namespace ID</label>
                <input
                  type="text"
                  value={remoteNVMeoFForm.nsid}
                  onChange={(e) => setRemoteNVMeoFForm(prev => ({ ...prev, nsid: e.target.value }))}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                  placeholder="1"
                />
              </div>
              
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">Authentication</label>
                <select
                  value={remoteNVMeoFForm.authMethod}
                  onChange={(e) => setRemoteNVMeoFForm(prev => ({ ...prev, authMethod: e.target.value }))}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="none">None</option>
                  <option value="dhchap">DH-CHAP (NVMe Authentication)</option>
                </select>
              </div>
              
              {remoteNVMeoFForm.authMethod === 'dhchap' && (
                <div className="grid grid-cols-2 gap-4">
                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-1">Username</label>
                    <input
                      type="text"
                      value={remoteNVMeoFForm.username}
                      onChange={(e) => setRemoteNVMeoFForm(prev => ({ ...prev, username: e.target.value }))}
                      className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                      placeholder="nvme_user"
                    />
                  </div>
                  <div>
                    <label className="block text-sm font-medium text-gray-700 mb-1">Password</label>
                    <input
                      type="password"
                      value={remoteNVMeoFForm.password}
                      onChange={(e) => setRemoteNVMeoFForm(prev => ({ ...prev, password: e.target.value }))}
                      className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                      placeholder="••••••••"
                    />
                  </div>
                </div>
              )}
              
              <div className="bg-blue-50 border border-blue-200 rounded-lg p-4">
                <div className="flex items-start gap-3">
                  <div className="text-blue-600">ℹ</div>
                  <div className="text-sm text-blue-800">
                    <p className="font-medium mb-1">About NVMe-oF Targets</p>
                    <p>Connect to a remote NVMe storage device over the network using NVMe over Fabrics. This will create a new nvmeof disk resource that can be used for RAID creation.</p>
                  </div>
                </div>
              </div>
            </div>
            
            <div className="mt-6 flex justify-end gap-2">
              <button
                onClick={() => {
                  setShowAddRemoteNVMeoF(false);
                  setRemoteNVMeoFForm({ 
                    name: '', nqn: '', transport: 'tcp', address: '', port: '4420', nsid: '',
                    authMethod: 'none', username: '', password: ''
                  });
                }}
                className="px-4 py-2 text-sm border border-gray-300 rounded-md hover:bg-gray-50"
              >
                Cancel
              </button>
              <button
                onClick={async () => {
                  // TODO: Implement the actual API call to create NVMe-oF target
                  console.log('Creating NVMe-oF target:', remoteNVMeoFForm);
                  // For now, just close the modal
                  setShowAddRemoteNVMeoF(false);
                  setRemoteNVMeoFForm({ 
                    name: '', nqn: '', transport: 'tcp', address: '', port: '4420', nsid: '',
                    authMethod: 'none', username: '', password: ''
                  });
                }}
                disabled={
                  !remoteNVMeoFForm.name || 
                  !remoteNVMeoFForm.nqn || 
                  !remoteNVMeoFForm.address || 
                  !remoteNVMeoFForm.nsid ||
                  (remoteNVMeoFForm.authMethod === 'dhchap' && (!remoteNVMeoFForm.username || !remoteNVMeoFForm.password))
                }
                className="px-4 py-2 text-sm bg-blue-600 text-white rounded-md hover:bg-blue-700 disabled:opacity-50"
              >
                Add Target
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Additional Filter Summary */}
      {filteredDisks.length > 0 && activeFilterCount > 0 && (
        <div className="mt-4 p-4 bg-gray-50 rounded-lg">
          <h4 className="text-sm font-medium text-gray-700 mb-2">Applied Filters Summary</h4>
          <div className="flex flex-wrap gap-2 text-xs">
            {searchTerm && (
              <span className="px-2 py-1 bg-blue-100 text-blue-800 rounded-full">
                Search: "{searchTerm}"
              </span>
            )}
            {selectedNodes.length > 0 && (
              <span className="px-2 py-1 bg-purple-100 text-purple-800 rounded-full">
                Nodes: {selectedNodes.length} selected
              </span>
            )}
            {healthFilter !== 'all' && (
              <span className="px-2 py-1 bg-green-100 text-green-800 rounded-full">
                Health: {healthFilter}
              </span>
            )}
            {lvsFilter !== 'all' && (
              <span className="px-2 py-1 bg-indigo-100 text-indigo-800 rounded-full">
                LVS: {lvsFilter}
              </span>
            )}
            {utilizationFilter !== 'all' && (
              <span className="px-2 py-1 bg-orange-100 text-orange-800 rounded-full">
                Utilization: {utilizationFilter}
              </span>
            )}
            {(capacityRange.min || capacityRange.max) && (
              <span className="px-2 py-1 bg-gray-100 text-gray-800 rounded-full">
                Capacity: {capacityRange.min || '0'}-{capacityRange.max || '∞'}GB
              </span>
            )}
            {sortField !== 'id' && (
              <span className="px-2 py-1 bg-yellow-100 text-yellow-800 rounded-full">
                Sort: {sortField} ({sortOrder})
              </span>
            )}
          </div>
        </div>
      )}

      {/* Performance Insights */}
      {filteredDisks.length > 10 && (
        <div className="mt-4 p-4 bg-blue-50 border border-blue-200 rounded-lg">
          <h4 className="text-sm font-medium text-blue-800 mb-2 flex items-center gap-2">
            <Activity className="w-4 h-4" />
            Performance Insights
          </h4>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-xs">
            <div>
              <span className="text-blue-700 font-medium">Highest Read IOPS:</span>
              <div className="text-blue-900">
                {Math.max(...filteredDisks.map(d => d.read_iops)).toLocaleString()} IOPS
              </div>
            </div>
            <div>
              <span className="text-blue-700 font-medium">Highest Write IOPS:</span>
              <div className="text-blue-900">
                {Math.max(...filteredDisks.map(d => d.write_iops)).toLocaleString()} IOPS
              </div>
            </div>
            <div>
              <span className="text-blue-700 font-medium">Total Capacity:</span>
              <div className="text-blue-900">
                {filteredDisks.reduce((sum, d) => sum + d.capacity_gb, 0).toLocaleString()}GB
              </div>
            </div>
            <div>
              <span className="text-blue-700 font-medium">Total Free Space:</span>
              <div className="text-blue-900">
                {filteredDisks.reduce((sum, d) => sum + d.free_space, 0).toLocaleString()}GB
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Quick Action Buttons */}
      {filteredDisks.length > 0 && (
        <div className="mt-4 flex flex-wrap gap-2">
          <button
            onClick={() => {
              setHealthFilter('unhealthy');
              setShowFilters(true);
            }}
            className="px-3 py-1 text-xs bg-red-100 text-red-800 rounded-full hover:bg-red-200 transition-colors"
          >
            Show Unhealthy Only
          </button>
          <button
            onClick={() => {
              setLVSFilter('uninitialized');
              setShowFilters(true);
            }}
            className="px-3 py-1 text-xs bg-gray-100 text-gray-800 rounded-full hover:bg-gray-200 transition-colors"
          >
            Show Uninitialized Only
          </button>
          <button
            onClick={() => {
              setUtilizationFilter('high');
              setShowFilters(true);
            }}
            className="px-3 py-1 text-xs bg-orange-100 text-orange-800 rounded-full hover:bg-orange-200 transition-colors"
          >
            Show High Utilization
          </button>
          <button
            onClick={() => {
              setSortField('read_iops');
              setSortOrder('desc');
            }}
            className="px-3 py-1 text-xs bg-blue-100 text-blue-800 rounded-full hover:bg-blue-200 transition-colors"
          >
            Sort by Performance
          </button>
        </div>
      )}

      {/* Delete Orphaned Volume Confirmation Dialog */}
      {showDeleteDialog && volumeToDelete && (
        <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
          <div className="bg-white rounded-lg shadow-xl max-w-md w-full mx-4">
            <div className="p-6">
              <h3 className="text-lg font-semibold text-gray-900 mb-4">
                Delete Orphaned SPDK Volume
              </h3>
              
              <div className="mb-4">
                <div className="bg-red-50 border border-red-200 rounded p-3 mb-4">
                  <div className="flex items-center">
                    <AlertTriangle className="w-5 h-5 text-red-500 mr-2" />
                    <span className="text-red-800 font-medium">Warning: Permanent Deletion</span>
                  </div>
                  <p className="text-red-700 text-sm mt-1">
                    This will permanently delete the SPDK logical volume and free up storage space. 
                    This action cannot be undone.
                  </p>
                </div>
                
                <div className="space-y-2 text-sm">
                  <div><strong>Volume:</strong> {volumeToDelete.volume.spdk_volume_name}</div>
                  <div><strong>UUID:</strong> {volumeToDelete.volume.spdk_volume_uuid}</div>
                  <div><strong>Node:</strong> {volumeToDelete.diskNode}</div>
                  <div><strong>Size:</strong> {volumeToDelete.volume.size_gb.toFixed(2)}GB</div>
                  <div><strong>Status:</strong> <span className="text-orange-600">Orphaned (no Kubernetes tracking)</span></div>
                </div>
              </div>

              <div className="mb-4">
                <label className="block text-sm font-medium text-gray-700 mb-2">
                  Type <code className="bg-gray-100 px-1 rounded">DELETE</code> to confirm:
                </label>
                <input
                  type="text"
                  value={deleteConfirmText}
                  onChange={(e) => setDeleteConfirmText(e.target.value)}
                  className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-red-500 focus:border-red-500"
                  placeholder="Type DELETE to confirm"
                  autoFocus
                />
              </div>

              <div className="flex justify-end space-x-3">
                <button
                  onClick={cancelDeleteOrphanedVolume}
                  disabled={isDeleting}
                  className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-100 rounded-md hover:bg-gray-200 focus:outline-none focus:ring-2 focus:ring-gray-500 disabled:opacity-50"
                >
                  Cancel
                </button>
                <button
                  onClick={confirmDeleteOrphanedVolume}
                  disabled={deleteConfirmText !== 'DELETE' || isDeleting}
                  className="px-4 py-2 text-sm font-medium text-white bg-red-600 rounded-md hover:bg-red-700 focus:outline-none focus:ring-2 focus:ring-red-500 disabled:opacity-50 disabled:cursor-not-allowed flex items-center"
                >
                  {isDeleting ? (
                    <>
                      <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin mr-2"></div>
                      Deleting...
                    </>
                  ) : (
                    <>
                      <Trash2 className="w-4 h-4 mr-2" />
                      Delete Volume
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
