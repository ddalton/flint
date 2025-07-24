import React, { useState, useEffect, useMemo } from 'react';
import { 
  HardDrive, Settings, AlertTriangle, CheckCircle, RefreshCw, 
  Play, Database, Shield, Info, ChevronLeft, ChevronRight,
  Search, Filter, Monitor, Grid, List, Trash2  
} from 'lucide-react';
import { 
  useDiskSetup, 
  useDashboardData,
  type UnimplementedDisk,
  type DiskSetupRequest,
  type DiskSetupResult
} from '../../hooks/useDashboardData';

type ViewMode = 'grid' | 'compact' | 'table';
type StatusFilter = 'all' | 'ready' | 'needs-unmount' | 'system' | 'spdk-ready';
type SizeFilter = 'all' | 'small' | 'medium' | 'large' | 'xlarge';

interface CompactDiskCardProps {
  disk: UnimplementedDisk;
  isSelected: boolean;
  onSelect: (selected: boolean) => void;
  nodeName: string;
}

const CompactDiskCard: React.FC<CompactDiskCardProps> = ({ disk, isSelected, onSelect }) => {
  const sizeGB = Math.round(disk.size_bytes / (1024 * 1024 * 1024));
  const needsUnmount = disk.mounted_partitions.length > 0;
  const canSelect = !disk.is_system_disk;

  const getStatusColor = () => {
    if (disk.is_system_disk) return 'border-red-200 bg-red-50';
    if (needsUnmount) return 'border-yellow-200 bg-yellow-50';
    if (disk.spdk_ready) return 'border-indigo-200 bg-indigo-50';
    return 'border-green-200 bg-green-50';
  };

  const getStatusIcon = () => {
    if (disk.is_system_disk) return <Shield className="w-4 h-4 text-red-600" />;
    if (needsUnmount) return <AlertTriangle className="w-4 h-4 text-yellow-600" />;
    if (disk.spdk_ready) return <Settings className="w-4 h-4 text-indigo-600" />;
    return <CheckCircle className="w-4 h-4 text-green-600" />;
  };

  return (
    <div className={`relative border-2 rounded-lg p-3 transition-all hover:shadow-sm ${
      isSelected ? `${getStatusColor()} ring-2 ring-blue-500 ring-offset-1` : getStatusColor()
    }`}>
      {canSelect && (
        <input
          type="checkbox"
          checked={isSelected}
          onChange={(e) => onSelect(e.target.checked)}
          className="absolute top-2 left-2 rounded"
        />
      )}
      
      <div className={`${canSelect ? 'ml-6' : ''}`}>
        <div className="flex items-center justify-between mb-1">
          <div className="flex items-center gap-1">
            {getStatusIcon()}
            <span className="font-medium text-sm">{disk.device_name}</span>
          </div>
          <span className="text-xs font-medium text-gray-600">{sizeGB}GB</span>
        </div>
        
        <div className="text-xs text-gray-500 space-y-0.5">
          <div className="flex justify-between">
            <span>PCI:</span>
            <span className="font-mono">{disk.pci_address}</span>
          </div>
          <div className="flex justify-between">
            <span>Driver:</span>
            <span className={`font-mono ${disk.spdk_ready ? 'text-indigo-600' : ''}`}>
              {disk.driver}
            </span>
          </div>
          {disk.numa_node !== undefined && (
            <div className="flex justify-between">
              <span>NUMA:</span>
              <span>Node {disk.numa_node}</span>
            </div>
          )}
        </div>
        
        {needsUnmount && (
          <div className="mt-1 text-xs text-yellow-700">
            Mounted: {disk.mounted_partitions.slice(0, 2).join(', ')}
            {disk.mounted_partitions.length > 2 && ` +${disk.mounted_partitions.length - 2}`}
          </div>
        )}
        
        <div className="mt-1 text-xs text-gray-400 truncate" title={disk.model}>
          {disk.model}
        </div>
      </div>
    </div>
  );
};

