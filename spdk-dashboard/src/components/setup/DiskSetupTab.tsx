import React, { useState, useEffect, useMemo, useRef } from 'react';
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
import { DisksTable } from '../tables/DisksTable';
import { useOperations } from '../../contexts/OperationsContext';

type ViewMode = 'grid' | 'compact' | 'table';
type StatusFilter = 'all' | 'free' | 'driver-bound' | 'lvs-ready' | 'setup-ready' | 'initialize-ready' | 'ready' | 'needs-unmount' | 'system' | 'spdk-ready' | 'driver-ready';
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
    if (disk.blobstore_initialized) return 'border-green-200 bg-green-50';
    if (disk.driver_ready) return 'border-blue-200 bg-blue-50';
    return 'border-gray-200 bg-gray-50';
  };

  const getStatusIcon = () => {
    if (disk.is_system_disk) return <Shield className="w-4 h-4 text-red-600" />;
    if (needsUnmount) return <AlertTriangle className="w-4 h-4 text-yellow-600" />;
    if (disk.blobstore_initialized) return <CheckCircle className="w-4 h-4 text-green-600" />;
    if (disk.driver_ready) return <Settings className="w-4 h-4 text-blue-600" />;
    return <CheckCircle className="w-4 h-4 text-gray-600" />;
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
    
    // Enhanced status logic based on driver and blobstore state
    if (disk.blobstore_initialized) {
      // Both driver and blobstore ready
      return <span className="px-2 py-0.5 text-xs bg-green-100 text-green-700 rounded-full">LVS Ready</span>;
    } else if (disk.driver_ready) {
      // Driver ready but needs blobstore initialization
      return <span className="px-2 py-0.5 text-xs bg-blue-100 text-blue-700 rounded-full">Driver Bound</span>;
    } else {
      // Needs full setup (driver binding + blobstore)
      return <span className="px-2 py-0.5 text-xs bg-gray-100 text-gray-700 rounded-full">Free</span>;
    }
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
  const { nodeData, refreshNodeDisks, setupDisksOnNode, initializeBlobstoreOnNode, deleteDiskOnNode, setNodeData } = useDiskSetup();
  const { data: dashboardData } = useDashboardData(false); // Get node names from dashboard
  const { setActiveOperationsCount, setActiveSelectionsCount } = useOperations();
  
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
  const [initializeLVSInProgress, setInitializeLVSInProgress] = useState<Set<string>>(new Set());
  const [unbindDriverInProgress, setUnbindDriverInProgress] = useState<Set<string>>(new Set());
  const [setupResults, setSetupResults] = useState<Record<string, DiskSetupResult>>({});
  const [showAdvancedOptions, setShowAdvancedOptions] = useState(false);

  // Delete State
  const [deleteInProgress, setDeleteInProgress] = useState<Set<string>>(new Set());
  const [showDeleteConfirmation, setShowDeleteConfirmation] = useState(false);
  const [diskToDelete, setDiskToDelete] = useState<{nodeName: string, pciAddr: string, diskName: string, model: string, size: number} | null>(null);
  const [deleteConfirmationText, setDeleteConfirmationText] = useState('');

  // Get node names from dashboard API, fallback to mock node names for mock data alignment
  const knownNodes = dashboardData?.nodes || [
    'worker-node-1', 
    'worker-node-2', 
    'worker-node-3'
  ];

  // DISABLED: Selection cleanup was causing selections to be cleared
  // This useEffect was the root cause of the selection clearing issue
  /*
  useEffect(() => {
    console.log(`🚫 [SELECTION_CLEANUP] DISABLED - This was clearing selections!`);
  }, [nodeData]);
  */



  useEffect(() => {
    const initialData: Record<string, any> = {};
    knownNodes.forEach(node => {
      initialData[node] = { node, disks: [], loading: true };
    });
    setNodeData(initialData);
    // Discovery disabled: do not auto-refresh on mount
  }, [knownNodes]);

  // Check if any operations are in progress to avoid discovery interference
  const hasActiveOperations = useMemo(() => {
    return setupInProgress.size > 0 || 
           initializeLVSInProgress.size > 0 || 
           unbindDriverInProgress.size > 0 ||
           deleteInProgress.size > 0;
  }, [setupInProgress, initializeLVSInProgress, unbindDriverInProgress, deleteInProgress]);

  // Sync local operation state with global context
  useEffect(() => {
    const totalOperations = setupInProgress.size + 
                            initializeLVSInProgress.size + 
                            unbindDriverInProgress.size + 
                            deleteInProgress.size;
    setActiveOperationsCount(totalOperations);
  }, [setupInProgress, initializeLVSInProgress, unbindDriverInProgress, deleteInProgress, setActiveOperationsCount]);

  // Sync selected disks count with global context to prevent auto-refresh during selection
  useEffect(() => {
    console.log(`🔄 [CONTEXT_SYNC] Updating global selections count: ${selectedDisks.size}`);
    setActiveSelectionsCount(selectedDisks.size);
  }, [selectedDisks, setActiveSelectionsCount]);

  const refreshAllNodes = async () => {
    // Prevent discovery interference during active operations or selections
    if (hasActiveOperations) {
      console.log('⏸️ [REFRESH] Skipping auto-refresh during active operations to prevent interference');
      return;
    }
    if (selectedDisks.size > 0) {
      console.log('⏸️ [REFRESH] Skipping auto-refresh while disks are selected to preserve user state');
      return;
    }
    
    setGlobalRefreshing(true);
    const promises = knownNodes.map(node => refreshNodeDisks(node));
    await Promise.allSettled(promises);
    setGlobalRefreshing(false);
  };

  // Track selection state with ref to avoid stale closures
  const selectedDisksRef = useRef(selectedDisks);
  const operationsRef = useRef({ setupInProgress, initializeLVSInProgress, unbindDriverInProgress, deleteInProgress });
  
  // Update refs when state changes
  useEffect(() => {
    selectedDisksRef.current = selectedDisks;
  }, [selectedDisks]);
  
  useEffect(() => {
    operationsRef.current = { setupInProgress, initializeLVSInProgress, unbindDriverInProgress, deleteInProgress };
  }, [setupInProgress, initializeLVSInProgress, unbindDriverInProgress, deleteInProgress]);

  // Auto-refresh remains enabled via main dashboard refresh, and disk setup reflects CRD + NVMe-oF endpoint info

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
          case 'free': return !disk.is_system_disk && disk.mounted_partitions.length === 0 && !disk.driver_ready;
          case 'driver-bound': return !disk.is_system_disk && disk.mounted_partitions.length === 0 && disk.driver_ready && !disk.blobstore_initialized;
          case 'needs-unmount': return !disk.is_system_disk && disk.mounted_partitions.length > 0;
          case 'system': return disk.is_system_disk;
          case 'lvs-ready': return disk.blobstore_initialized;
          // Legacy support for existing filters
          case 'setup-ready': return !disk.is_system_disk && disk.mounted_partitions.length === 0 && !disk.driver_ready;
          case 'initialize-ready': return !disk.is_system_disk && disk.mounted_partitions.length === 0 && disk.driver_ready && !disk.blobstore_initialized;
          case 'driver-ready': return !disk.is_system_disk && disk.mounted_partitions.length === 0 && disk.driver_ready && !disk.blobstore_initialized;
          case 'spdk-ready': return disk.blobstore_initialized;
          case 'ready': return !disk.is_system_disk && disk.mounted_partitions.length === 0 && !disk.blobstore_initialized;
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

  // Check if deletion is allowed (single LVS Ready disk selected)
  const canDeleteSelectedDisk = useMemo(() => {
    if (selectedDisks.size !== 1) return false;
    
    const selectedDiskKey = Array.from(selectedDisks)[0];
    const colonIndex = selectedDiskKey.indexOf(':');
    const nodeName = selectedDiskKey.substring(0, colonIndex);
    const pciAddr = selectedDiskKey.substring(colonIndex + 1);
    const disk = allDisks.find(d => d.nodeName === nodeName && d.pci_address === pciAddr);
    
    return disk && disk.blobstore_initialized && !disk.is_system_disk;
  }, [selectedDisks, allDisks]);

  // Check if setup is allowed (Free disks or Needs Unmount disks with force_unmount)
  // This will do FULL setup: driver binding + LVS initialization
  const canSetupSelected = useMemo(() => {
    if (selectedDisks.size === 0) return false;
    
    const selectedDiskDetails = Array.from(selectedDisks).map(diskKey => {
      const colonIndex = diskKey.indexOf(':');
      const nodeName = diskKey.substring(0, colonIndex);
      const pciAddr = diskKey.substring(colonIndex + 1);
      return allDisks.find(d => d.nodeName === nodeName && d.pci_address === pciAddr);
    }).filter(Boolean);
    
    // All selected disks must be Free or (Needs Unmount with force_unmount enabled)
    // This will do complete setup from start to LVS Ready
    const result = selectedDiskDetails.every(disk => 
      disk && 
      !disk.is_system_disk && 
      !disk.blobstore_initialized &&  // Not already LVS Ready
      (
        (!disk.driver_ready && disk.mounted_partitions.length === 0) ||  // Free disks
        (!disk.driver_ready && disk.mounted_partitions.length > 0 && setupOptions.force_unmount)  // Needs Unmount with force
      )
    );
    return result;
  }, [selectedDisks, allDisks, setupOptions.force_unmount]);

  // Check if LVS initialization is allowed (Driver Ready disks - RECOVERY ONLY)
  // This is for when setup partially failed and disk is stuck in Driver Ready state
  const canInitializeLVS = useMemo(() => {
    if (selectedDisks.size === 0) return false;
    
    const selectedDiskDetails = Array.from(selectedDisks).map(diskKey => {
      const colonIndex = diskKey.indexOf(':');
      const nodeName = diskKey.substring(0, colonIndex);
      const pciAddr = diskKey.substring(colonIndex + 1);
      return allDisks.find(d => d.nodeName === nodeName && d.pci_address === pciAddr);
    }).filter(Boolean);
    
    // All selected disks must be Driver Ready (recovery scenario)
    const result = selectedDiskDetails.every(disk => 
      disk && 
      !disk.is_system_disk && 
      disk.driver_ready &&
      !disk.blobstore_initialized
    );
    return result;
  }, [selectedDisks, allDisks]);

  // Check if driver unbinding is allowed (Driver Bound disks only)
  const canUnbindDriver = useMemo(() => {
    if (selectedDisks.size === 0) return false;
    
    const selectedDiskDetails = Array.from(selectedDisks).map(diskKey => {
      const colonIndex = diskKey.indexOf(':');
      const nodeName = diskKey.substring(0, colonIndex);
      const pciAddr = diskKey.substring(colonIndex + 1);
      return allDisks.find(d => d.nodeName === nodeName && d.pci_address === pciAddr);
    }).filter(Boolean);
    
    // All selected disks must be Driver Bound (has driver, no LVS)
    const result = selectedDiskDetails.every(disk => 
      disk && 
      !disk.is_system_disk && 
      disk.driver_ready &&
      !disk.blobstore_initialized
    );
    return result;
  }, [selectedDisks, allDisks]);

  const getSelectedDiskInfo = () => {
    if (selectedDisks.size !== 1) return null;
    
    const selectedDiskKey = Array.from(selectedDisks)[0];
    const colonIndex = selectedDiskKey.indexOf(':');
    const nodeName = selectedDiskKey.substring(0, colonIndex);
    const pciAddr = selectedDiskKey.substring(colonIndex + 1);
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
      free: allDisks.filter(d => !d.is_system_disk && d.mounted_partitions.length === 0 && !d.driver_ready).length,
      driverBound: allDisks.filter(d => !d.is_system_disk && d.mounted_partitions.length === 0 && d.driver_ready && !d.blobstore_initialized).length,
      needsUnmount: allDisks.filter(d => !d.is_system_disk && d.mounted_partitions.length > 0).length,
      system: allDisks.filter(d => d.is_system_disk).length,
      lvsReady: allDisks.filter(d => d.blobstore_initialized).length,
      selected: selectedDisks.size,
      filtered: filteredDisks.length,
      // Legacy support
      setupReady: allDisks.filter(d => !d.is_system_disk && d.mounted_partitions.length === 0 && !d.driver_ready).length,
      initializeReady: allDisks.filter(d => !d.is_system_disk && d.mounted_partitions.length === 0 && d.driver_ready && !d.blobstore_initialized).length,
      spdkReady: allDisks.filter(d => d.blobstore_initialized).length,
      ready: allDisks.filter(d => !d.is_system_disk && d.mounted_partitions.length === 0 && !d.blobstore_initialized).length,
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
      setSelectedDisks(prev => {
        const newSelection = new Set([...prev, ...selectableDisks]);
        return newSelection;
      });
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

  const initializeLVSSelectedDisks = async () => {
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
        await initializeLVSOnNodeWrapper(nodeName, disks.map(d => d.pciAddr));
      }
    }
  };

  const initializeLVSOnNodeWrapper = async (node: string, diskPciAddresses: string[]) => {
    setInitializeLVSInProgress(prev => new Set([...prev, node]));

    try {
      const result = await initializeBlobstoreOnNode(node, diskPciAddresses);
      
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
      setInitializeLVSInProgress(prev => {
        const newSet = new Set(prev);
        newSet.delete(node);
        return newSet;
      });
    }
  };

  const unbindDriverSelectedDisks = async () => {
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
        await unbindDriverOnNodeWrapper(nodeName, disks.map(d => d.pciAddr));
      }
    }
  };

  const unbindDriverOnNodeWrapper = async (node: string, diskPciAddresses: string[]) => {
    setUnbindDriverInProgress(prev => new Set([...prev, node]));

    try {
      // Call reset/unbind API endpoint
      const response = await fetch(`/api/nodes/${node}/disks/reset`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ pci_addresses: diskPciAddresses })
      });

      if (response.ok) {
        const result = await response.json();
        
        setSetupResults(prev => ({ ...prev, [node]: {
          success: result.success,
          setup_disks: result.reset_disks || [],
          failed_disks: result.failed_disks || [],
          warnings: result.warnings || [],
          completed_at: result.completed_at || new Date().toISOString()
        }}));

        if (result.success) {
          setSelectedDisks(prev => {
            const newSelection = new Set(prev);
            diskPciAddresses.forEach(addr => newSelection.delete(`${node}:${addr}`));
            return newSelection;
          });
          
          // Refresh node data after unbind to show new status
          setTimeout(() => refreshNodeDisks(node), 2000);
        }
      } else {
        throw new Error(`Unbind request failed: ${response.statusText}`);
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
      setUnbindDriverInProgress(prev => {
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
      setDeleteConfirmationText('');
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
        const newSelection = new Set<string>();
        setSelectedDisks(newSelection);
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

      </div>

      {/* Permanent Two-Pane View: Left = NVMe-oF (from disks inventory), Right = RAID disks */}
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <div className="bg-white rounded-lg shadow p-4">
          <h3 className="text-lg font-semibold mb-3">NVMe-oF Disks</h3>
          <DisksTable 
            disks={[]}
            volumes={[]}
            stats={{ totalDisks: 0, healthyDisks: 0, formattedDisks: 0 }}
            embedInSetup
            statusCardType="nvmeof"
          />
        </div>
        <div className="bg-white rounded-lg shadow p-4">
          <h3 className="text-lg font-semibold mb-3">RAID Disks</h3>
          <DisksTable 
            disks={[]}
            volumes={[]}
            stats={{ totalDisks: 0, healthyDisks: 0, formattedDisks: 0 }}
            embedInSetup
            statusCardType="raid"
          />
        </div>
      </div>
    </div>
  );
};