const CompactDiskRow: React.FC<CompactDiskCardProps> = ({ disk, isSelected, onSelect }) => {
  const sizeGB = Math.round(disk.size_bytes / (1024 * 1024 * 1024));
  const needsUnmount = disk.mounted_partitions.length > 0;
  const canSelect = !disk.is_system_disk;

  const getStatusBadge = () => {
    if (disk.is_system_disk) return <span className="px-2 py-0.5 text-xs bg-red-100 text-red-700 rounded-full">System</span>;
    if (needsUnmount) return <span className="px-2 py-0.5 text-xs bg-yellow-100 text-yellow-700 rounded-full">Unmount</span>;
    if (disk.spdk_ready) return <span className="px-2 py-0.5 text-xs bg-indigo-100 text-indigo-700 rounded-full">SPDK</span>;
    return <span className="px-2 py-0.5 text-xs bg-green-100 text-green-700 rounded-full">Ready</span>;
  };

  return (
    <tr className={`hover:bg-gray-50 ${isSelected ? 'bg-blue-50' : ''}`}>
      <td className="px-3 py-2">
        {canSelect && (
          <input
            type="checkbox"
            checked={isSelected}
            onChange={(e) => onSelect(e.target.checked)}
            className="rounded"
          />
        )}
      </td>
      <td className="px-3 py-2">
        <div className="text-sm font-medium">{disk.device_name}</div>
        <div className="text-xs text-gray-500 font-mono">{disk.pci_address}</div>
      </td>
      <td className="px-3 py-2 text-sm">{sizeGB}GB</td>
      <td className="px-3 py-2">
        <span className={`text-xs font-mono px-2 py-1 rounded ${
          disk.spdk_ready ? 'bg-indigo-100 text-indigo-700' : 'bg-gray-100 text-gray-700'
        }`}>
          {disk.driver}
        </span>
      </td>
      <td className="px-3 py-2">{getStatusBadge()}</td>
      <td className="px-3 py-2 text-xs text-gray-500 max-w-xs truncate" title={disk.model}>
        {disk.model}
      </td>
      <td className="px-3 py-2 text-xs text-center">
        {disk.numa_node !== undefined ? disk.numa_node : '-'}
      </td>
      <td className="px-3 py-2 text-xs">
        {needsUnmount ? (
          <div className="text-yellow-700">
            {disk.mounted_partitions.slice(0, 1).join(', ')}
            {disk.mounted_partitions.length > 1 && `+${disk.mounted_partitions.length - 1}`}
          </div>
        ) : (
          <span className="text-gray-400">None</span>
        )}
      </td>
    </tr>
  );
};

export const DiskSetupTab: React.FC = () => {
  const { nodeData, refreshNodeDisks, setupDisksOnNode, deleteDiskOnNode, setNodeData } = useDiskSetup();
  const { data: dashboardData } = useDashboardData(false); // Get node names from dashboard
  
  // UI State
  const [selectedDisks, setSelectedDisks] = useState<Set<string>>(new Set());
  const [viewMode, setViewMode] = useState<ViewMode>('grid');
  const [currentPage, setCurrentPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);
  const [searchTerm, setSearchTerm] = useState('');
  const [selectedNodes, setSelectedNodes] = useState<string[]>([]);
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all');
  const [sizeFilter, setSizeFilter] = useState<SizeFilter>('all');
  const [showFilters, setShowFilters] = useState(false);
  const [globalRefreshing, setGlobalRefreshing] = useState(false);
  
  // Setup State
  const [setupOptions, setSetupOptions] = useState({
    force_unmount: false,
    backup_data: true,
    huge_pages_mb: 2048,
    driver_override: 'vfio-pci'
  });
  const [setupInProgress, setSetupInProgress] = useState<Set<string>>(new Set());
  const [setupResults, setSetupResults] = useState<Record<string, DiskSetupResult>>({});
  const [showAdvancedOptions, setShowAdvancedOptions] = useState(false);

  // Delete State
  const [deleteInProgress, setDeleteInProgress] = useState<Set<string>>(new Set());
  const [showDeleteConfirmation, setShowDeleteConfirmation] = useState(false);
  const [diskToDelete, setDiskToDelete] = useState<{nodeName: string, pciAddr: string, diskName: string, model: string, size: number} | null>(null);

  // Get node names from dashboard API, fallback to mock node names for mock data alignment
  const knownNodes = dashboardData?.nodes || [
    'worker-node-1', 
    'worker-node-2', 
    'worker-node-3'
  ];

  useEffect(() => {
    const initialData: Record<string, any> = {};
    knownNodes.forEach(node => {
      initialData[node] = { node, disks: [], loading: true };
    });
    setNodeData(initialData);
    refreshAllNodes();
  }, [knownNodes]);

  const refreshAllNodes = async () => {
    setGlobalRefreshing(true);
    const promises = knownNodes.map(node => refreshNodeDisks(node));
    await Promise.allSettled(promises);
    setGlobalRefreshing(false);
  };

  // Flatten all disks from all nodes
  const allDisks = useMemo(() => {
    return Object.entries(nodeData).flatMap(([nodeName, data]) => 
      data.disks.map((disk: UnimplementedDisk) => ({ ...disk, nodeName }))
    );
  }, [nodeData]);

  // Apply filters
  const filteredDisks = useMemo(() => {
    let result = allDisks;

    // Search filter
    if (searchTerm) {
      const searchLower = searchTerm.toLowerCase();
      result = result.filter(disk => 
        disk.device_name.toLowerCase().includes(searchLower) ||
        disk.pci_address.toLowerCase().includes(searchLower) ||
        disk.model.toLowerCase().includes(searchLower) ||
        disk.serial.toLowerCase().includes(searchLower) ||
        disk.nodeName.toLowerCase().includes(searchLower)
      );
    }

    // Node filter
    if (selectedNodes.length > 0) {
      result = result.filter(disk => selectedNodes.includes(disk.nodeName));
    }

    // Status filter
    if (statusFilter !== 'all') {
      result = result.filter(disk => {
        switch (statusFilter) {
          case 'ready': return !disk.is_system_disk && disk.mounted_partitions.length === 0 && !disk.spdk_ready;
          case 'needs-unmount': return !disk.is_system_disk && disk.mounted_partitions.length > 0;
          case 'system': return disk.is_system_disk;
          case 'spdk-ready': return disk.spdk_ready;
          default: return true;
        }
      });
    }

    // Size filter
    if (sizeFilter !== 'all') {
      result = result.filter(disk => {
        const sizeGB = disk.size_bytes / (1024 * 1024 * 1024);
        switch (sizeFilter) {
          case 'small': return sizeGB < 500;
          case 'medium': return sizeGB >= 500 && sizeGB < 1000;
          case 'large': return sizeGB >= 1000 && sizeGB < 2000;
          case 'xlarge': return sizeGB >= 2000;
          default: return true;
        }
      });
    }

    return result;
  }, [allDisks, searchTerm, selectedNodes, statusFilter, sizeFilter]);

  // Check if deletion is allowed (single SPDK Ready disk selected)
  const canDeleteSelectedDisk = useMemo(() => {
    if (selectedDisks.size !== 1) return false;
    
    const selectedDiskKey = Array.from(selectedDisks)[0];
    const [nodeName, pciAddr] = selectedDiskKey.split(':');
    const disk = allDisks.find(d => d.nodeName === nodeName && d.pci_address === pciAddr);
    
    return disk && disk.spdk_ready && !disk.is_system_disk;
  }, [selectedDisks, allDisks]);

  // Check if setup is allowed (selected disks are NOT already SPDK ready and are suitable for setup)
  const canSetupSelected = useMemo(() => {
    if (selectedDisks.size === 0) return false;
    
    const selectedDiskDetails = Array.from(selectedDisks).map(diskKey => {
      const [nodeName, pciAddr] = diskKey.split(':');
      return allDisks.find(d => d.nodeName === nodeName && d.pci_address === pciAddr);
    }).filter(Boolean);
    
    // All selected disks must be suitable for setup (not system disks, not already SPDK ready, no mounted partitions)
    return selectedDiskDetails.every(disk => 
      disk && 
      !disk.is_system_disk && 
      !disk.spdk_ready && 
      disk.mounted_partitions.length === 0
    );
  }, [selectedDisks, allDisks]);

  const getSelectedDiskInfo = () => {
    if (selectedDisks.size !== 1) return null;
    
    const selectedDiskKey = Array.from(selectedDisks)[0];
    const [nodeName, pciAddr] = selectedDiskKey.split(':');
    const disk = allDisks.find(d => d.nodeName === nodeName && d.pci_address === pciAddr);
    
    if (disk && disk.spdk_ready && !disk.is_system_disk) {
      return { 
        nodeName, 
        pciAddr, 
        diskName: disk.device_name,
        model: disk.model,
        size: Math.round(disk.size_bytes / (1024 * 1024 * 1024))
      };
    }
    return null;
  };

  // Pagination
  const totalPages = Math.ceil(filteredDisks.length / pageSize);
  const paginatedDisks = filteredDisks.slice((currentPage - 1) * pageSize, currentPage * pageSize);

  // Statistics
  const stats = useMemo(() => {
    return {
      total: allDisks.length,
      ready: allDisks.filter(d => !d.is_system_disk && d.mounted_partitions.length === 0 && !d.spdk_ready).length,
      needsUnmount: allDisks.filter(d => !d.is_system_disk && d.mounted_partitions.length > 0).length,
      system: allDisks.filter(d => d.is_system_disk).length,
      spdkReady: allDisks.filter(d => d.spdk_ready).length,
      selected: selectedDisks.size,
      filtered: filteredDisks.length
    };
  }, [allDisks, filteredDisks, selectedDisks]);

  const handleDiskSelection = (diskKey: string, selected: boolean) => {
    setSelectedDisks(prev => {
      const newSelection = new Set(prev);
      if (selected) {
        newSelection.add(diskKey);
      } else {
        newSelection.delete(diskKey);
      }
      return newSelection;
    });
  };

  const handleSelectAll = (selectAll: boolean) => {
    if (selectAll) {
      const selectableDisks = paginatedDisks
        .filter(disk => !disk.is_system_disk)
        .map(disk => `${disk.nodeName}:${disk.pci_address}`);
      setSelectedDisks(prev => new Set([...prev, ...selectableDisks]));
    } else {
      const pageDisks = paginatedDisks.map(disk => `${disk.nodeName}:${disk.pci_address}`);
      setSelectedDisks(prev => {
        const newSelection = new Set(prev);
        pageDisks.forEach(key => newSelection.delete(key));
        return newSelection;
      });
    }
  };

  const setupSelectedDisks = async () => {
    const disksByNode = Object.groupBy(
      Array.from(selectedDisks).map(diskKey => {
        const firstColonIndex = diskKey.indexOf(':');
        const nodeName = diskKey.substring(0, firstColonIndex);
        const pciAddr = diskKey.substring(firstColonIndex + 1);
        return { nodeName, pciAddr };
      }),
      ({ nodeName }) => nodeName
    );

    for (const [nodeName, disks] of Object.entries(disksByNode)) {
      if (disks && disks.length > 0) {
        await setupDisksOnNodeWrapper(nodeName, disks.map(d => d.pciAddr));
      }
    }
  };

  const setupDisksOnNodeWrapper = async (node: string, diskPciAddresses: string[]) => {
    setSetupInProgress(prev => new Set([...prev, node]));

    const request: DiskSetupRequest = {
      pci_addresses: diskPciAddresses,
      force_unmount: setupOptions.force_unmount,
      backup_data: setupOptions.backup_data,
      huge_pages_mb: setupOptions.huge_pages_mb,
      driver_override: setupOptions.driver_override
    };

    try {
      const result = await setupDisksOnNode(node, request);
      
      setSetupResults(prev => ({ ...prev, [node]: result }));

      if (result.success) {
        setSelectedDisks(prev => {
          const newSelection = new Set(prev);
          diskPciAddresses.forEach(addr => newSelection.delete(`${node}:${addr}`));
          return newSelection;
        });
      }
    } catch (error) {
      const errorResult: DiskSetupResult = {
        success: false,
        setup_disks: [],
        failed_disks: diskPciAddresses.map(addr => [addr, error instanceof Error ? error.message : 'Unknown error']),
        warnings: [],
        completed_at: new Date().toISOString()
      };
      setSetupResults(prev => ({ ...prev, [node]: errorResult }));
    } finally {
      setSetupInProgress(prev => {
        const newSet = new Set(prev);
        newSet.delete(node);
        return newSet;
      });
    }
  };

  const handleDeleteDisk = () => {
    const diskInfo = getSelectedDiskInfo();
    if (diskInfo) {
      setDiskToDelete(diskInfo);
      setShowDeleteConfirmation(true);
    }
  };

  const confirmDeleteDisk = async () => {
    if (!diskToDelete) return;

    setDeleteInProgress(prev => new Set([...prev, diskToDelete.nodeName]));
    setShowDeleteConfirmation(false);

    try {
      const result = await deleteDiskOnNode(diskToDelete.nodeName, diskToDelete.pciAddr);

      if (result.success) {
        // Remove from selection and refresh
        setSelectedDisks(new Set());
        setTimeout(() => refreshNodeDisks(diskToDelete.nodeName), 2000);
      }
    } catch (error) {
      console.error('Failed to delete disk:', error);
    } finally {
      setDeleteInProgress(prev => {
        const newSet = new Set(prev);
        newSet.delete(diskToDelete.nodeName);
        return newSet;
      });
      setDiskToDelete(null);
    }
  };

  const clearAllFilters = () => {
    setSearchTerm('');
    setSelectedNodes([]);
    setStatusFilter('all');
    setSizeFilter('all');
    setCurrentPage(1);
  };

  const activeFilterCount = [
    searchTerm,
    selectedNodes.length > 0,
    statusFilter !== 'all',
    sizeFilter !== 'all'
  ].filter(Boolean).length;

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="bg-white rounded-lg shadow p-6">
        <div className="flex items-center justify-between mb-6">
          <div className="flex items-center gap-3">
            <HardDrive className="w-8 h-8 text-blue-600" />
            <div>
              <h2 className="text-2xl font-bold text-gray-900">Disk Setup for SPDK</h2>
              <p className="text-gray-600">Initialize NVMe disks across {knownNodes.length} nodes</p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <button
              onClick={refreshAllNodes}
              disabled={globalRefreshing}
              className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md disabled:opacity-50"
            >
              <RefreshCw className={`w-5 h-5 ${globalRefreshing ? 'animate-spin' : ''}`} />
            </button>
          </div>
        </div>

        {/* Statistics Cards */}
        <div className="grid grid-cols-2 md:grid-cols-6 gap-4">
          <div className="bg-blue-50 rounded-lg p-4 text-center">
            <Database className="w-6 h-6 text-blue-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-blue-600">{stats.total}</p>
            <p className="text-sm text-gray-600">Total Disks</p>
          </div>
          <div className="bg-green-50 rounded-lg p-4 text-center">
            <CheckCircle className="w-6 h-6 text-green-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-green-600">{stats.ready}</p>
            <p className="text-sm text-gray-600">Setup Ready</p>
          </div>
          <div className="bg-yellow-50 rounded-lg p-4 text-center">
            <AlertTriangle className="w-6 h-6 text-yellow-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-yellow-600">{stats.needsUnmount}</p>
            <p className="text-sm text-gray-600">Needs Unmount</p>
          </div>
          <div className="bg-red-50 rounded-lg p-4 text-center">
            <Shield className="w-6 h-6 text-red-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-red-600">{stats.system}</p>
            <p className="text-sm text-gray-600">System Disks</p>
          </div>
          <div className="bg-indigo-50 rounded-lg p-4 text-center">
            <Settings className="w-6 h-6 text-indigo-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-indigo-600">{stats.spdkReady}</p>
            <p className="text-sm text-gray-600">SPDK Ready</p>
          </div>
          <div className="bg-purple-50 rounded-lg p-4 text-center">
            <Monitor className="w-6 h-6 text-purple-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-purple-600">{stats.selected}</p>
            <p className="text-sm text-gray-600">Selected</p>
          </div>
        </div>
      </div>

      {/* Filters and Controls */}
      <div className="bg-white rounded-lg shadow">
        {/* Filter Header */}
        <div className="px-6 py-4 border-b border-gray-200 flex items-center justify-between">
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2">
              <Filter className="w-5 h-5 text-gray-600" />
              <span className="font-medium">Filters</span>
              {activeFilterCount > 0 && (
                <span className="px-2 py-1 text-xs bg-blue-100 text-blue-800 rounded-full">
                  {activeFilterCount} active
                </span>
              )}
            </div>
            <div className="text-sm text-gray-500">
              {stats.filtered} of {stats.total} disks
            </div>
          </div>
          
          <div className="flex items-center gap-2">
            {activeFilterCount > 0 && (
              <button
                onClick={clearAllFilters}
                className="text-sm text-gray-600 hover:text-gray-800"
              >
                Clear All
              </button>
            )}
            <button
              onClick={() => setShowFilters(!showFilters)}
              className="text-sm text-blue-600 hover:text-blue-800"
            >
              {showFilters ? 'Hide' : 'Show'} Filters
            </button>
          </div>
        </div>

        {/* Search Bar */}
        <div className="px-6 py-4 bg-gray-50">
          <div className="relative">
            <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
            <input
              type="text"
              placeholder="Search by device name, PCI address, model, serial, or node..."
              value={searchTerm}
              onChange={(e) => {
                setSearchTerm(e.target.value);
                setCurrentPage(1);
              }}
              className="w-full pl-10 pr-4 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
            />
          </div>
        </div>

        {/* Advanced Filters */}
        {showFilters && (
          <div className="px-6 py-4 border-t border-gray-200 space-y-4">
            <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
              {/* Node Filter */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">
                  Nodes ({selectedNodes.length} selected)
                </label>
                <div className="space-y-1 max-h-32 overflow-y-auto border border-gray-300 rounded p-2 bg-white">
                  {knownNodes.map(node => (
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
                          setCurrentPage(1);
                        }}
                        className="mr-2 rounded"
                      />
                      {node}
                    </label>
                  ))}
                </div>
              </div>

              {/* Status Filter */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">Status</label>
                <select
                  value={statusFilter}
                  onChange={(e) => {
                    setStatusFilter(e.target.value as StatusFilter);
                    setCurrentPage(1);
                  }}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="all">All Disks</option>
                  <option value="ready">Setup Ready</option>
                  <option value="needs-unmount">Needs Unmount</option>
                  <option value="system">System Disks</option>
                  <option value="spdk-ready">SPDK Ready</option>
                </select>
              </div>

              {/* Size Filter */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">Size</label>
                <select
                  value={sizeFilter}
                  onChange={(e) => {
                    setSizeFilter(e.target.value as SizeFilter);
                    setCurrentPage(1);
                  }}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="all">All Sizes</option>
                  <option value="small">Small (&lt; 500GB)</option>
                  <option value="medium">Medium (500GB - 1TB)</option>
                  <option value="large">Large (1TB - 2TB)</option>
                  <option value="xlarge">X-Large (&gt; 2TB)</option>
                </select>
              </div>
            </div>
          </div>
        )}
      </div>

      {/* View Controls and Pagination */}
      <div className="bg-white rounded-lg shadow p-4">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-4">
            {/* View Mode */}
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium text-gray-700">View:</span>
              <div className="flex border border-gray-300 rounded-md overflow-hidden">
                <button
                  onClick={() => setViewMode('grid')}
                  className={`px-3 py-1 text-sm ${viewMode === 'grid' ? 'bg-blue-600 text-white' : 'bg-white text-gray-700 hover:bg-gray-50'}`}
                >
                  <Grid className="w-4 h-4" />
                </button>
                <button
                  onClick={() => setViewMode('compact')}
                  className={`px-3 py-1 text-sm border-l border-gray-300 ${viewMode === 'compact' ? 'bg-blue-600 text-white' : 'bg-white text-gray-700 hover:bg-gray-50'}`}
                >
                  <List className="w-4 h-4" />
                </button>
              </div>
            </div>

            {/* Page Size */}
            <div className="flex items-center gap-2">
              <span className="text-sm text-gray-700">Show:</span>
              <select
                value={pageSize}
                onChange={(e) => {
                  setPageSize(Number(e.target.value));
                  setCurrentPage(1);
                }}
                className="border border-gray-300 rounded px-2 py-1 text-sm"
              >
                <option value={25}>25</option>
                <option value={50}>50</option>
                <option value={100}>100</option>
                <option value={200}>200</option>
              </select>
              <span className="text-sm text-gray-700">per page</span>
            </div>
          </div>

          {/* Pagination */}
          <div className="flex items-center gap-2">
            <span className="text-sm text-gray-700">
              {((currentPage - 1) * pageSize) + 1}-{Math.min(currentPage * pageSize, filteredDisks.length)} of {filteredDisks.length}
            </span>
            <button
              onClick={() => setCurrentPage(prev => Math.max(1, prev - 1))}
              disabled={currentPage === 1}
              className="p-1 text-gray-500 hover:text-gray-700 disabled:opacity-50"
            >
              <ChevronLeft className="w-4 h-4" />
            </button>
            <span className="px-2 py-1 text-sm">{currentPage} / {totalPages}</span>
            <button
              onClick={() => setCurrentPage(prev => Math.min(totalPages, prev + 1))}
              disabled={currentPage === totalPages}
              className="p-1 text-gray-500 hover:text-gray-700 disabled:opacity-50"
            >
              <ChevronRight className="w-4 h-4" />
            </button>
          </div>
        </div>
      </div>

      {/* Selection and Setup Controls */}
      {selectedDisks.size > 0 && (
        <div className="bg-white rounded-lg shadow p-4">
          <div className="flex items-center justify-between mb-4">
            <div className="flex items-center gap-4">
              <span className="text-sm font-medium text-gray-700">
                {selectedDisks.size} disk{selectedDisks.size !== 1 ? 's' : ''} selected
              </span>
              <button
                onClick={() => setSelectedDisks(new Set())}
                className="text-sm text-gray-600 hover:text-gray-800"
              >
                Clear Selection
              </button>
            </div>
            <div className="flex items-center gap-2">
              {canDeleteSelectedDisk && (
                <button
                  onClick={handleDeleteDisk}
                  disabled={Array.from(deleteInProgress).length > 0}
                  className="px-4 py-2 bg-red-600 text-white rounded hover:bg-red-700 disabled:opacity-50 flex items-center gap-2"
                >
                  {Array.from(deleteInProgress).length > 0 ? (
                    <>
                      <Settings className="w-4 h-4 animate-spin" />
                      Deleting...
                    </>
                  ) : (
                    <>
                      <Trash2 className="w-4 h-4" />
                      Delete SPDK Disk
                    </>
                  )}
                </button>
              )}
              <button
                onClick={() => setShowAdvancedOptions(!showAdvancedOptions)}
                className="text-sm text-blue-600 hover:text-blue-800"
              >
                {showAdvancedOptions ? 'Hide' : 'Show'} Options
              </button>
              {canSetupSelected && (
                <button
                  onClick={setupSelectedDisks}
                  disabled={Array.from(setupInProgress).length > 0}
                  className="px-4 py-2 bg-blue-600 text-white rounded hover:bg-blue-700 disabled:opacity-50 flex items-center gap-2"
                >
                  {Array.from(setupInProgress).length > 0 ? (
                    <>
                      <Settings className="w-4 h-4 animate-spin" />
                      Setting up...
                    </>
                  ) : (
                    <>
                      <Play className="w-4 h-4" />
                      Setup Selected
                    </>
                  )}
                </button>
              )}
            </div>
          </div>

          {/* Setup Options */}
          <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-4 mb-4">
            <label className="flex items-center space-x-2">
              <input
                type="checkbox"
                checked={setupOptions.force_unmount}
                onChange={(e) => setSetupOptions(prev => ({ ...prev, force_unmount: e.target.checked }))}
                className="rounded"
              />
              <span className="text-sm font-medium">Force Unmount</span>
            </label>
            <label className="flex items-center space-x-2">
              <input
                type="checkbox"
                checked={setupOptions.backup_data}
                onChange={(e) => setSetupOptions(prev => ({ ...prev, backup_data: e.target.checked }))}
                className="rounded"
              />
              <span className="text-sm font-medium">Backup Data</span>
            </label>
          </div>

          {showAdvancedOptions && (
            <div className="grid grid-cols-1 md:grid-cols-2 gap-4 p-4 bg-gray-50 rounded-lg">
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">
                  Huge Pages (MB)
                </label>
                <input
                  type="number"
                  value={setupOptions.huge_pages_mb}
                  onChange={(e) => setSetupOptions(prev => ({ ...prev, huge_pages_mb: parseInt(e.target.value) || 0 }))}
                  className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
                  min="0"
                  step="512"
                />
              </div>
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-1">
                  SPDK Driver
                </label>
                <select
                  value={setupOptions.driver_override}
                  onChange={(e) => setSetupOptions(prev => ({ ...prev, driver_override: e.target.value }))}
                  className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="vfio-pci">vfio-pci (Recommended)</option>
                  <option value="uio_pci_generic">uio_pci_generic</option>
                  <option value="igb_uio">igb_uio</option>
                </select>
              </div>
            </div>
          )}
        </div>
      )}



      {/* Bulk Actions */}
      <div className="bg-white rounded-lg shadow p-4">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm font-medium text-gray-700">Bulk Actions:</span>
            <button
              onClick={() => handleSelectAll(true)}
              className="text-sm text-blue-600 hover:text-blue-800"
            >
              Select All (This Page)
            </button>
            <span className="text-gray-300">|</span>
            <button
              onClick={() => handleSelectAll(false)}
              className="text-sm text-blue-600 hover:text-blue-800"
            >
              Deselect All
            </button>
          </div>
          
          <div className="text-sm text-gray-500">
            Page {currentPage} of {totalPages} • {paginatedDisks.length} items
          </div>
        </div>
      </div>

      {/* Setup Results */}
      {Object.keys(setupResults).length > 0 && (
        <div className="bg-white rounded-lg shadow p-4">
          <h3 className="text-lg font-semibold mb-4">Recent Setup Results</h3>
          <div className="space-y-3">
            {Object.entries(setupResults).map(([node, result]) => (
              <div key={node} className={`p-3 rounded border-l-4 ${
                result.success ? 'border-green-500 bg-green-50' : 'border-red-500 bg-red-50'
              }`}>
                <div className="flex items-center justify-between mb-2">
                  <span className="font-medium">{node}</span>
                  <span className="text-xs text-gray-500">
                    {new Date(result.completed_at).toLocaleString()}
                  </span>
                </div>
                {result.setup_disks.length > 0 && (
                  <div className="text-sm text-green-700 mb-1">
                    ✓ Setup: {result.setup_disks.length} disk{result.setup_disks.length !== 1 ? 's' : ''}
                  </div>
                )}
                {result.failed_disks.length > 0 && (
                  <div className="text-sm text-red-700 mb-1">
                    ✗ Failed: {result.failed_disks.length} disk{result.failed_disks.length !== 1 ? 's' : ''}
                  </div>
                )}
                {result.warnings.length > 0 && (
                  <div className="text-sm text-yellow-700">
                    ⚠ {result.warnings.join(', ')}
                  </div>
                )}
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Disk Display */}
      <div className="bg-white rounded-lg shadow">
        {paginatedDisks.length === 0 ? (
          <div className="text-center py-12">
            <HardDrive className="w-12 h-12 text-gray-400 mx-auto mb-4" />
            <p className="text-lg font-medium text-gray-900">No disks found</p>
            <p className="text-gray-500">
              {activeFilterCount > 0 
                ? 'Try adjusting your filters to see more results.'
                : 'No uninitialized disks are available for setup.'
              }
            </p>
          </div>
        ) : viewMode === 'grid' ? (
          <div className="p-6">
            <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 2xl:grid-cols-6 gap-4">
              {paginatedDisks.map((disk) => {
                const diskKey = `${disk.nodeName}:${disk.pci_address}`;
                return (
                  <CompactDiskCard
                    key={diskKey}
                    disk={disk}
                    isSelected={selectedDisks.has(diskKey)}
                    onSelect={(selected) => handleDiskSelection(diskKey, selected)}
                    nodeName={disk.nodeName}
                  />
                );
              })}
            </div>
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="min-w-full divide-y divide-gray-200">
              <thead className="bg-gray-50">
                <tr>
                  <th className="px-3 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
                    <input
                      type="checkbox"
                      checked={paginatedDisks.filter(d => !d.is_system_disk).length > 0 && 
                               paginatedDisks.filter(d => !d.is_system_disk).every(d => selectedDisks.has(`${d.nodeName}:${d.pci_address}`))}
                      onChange={(e) => handleSelectAll(e.target.checked)}
                      className="rounded"
                    />
                  </th>
                  <th className="px-3 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Device</th>
                  <th className="px-3 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Size</th>
                  <th className="px-3 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Driver</th>
                  <th className="px-3 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Status</th>
                  <th className="px-3 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Model</th>
                  <th className="px-3 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">NUMA</th>
                  <th className="px-3 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">Mounted</th>
                </tr>
              </thead>
              <tbody className="bg-white divide-y divide-gray-200">
                {paginatedDisks.map((disk) => {
                  const diskKey = `${disk.nodeName}:${disk.pci_address}`;
                  return (
                    <CompactDiskRow
                      key={diskKey}
                      disk={disk}
                      isSelected={selectedDisks.has(diskKey)}
                      onSelect={(selected) => handleDiskSelection(diskKey, selected)}
                      nodeName={disk.nodeName}
                    />
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>

      {/* Bottom Pagination */}
      {totalPages > 1 && (
        <div className="bg-white rounded-lg shadow p-4">
          <div className="flex items-center justify-between">
            <div className="text-sm text-gray-700">
              Showing {((currentPage - 1) * pageSize) + 1} to {Math.min(currentPage * pageSize, filteredDisks.length)} of {filteredDisks.length} results
            </div>
            <div className="flex items-center gap-2">
              <button
                onClick={() => setCurrentPage(1)}
                disabled={currentPage === 1}
                className="px-3 py-1 text-sm border border-gray-300 rounded hover:bg-gray-50 disabled:opacity-50"
              >
                First
              </button>
              <button
                onClick={() => setCurrentPage(prev => Math.max(1, prev - 1))}
                disabled={currentPage === 1}
                className="px-3 py-1 text-sm border border-gray-300 rounded hover:bg-gray-50 disabled:opacity-50"
              >
                Previous
              </button>
              
              {/* Page numbers */}
              {Array.from({ length: Math.min(5, totalPages) }, (_, i) => {
                const pageNum = Math.max(1, Math.min(totalPages - 4, currentPage - 2)) + i;
                return (
                  <button
                    key={pageNum}
                    onClick={() => setCurrentPage(pageNum)}
                    className={`px-3 py-1 text-sm border rounded ${
                      pageNum === currentPage
                        ? 'bg-blue-600 text-white border-blue-600'
                        : 'border-gray-300 hover:bg-gray-50'
                    }`}
                  >
                    {pageNum}
                  </button>
                );
              })}
              
              <button
                onClick={() => setCurrentPage(prev => Math.min(totalPages, prev + 1))}
                disabled={currentPage === totalPages}
                className="px-3 py-1 text-sm border border-gray-300 rounded hover:bg-gray-50 disabled:opacity-50"
              >
                Next
              </button>
              <button
                onClick={() => setCurrentPage(totalPages)}
                disabled={currentPage === totalPages}
                className="px-3 py-1 text-sm border border-gray-300 rounded hover:bg-gray-50 disabled:opacity-50"
              >
                Last
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Information Panel */}
      <div className="bg-blue-50 border border-blue-200 rounded-lg p-6">
        <div className="flex items-start gap-3">
          <Info className="w-6 h-6 text-blue-600 mt-1 flex-shrink-0" />
          <div>
            <h4 className="font-medium text-blue-900 mb-2">SPDK Disk Setup Process</h4>
            <div className="text-sm text-blue-800 space-y-2">
              <p>
                <strong>What this does:</strong> Prepares NVMe disks for SPDK usage by unbinding them from the kernel 
                NVMe driver and binding them to a userspace-compatible driver.
              </p>
              <p>
                <strong>Scale:</strong> This interface can handle hundreds of disks across multiple nodes with 
                filtering, pagination, and bulk operations.
              </p>
              <p>
                <strong>Safety:</strong> System disks are automatically excluded. Use filters to focus on specific 
                nodes or disk types before performing bulk operations.
              </p>
            </div>
          </div>
        </div>
      </div>

      {/* Delete Confirmation Dialog */}
      {showDeleteConfirmation && diskToDelete && (
        <div className="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
          <div className="bg-white rounded-lg p-6 max-w-lg w-full mx-4">
            <div className="flex items-center gap-3 mb-4">
              <AlertTriangle className="w-8 h-8 text-red-600" />
              <h3 className="text-lg font-bold text-gray-900">Delete SPDK Disk</h3>
            </div>
            
            <div className="mb-6">
              <p className="text-gray-700 mb-4">
                You are about to delete the SPDK setup for disk:
              </p>
              <div className="bg-gray-50 rounded-lg p-4 space-y-2">
                <div className="flex justify-between">
                  <span className="font-medium">Device:</span>
                  <span className="font-mono">{diskToDelete.diskName}</span>
                </div>
                <div className="flex justify-between">
                  <span className="font-medium">Node:</span>
                  <span>{diskToDelete.nodeName}</span>
                </div>
                <div className="flex justify-between">
                  <span className="font-medium">Model:</span>
                  <span>{diskToDelete.model}</span>
                </div>
                <div className="flex justify-between">
                  <span className="font-medium">Size:</span>
                  <span>{diskToDelete.size}GB</span>
                </div>
              </div>
              
              <div className="mt-4 space-y-3">
                <div className="p-4 bg-blue-50 border border-blue-200 rounded-lg">
                  <h4 className="font-medium text-blue-900 mb-2">Industry Best Practice Options</h4>
                  <div className="space-y-2 text-sm text-blue-800">
                    <label className="flex items-center space-x-2">
                      <input type="checkbox" className="rounded" defaultChecked />
                      <span>Migrate single-replica volumes to other disks</span>
                    </label>
                    <label className="flex items-center space-x-2">
                      <input type="checkbox" className="rounded" defaultChecked />
                      <span>Take snapshots before deletion</span>
                    </label>
                    <label className="flex items-center space-x-2">
                      <input type="checkbox" className="rounded" />
                      <span>Force delete (skip safety checks)</span>
                    </label>
                  </div>
                </div>
                
                <div className="p-4 bg-yellow-50 border border-yellow-200 rounded-lg">
                  <p className="text-sm text-yellow-800">
                    <strong>What will happen:</strong>
                  </p>
                  <ul className="mt-2 text-xs text-yellow-700 space-y-1">
                    <li>• Single-replica volumes will be migrated or deleted</li>
                    <li>• Multi-replica volumes allowed if ≥2 healthy replicas total</li>
                    <li>• LVS (Logical Volume Store) will be destroyed</li>
                    <li>• Disk will be reset to kernel driver mode</li>
                    <li>• Custom resources will be updated</li>
                  </ul>
                </div>
                
                <div className="p-4 bg-red-50 border border-red-200 rounded-lg">
                  <p className="text-sm text-red-800">
                    <strong>⚠️ Warning:</strong> This action cannot be undone. Any data on 
                    single-replica volumes will be lost unless migrated or snapshotted first.
                  </p>
                </div>
              </div>
            </div>

            <div className="flex gap-3 justify-end">
              <button
                onClick={() => {
                  setShowDeleteConfirmation(false);
                  setDiskToDelete(null);
                }}
                className="px-4 py-2 border border-gray-300 text-gray-700 rounded hover:bg-gray-50"
              >
                Cancel
              </button>
              <button
                onClick={confirmDeleteDisk}
                className="px-4 py-2 bg-red-600 text-white rounded hover:bg-red-700 flex items-center gap-2"
              >
                <Trash2 className="w-4 h-4" />
                Delete Disk
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};
