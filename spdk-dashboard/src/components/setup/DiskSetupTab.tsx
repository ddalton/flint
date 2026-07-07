import React, { useState, useEffect, useMemo, useRef } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { apiFetch } from '../../api/client';
import {
  HardDrive, Settings, AlertTriangle, CheckCircle, RefreshCw,
  Play, Database, Shield, Info, ChevronLeft, ChevronRight, ChevronDown,
  Search, Filter, Monitor, Grid, List, Trash2
} from 'lucide-react';
import {
  useDiskSetup,
  useDashboardData,
  type UnimplementedDisk,
  type DiskSetupRequest,
  type DiskSetupResult,
  type NodeDiskData
} from '../../hooks/useDashboardData';
import { useOperations } from '../../contexts/OperationsContext';
import {
  runInitBatch, isBulkSelectable, isBatchEligible, groupDisks, rangeBetween,
  type BatchDisk, type BatchItem, type GroupBy, type SetupOneResult, type DiskGroup
} from './batchSetup';
import { BulkConfirmModal, BatchProgressPanel, type ExcludedDisk } from './BulkInitPanels';
import { Button, IconButton } from '../ui/Button';
import { ConfirmModal } from '../ui/ConfirmModal';
import { SegmentedControl } from '../ui/SegmentedControl';

type ViewMode = 'grid' | 'compact';
type StatusFilter = 'all' | 'free' | 'driver-bound' | 'lvs-ready' | 'setup-ready' | 'initialize-ready' | 'ready' | 'needs-unmount' | 'system' | 'spdk-ready' | 'driver-ready';
type SizeFilter = 'all' | 'small' | 'medium' | 'large' | 'xlarge';

interface CompactDiskCardProps {
  disk: UnimplementedDisk;
  isSelected: boolean;
  onSelect: (selected: boolean, shiftKey: boolean) => void;
  nodeName: string;
}

// A checkbox change carries the modifier state of the click that caused it;
// shift extends the selection over the visible range (see rangeBetween).
const shiftKeyOf = (e: React.ChangeEvent<HTMLInputElement>): boolean =>
  e.nativeEvent instanceof MouseEvent && e.nativeEvent.shiftKey;

const CompactDiskCard: React.FC<CompactDiskCardProps> = ({ disk, isSelected, onSelect }) => {
  const sizeGB = Math.round(disk.size_bytes / (1024 * 1024 * 1024));
  const needsUnmount = disk.mounted_partitions.length > 0;
  const canSelect = !disk.is_system_disk;

  const getStatusColor = () => {
    if (disk.is_system_disk) return 'border-failed-200 bg-failed-50';
    if (needsUnmount) return 'border-degraded-200 bg-degraded-50';
    if (disk.blobstore_initialized) return 'border-healthy-200 bg-healthy-50';
    if (disk.driver_ready) return 'border-brand-200 bg-brand-50';
    return 'border-gray-200 bg-gray-50';
  };

  const getStatusIcon = () => {
    if (disk.is_system_disk) return <Shield className="w-4 h-4 text-failed-600" />;
    if (needsUnmount) return <AlertTriangle className="w-4 h-4 text-degraded-600" />;
    if (disk.blobstore_initialized) return <CheckCircle className="w-4 h-4 text-healthy-600" />;
    if (disk.driver_ready) return <Settings className="w-4 h-4 text-brand-600" />;
    return <CheckCircle className="w-4 h-4 text-gray-600" />;
  };

  return (
    <div className={`relative border-2 rounded-lg p-3 transition-all hover:shadow-sm ${
      isSelected ? `${getStatusColor()} ring-2 ring-brand-500 ring-offset-1` : getStatusColor()
    }`}>
      {canSelect && (
        <input
          type="checkbox"
          checked={isSelected}
          onChange={(e) => onSelect(e.target.checked, shiftKeyOf(e))}
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
            {/* raw indigo on purpose: SPDK-driver accent, no semantic alias */}
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
          <div className="mt-1 text-xs text-degraded-700">
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

  // Borderless status badges on purpose (not Chip): converting would add the
  // kit border and change the table's look; colors are semantic.
  const getStatusBadge = () => {
    if (disk.is_system_disk) return <span className="px-2 py-0.5 text-xs bg-failed-100 text-failed-700 rounded-full">System</span>;
    if (needsUnmount) return <span className="px-2 py-0.5 text-xs bg-degraded-100 text-degraded-700 rounded-full">Unmount</span>;

    // Enhanced status logic based on driver and blobstore state
    if (disk.blobstore_initialized) {
      // Both driver and blobstore ready
      return <span className="px-2 py-0.5 text-xs bg-healthy-100 text-healthy-700 rounded-full">LVS Ready</span>;
    } else if (disk.driver_ready) {
      // Driver ready but needs blobstore initialization
      return <span className="px-2 py-0.5 text-xs bg-brand-100 text-brand-700 rounded-full">Driver Bound</span>;
    } else {
      // Needs full setup (driver binding + blobstore)
      return <span className="px-2 py-0.5 text-xs bg-gray-100 text-gray-700 rounded-full">Free</span>;
    }
  };

  return (
    <tr className={`hover:bg-gray-50 ${isSelected ? 'bg-brand-50' : ''}`}>
      <td className="px-3 py-2">
        {canSelect && (
          <input
            type="checkbox"
            checked={isSelected}
            onChange={(e) => onSelect(e.target.checked, shiftKeyOf(e))}
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
          // raw indigo on purpose: SPDK-driver accent, no semantic alias
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
          <div className="text-degraded-700">
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

type DiskWithNode = UnimplementedDisk & { nodeName: string };

// One place mapping a disk's init state onto the semantic palette — used by
// the per-group status strip and its legend (same buckets as the cards/rows).
const diskStatusMeta = (disk: UnimplementedDisk): { label: string; cell: string } => {
  if (disk.is_system_disk) return { label: 'System', cell: 'bg-failed-500' };
  if (disk.mounted_partitions.length > 0) return { label: 'Needs unmount', cell: 'bg-degraded-400' };
  if (disk.blobstore_initialized) return { label: 'LVS ready', cell: 'bg-healthy-500' };
  if (disk.driver_ready) return { label: 'Driver bound', cell: 'bg-brand-500' };
  return { label: 'Free', cell: 'bg-gray-300' };
};

const STRIP_LEGEND: { label: string; cell: string }[] = [
  { label: 'Free', cell: 'bg-gray-300' },
  { label: 'Driver bound', cell: 'bg-brand-500' },
  { label: 'LVS ready', cell: 'bg-healthy-500' },
  { label: 'Needs unmount', cell: 'bg-degraded-400' },
  { label: 'System', cell: 'bg-failed-500' },
];

// Fleet-scale scan surface (NodesFleetView pattern): one status cell per
// disk, so a node with 100s of disks reads at a glance even collapsed.
// Clicking a cell toggles that disk's selection.
const DiskStatusStrip: React.FC<{
  disks: DiskWithNode[];
  selectedDisks: Set<string>;
  onToggle: (diskKey: string, selected: boolean) => void;
}> = ({ disks, selectedDisks, onToggle }) => (
  <div className="flex flex-wrap gap-1">
    {disks.map(disk => {
      const diskKey = `${disk.nodeName}:${disk.pci_address}`;
      const meta = diskStatusMeta(disk);
      const selected = selectedDisks.has(diskKey);
      const selectable = !disk.is_system_disk;
      return (
        <button
          key={diskKey}
          onClick={() => selectable && onToggle(diskKey, !selected)}
          disabled={!selectable}
          aria-label={`${disk.device_name}: ${meta.label}${selected ? ', selected' : ''}`}
          title={`${disk.device_name} · ${meta.label} · ${Math.round(disk.size_bytes / 1024 ** 3)}GB${selectable ? '' : ' (system disk)'}`}
          className={`w-4 h-4 rounded-sm ${meta.cell} ${
            selectable ? 'hover:ring-2 hover:ring-brand-400' : 'cursor-default opacity-60'
          } focus-visible:outline focus-visible:outline-2 focus-visible:outline-brand-600 ${
            selected ? 'ring-2 ring-offset-1 ring-brand-600' : ''
          }`}
        />
      );
    })}
  </div>
);

interface DiskCollectionProps {
  disks: DiskWithNode[];
  selectedDisks: Set<string>;
  onSelect: (diskKey: string, selected: boolean, shiftKey: boolean) => void;
  headerCheckbox?: React.ReactNode;
}

const DiskGrid: React.FC<DiskCollectionProps> = ({ disks, selectedDisks, onSelect }) => (
  <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 2xl:grid-cols-6 gap-4">
    {disks.map((disk) => {
      const diskKey = `${disk.nodeName}:${disk.pci_address}`;
      return (
        <CompactDiskCard
          key={diskKey}
          disk={disk}
          isSelected={selectedDisks.has(diskKey)}
          onSelect={(selected, shiftKey) => onSelect(diskKey, selected, shiftKey)}
          nodeName={disk.nodeName}
        />
      );
    })}
  </div>
);

const DiskTable: React.FC<DiskCollectionProps> = ({ disks, selectedDisks, onSelect, headerCheckbox }) => (
  <div className="overflow-x-auto">
    <table className="min-w-full divide-y divide-gray-200">
      <thead className="bg-gray-50">
        <tr>
          <th className="px-3 py-3 text-left text-xs font-medium text-gray-500 uppercase tracking-wider">
            {headerCheckbox}
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
        {disks.map((disk) => {
          const diskKey = `${disk.nodeName}:${disk.pci_address}`;
          return (
            <CompactDiskRow
              key={diskKey}
              disk={disk}
              isSelected={selectedDisks.has(diskKey)}
              onSelect={(selected, shiftKey) => onSelect(diskKey, selected, shiftKey)}
              nodeName={disk.nodeName}
            />
          );
        })}
      </tbody>
    </table>
  </div>
);

interface DiskSetupTabProps {
  // Set when the state-aware landing routed here on a fresh cluster —
  // renders the onboarding callout above the tab.
  onboarding?: boolean;
}

export const DiskSetupTab: React.FC<DiskSetupTabProps> = ({ onboarding = false }) => {
  const { nodeData, refreshNodeDisks, deleteDiskOnNode, setNodeData } = useDiskSetup();
  const { data: dashboardData } = useDashboardData(false); // Get node names from dashboard
  const { setActiveOperationsCount, setActiveSelectionsCount } = useOperations();
  
  // UI State. Dense rows are the default: they stay usable at 100s of
  // disks per node, where the card grid is a wall of tiles.
  const [selectedDisks, setSelectedDisks] = useState<Set<string>>(new Set());
  const [viewMode, setViewMode] = useState<ViewMode>('compact');
  const [groupBy, setGroupBy] = useState<GroupBy>('node');
  const [collapsedGroups, setCollapsedGroups] = useState<Set<string>>(new Set());
  // Groups render at most GROUP_RENDER_CAP disks until explicitly expanded —
  // the status strip still shows every disk, so nothing is invisible.
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(new Set());
  const [currentPage, setCurrentPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);
  const [searchTerm, setSearchTerm] = useState('');
  const [selectedNodes, setSelectedNodes] = useState<string[]>([]);
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all');
  const [sizeFilter, setSizeFilter] = useState<SizeFilter>('all');
  const [showFilters, setShowFilters] = useState(false);
  const [globalRefreshing, setGlobalRefreshing] = useState(false);
  // Anchor for shift-click range selection (last individually toggled disk)
  const lastAnchorRef = useRef<string | null>(null);

  // Setup State. Only options the agent's DiskSetupRequest actually carries
  // — the old huge_pages_mb/driver_override "advanced options" were never
  // read server-side (surfaced by the generated API types).
  const [setupOptions, setSetupOptions] = useState({
    force_unmount: false,
    backup_data: true
  });
  const [unbindDriverInProgress, setUnbindDriverInProgress] = useState<Set<string>>(new Set());
  // Some node-agent error responses carry a top-level `error` next to the
  // DiskSetupResult fields; keep it typed instead of casting at render time.
  const [setupResults, setSetupResults] = useState<Record<string, DiskSetupResult & { error?: string }>>({});

  // Bulk init batch state (plan 2d): one confirmed action, per-disk outcomes
  const [batchItems, setBatchItems] = useState<BatchItem[] | null>(null);
  const [batchRunning, setBatchRunning] = useState(false);
  const [showBulkConfirm, setShowBulkConfirm] = useState(false);
  const batchCancelRef = useRef(false);
  const queryClient = useQueryClient();

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
    const initialData: Record<string, NodeDiskData> = {};
    knownNodes.forEach(node => {
      initialData[node] = { node, disks: [], loading: true };
    });
    setNodeData(initialData);
    refreshAllNodes();
  }, [knownNodes]);

  // Check if any operations are in progress to avoid discovery interference
  const hasActiveOperations = useMemo(() => {
    return batchRunning ||
           unbindDriverInProgress.size > 0 ||
           deleteInProgress.size > 0;
  }, [batchRunning, unbindDriverInProgress, deleteInProgress]);

  // Sync local operation state with global context
  useEffect(() => {
    const totalOperations = (batchRunning ? 1 : 0) +
                            unbindDriverInProgress.size +
                            deleteInProgress.size;
    setActiveOperationsCount(totalOperations);
  }, [batchRunning, unbindDriverInProgress, deleteInProgress, setActiveOperationsCount]);

  // Sync selected disks count with global context to prevent auto-refresh during selection
  useEffect(() => {
    setActiveSelectionsCount(selectedDisks.size);
  }, [selectedDisks, setActiveSelectionsCount]);

  const refreshAllNodes = async () => {
    // Prevent discovery interference during active operations or selections
    if (hasActiveOperations) return;
    if (selectedDisks.size > 0) return;


    setGlobalRefreshing(true);
    const promises = knownNodes.map(node => refreshNodeDisks(node));
    await Promise.allSettled(promises);
    setGlobalRefreshing(false);
  };

  // Track selection state with ref to avoid stale closures
  const selectedDisksRef = useRef(selectedDisks);
  const operationsRef = useRef({ batchRunning, unbindDriverInProgress, deleteInProgress });

  // Update refs when state changes
  useEffect(() => {
    selectedDisksRef.current = selectedDisks;
  }, [selectedDisks]);

  useEffect(() => {
    operationsRef.current = { batchRunning, unbindDriverInProgress, deleteInProgress };
  }, [batchRunning, unbindDriverInProgress, deleteInProgress]);

  // Auto-refresh disk data every 15 seconds, but pause during operations or selections
  useEffect(() => {
    const interval = setInterval(() => {
      // Use refs to get current state without causing re-renders
      const currentHasOperations = operationsRef.current.batchRunning ||
                                  operationsRef.current.unbindDriverInProgress.size > 0 ||
                                  operationsRef.current.deleteInProgress.size > 0;
      
      const currentSelectedCount = selectedDisksRef.current.size;

      // Check only local state - global selections are handled by auto-refresh checkbox
      if (!currentHasOperations && currentSelectedCount === 0) {
        refreshAllNodes();
      }
    }, 15000);

    return () => clearInterval(interval);
  }, [knownNodes]); // Stable dependency - won't recreate interval unnecessarily

  // Flatten all disks from all nodes
  const allDisks = useMemo(() => {
    return Object.entries(nodeData).flatMap(([nodeName, data]) =>
      data.disks.map((disk: UnimplementedDisk) => ({ ...disk, nodeName }))
    );
  }, [nodeData]);

  // Nodes whose agent could not be reached (e.g. CSI node pod not running)
  const unreachableNodes = useMemo(() => {
    return Object.values(nodeData).filter(data => data.error && !data.loading);
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
    if (selectedDiskKey === undefined) return false;
    const colonIndex = selectedDiskKey.indexOf(':');
    const nodeName = selectedDiskKey.substring(0, colonIndex);
    const pciAddr = selectedDiskKey.substring(colonIndex + 1);
    const disk = allDisks.find(d => d.nodeName === nodeName && d.pci_address === pciAddr);
    
    return disk && disk.blobstore_initialized && !disk.is_system_disk;
  }, [selectedDisks, allDisks]);

  // Partition the selection for the bulk-init flow: eligible disks form the
  // batch; the rest are surfaced in the confirm modal with the reason they
  // are excluded. Initialized and system disks never enter a batch.
  const { eligibleBatchDisks, excludedBatchDisks } = useMemo(() => {
    const eligible: BatchDisk[] = [];
    const excluded: ExcludedDisk[] = [];
    for (const diskKey of selectedDisks) {
      const colonIndex = diskKey.indexOf(':');
      const nodeName = diskKey.substring(0, colonIndex);
      const pciAddr = diskKey.substring(colonIndex + 1);
      const disk = allDisks.find(d => d.nodeName === nodeName && d.pci_address === pciAddr);
      if (!disk) continue;
      const batchDisk: BatchDisk = {
        key: diskKey,
        node: nodeName,
        pci: pciAddr,
        device: disk.device_name,
        model: disk.model,
        serial: disk.serial,
        sizeBytes: disk.size_bytes,
      };
      if (isBatchEligible(disk, setupOptions.force_unmount)) {
        eligible.push(batchDisk);
      } else {
        const reason = disk.is_system_disk
          ? 'system disk'
          : disk.blobstore_initialized
            ? 'already initialized (LVS present)'
            : 'has mounted partitions — enable Force Unmount to include';
        excluded.push({ disk: batchDisk, reason });
      }
    }
    return { eligibleBatchDisks: eligible, excludedBatchDisks: excluded };
  }, [selectedDisks, allDisks, setupOptions.force_unmount]);

  const groupedDisks = useMemo(
    () => groupDisks(filteredDisks, groupBy),
    [filteredDisks, groupBy]
  );

  // Fleet-scale landing: once, when the whole fleet has reported, start with
  // all groups collapsed — the status strips still show every disk, and one
  // click opens the node being worked on. Small clusters stay expanded.
  // Decided only after every node finished loading (disks arrive per node,
  // so an early count would undershoot the threshold).
  const autoCollapsed = useRef(false);
  useEffect(() => {
    const nodes = Object.values(nodeData);
    if (autoCollapsed.current || nodes.length === 0 || nodes.some(d => d.loading)) return;
    autoCollapsed.current = true;
    if (allDisks.length > 80 && groupBy !== 'none') {
      setCollapsedGroups(new Set(groupedDisks.map(group => group.key)));
    }
  }, [nodeData, allDisks, groupBy, groupedDisks]);

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
    if (selectedDiskKey === undefined) return null;
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

  // Pagination (flat view only)
  const totalPages = Math.ceil(filteredDisks.length / pageSize);
  const paginatedDisks = filteredDisks.slice((currentPage - 1) * pageSize, currentPage * pageSize);

  // Grouped views cap the rows/cards actually mounted per group; the group's
  // status strip covers the full disk list regardless.
  const GROUP_RENDER_CAP = 60;
  const renderedGroupDisks = (group: DiskGroup<DiskWithNode>): DiskWithNode[] =>
    expandedGroups.has(group.key) ? group.disks : group.disks.slice(0, GROUP_RENDER_CAP);

  // Selectable disk keys in current render order, for shift-click ranges.
  // Mirrors exactly what is on screen: collapsed groups contribute nothing,
  // capped groups only their rendered slice — a shift-range can never grab
  // disks the user cannot see.
  const visibleOrder = useMemo(() => {
    const source = groupBy === 'none'
      ? paginatedDisks
      : groupedDisks.flatMap(group =>
          collapsedGroups.has(group.key) ? [] : renderedGroupDisks(group));
    return source
      .filter(disk => !disk.is_system_disk)
      .map(disk => `${disk.nodeName}:${disk.pci_address}`);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [groupBy, paginatedDisks, groupedDisks, collapsedGroups, expandedGroups]);

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

  const handleDiskSelection = (diskKey: string, selected: boolean, shiftKey: boolean) => {
    const keys = shiftKey ? rangeBetween(visibleOrder, lastAnchorRef.current, diskKey) : [diskKey];
    lastAnchorRef.current = diskKey;
    setSelectedDisks(prev => {
      const newSelection = new Set(prev);
      keys.forEach(key => {
        if (selected) {
          newSelection.add(key);
        } else {
          newSelection.delete(key);
        }
      });
      return newSelection;
    });
  };

  const addDisksToSelection = (disks: DiskWithNode[]) => {
    setSelectedDisks(prev => new Set([
      ...prev,
      ...disks.map(disk => `${disk.nodeName}:${disk.pci_address}`)
    ]));
  };

  const selectAllUninitializedCluster = () => addDisksToSelection(allDisks.filter(isBulkSelectable));
  const selectFilteredUninitialized = () => addDisksToSelection(filteredDisks.filter(isBulkSelectable));
  const selectGroupUninitialized = (group: DiskGroup<DiskWithNode>) =>
    addDisksToSelection(group.disks.filter(isBulkSelectable));
  const deselectGroup = (group: DiskGroup<DiskWithNode>) => {
    setSelectedDisks(prev => {
      const newSelection = new Set(prev);
      group.disks.forEach(disk => newSelection.delete(`${disk.nodeName}:${disk.pci_address}`));
      return newSelection;
    });
  };

  const toggleGroupCollapsed = (groupKey: string) => {
    setCollapsedGroups(prev => {
      const next = new Set(prev);
      if (next.has(groupKey)) {
        next.delete(groupKey);
      } else {
        next.add(groupKey);
      }
      return next;
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

  // One setup call per disk (the agent loops per-PCI server-side anyway),
  // options frozen at confirm time so mid-batch toggles can't change a
  // running batch's behavior.
  const makeSetupOne = (options: typeof setupOptions) =>
    async (disk: BatchDisk): Promise<SetupOneResult> => {
      try {
        const request: DiskSetupRequest = {
          pci_addresses: [disk.pci],
          force_unmount: options.force_unmount,
          backup_data: options.backup_data
        };
        const response = await apiFetch(`/api/nodes/${disk.node}/disks/setup`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(request)
        });
        const result = await response.json().catch(() => null);
        if (response.ok && result?.success) return { ok: true };
        const error = result?.warnings?.length
          ? result.warnings.join('; ')
          : result?.error || `HTTP ${response.status}`;
        return { ok: false, error };
      } catch (error) {
        return { ok: false, error: error instanceof Error ? error.message : 'Connection error' };
      }
    };

  const startBatch = async (disks: BatchDisk[]) => {
    batchCancelRef.current = false;
    setBatchRunning(true);
    setBatchItems(disks.map(disk => ({ disk, status: 'pending' as const })));
    try {
      const finished = await runInitBatch(disks, {
        setupOne: makeSetupOne({ ...setupOptions }),
        onUpdate: items => setBatchItems(items),
        isCancelled: () => batchCancelRef.current,
        // Refresh a node's disk list once when its queue drains, not per disk
        onNodeDrained: node => { refreshNodeDisks(node); }
      });
      setSelectedDisks(prev => {
        const newSelection = new Set(prev);
        finished.forEach(item => {
          if (item.status === 'ok') newSelection.delete(item.disk.key);
        });
        return newSelection;
      });
    } finally {
      setBatchRunning(false);
      queryClient.invalidateQueries({ queryKey: ['dashboard'] });
    }
  };

  const retryFailedBatch = () => {
    if (!batchItems) return;
    const failed = batchItems.filter(item => item.status === 'failed').map(item => item.disk);
    if (failed.length > 0) startBatch(failed);
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

    // Fan out across nodes, same as setupSelectedDisks; per-node disks
    // stay batched in one sequential request.
    await Promise.all(
      Object.entries(disksByNode).map(async ([nodeName, disks]) => {
        if (disks && disks.length > 0) {
          await unbindDriverOnNodeWrapper(nodeName, disks.map(d => d.pciAddr));
        }
      })
    );
  };

  const unbindDriverOnNodeWrapper = async (node: string, diskPciAddresses: string[]) => {
    setUnbindDriverInProgress(prev => new Set([...prev, node]));

    try {
      // Call reset/unbind API endpoint
      const response = await apiFetch(`/api/nodes/${node}/disks/reset`, {
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
        failed_disks: diskPciAddresses,
        warnings: [error instanceof Error ? error.message : 'Unknown error'],
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
      {/* Fresh-cluster onboarding (state-aware landing routed here) */}
      {onboarding && (
        <div className="bg-brand-50 border border-brand-200 rounded-lg p-4">
          <div className="flex items-start gap-3">
            <Info className="w-6 h-6 text-brand-600 mt-0.5 flex-shrink-0" />
            <div>
              <p className="font-medium text-brand-900">Welcome — no storage is initialized yet</p>
              <p className="text-sm text-brand-800 mt-1">
                Flint found no logical volume stores on this cluster, so you landed here.
                Select the disks flint should manage and initialize them to start
                provisioning volumes. System disks are excluded automatically, and any
                disk or node you leave unselected is simply skipped.
              </p>
            </div>
          </div>
        </div>
      )}

      {/* Header */}
      <div className="bg-white rounded-lg shadow p-6">
        <div className="flex items-center justify-between mb-6">
          <div className="flex items-center gap-3">
            <HardDrive className="w-8 h-8 text-brand-600" />
            <div>
              <h2 className="text-page-title text-gray-900">Disk Setup for SPDK</h2>
              <p className="text-gray-600">Initialize NVMe disks across {knownNodes.length} nodes</p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <IconButton
              icon={RefreshCw}
              aria-label="Refresh disk discovery on all nodes"
              onClick={refreshAllNodes}
              disabled={globalRefreshing}
              iconClass={globalRefreshing ? 'animate-spin' : ''}
            />
          </div>
        </div>

        {/* Statistics Cards */}
        <div className="flex gap-4">
          {/* Info Cards - Smaller, stacked */}
          <div className="flex flex-col gap-2">
            <div className="bg-gray-50 border border-gray-200 rounded p-2 text-center min-w-[100px]">
              <Database className="w-4 h-4 text-gray-600 mx-auto mb-1" />
              <p className="text-sm font-semibold text-gray-700">{stats.total}</p>
              <p className="text-xs text-gray-500">Total</p>
            </div>
            <div className="bg-gray-50 border border-gray-200 rounded p-2 text-center min-w-[100px]">
              <Monitor className="w-4 h-4 text-gray-600 mx-auto mb-1" />
              <p className="text-sm font-semibold text-gray-700">{stats.selected}</p>
              <p className="text-xs text-gray-500">Selected</p>
            </div>
          </div>
          
          {/* Disk State Cards - clear progression from gray to green */}
          <div className="flex-1 grid grid-cols-2 md:grid-cols-5 gap-4">
            <div 
              className={`bg-gray-50 rounded-lg p-4 text-center border border-gray-200 cursor-pointer hover:shadow-md transition-all ${
                statusFilter === 'free' ? 'ring-2 ring-gray-500 shadow-md' : ''
              }`}
              onClick={() => {
                setStatusFilter(statusFilter === 'free' ? 'all' : 'free');
                setCurrentPage(1);
              }}
            >
              <CheckCircle className="w-6 h-6 text-gray-600 mx-auto mb-2" />
              <p className="text-stat text-gray-600">{stats.free}</p>
              <p className="text-sm text-gray-600">Free</p>
            </div>
            <div 
              className={`bg-brand-50 rounded-lg p-4 text-center border border-brand-200 cursor-pointer hover:shadow-md transition-all ${
                statusFilter === 'driver-bound' ? 'ring-2 ring-brand-500 shadow-md' : ''
              }`}
              onClick={() => {
                setStatusFilter(statusFilter === 'driver-bound' ? 'all' : 'driver-bound');
                setCurrentPage(1);
              }}
            >
              <Settings className="w-6 h-6 text-brand-600 mx-auto mb-2" />
              <p className="text-stat text-brand-600">{stats.driverBound}</p>
              <p className="text-sm text-gray-600">Driver Bound</p>
            </div>
            <div 
              className={`bg-degraded-50 rounded-lg p-4 text-center border border-degraded-200 cursor-pointer hover:shadow-md transition-all ${
                statusFilter === 'needs-unmount' ? 'ring-2 ring-degraded-500 shadow-md' : ''
              }`}
              onClick={() => {
                setStatusFilter(statusFilter === 'needs-unmount' ? 'all' : 'needs-unmount');
                setCurrentPage(1);
              }}
            >
              <AlertTriangle className="w-6 h-6 text-degraded-600 mx-auto mb-2" />
              <p className="text-stat text-degraded-600">{stats.needsUnmount}</p>
              <p className="text-sm text-gray-600">Needs Unmount</p>
            </div>
            <div 
              className={`bg-failed-50 rounded-lg p-4 text-center border border-failed-200 cursor-pointer hover:shadow-md transition-all ${
                statusFilter === 'system' ? 'ring-2 ring-failed-500 shadow-md' : ''
              }`}
              onClick={() => {
                setStatusFilter(statusFilter === 'system' ? 'all' : 'system');
                setCurrentPage(1);
              }}
            >
              <Shield className="w-6 h-6 text-failed-600 mx-auto mb-2" />
              <p className="text-stat text-failed-600">{stats.system}</p>
              <p className="text-sm text-gray-600">System Disks</p>
            </div>
            <div 
              className={`bg-healthy-50 rounded-lg p-4 text-center border border-healthy-200 cursor-pointer hover:shadow-md transition-all ${
                statusFilter === 'lvs-ready' ? 'ring-2 ring-healthy-500 shadow-md' : ''
              }`}
              onClick={() => {
                setStatusFilter(statusFilter === 'lvs-ready' ? 'all' : 'lvs-ready');
                setCurrentPage(1);
              }}
            >
              <CheckCircle className="w-6 h-6 text-healthy-600 mx-auto mb-2" />
              <p className="text-stat text-healthy-600">{stats.lvsReady}</p>
              <p className="text-sm text-gray-600">LVS Ready</p>
            </div>
          </div>
        </div>
      </div>

      {/* Unreachable Nodes Warning */}
      {unreachableNodes.length > 0 && (
        <div className="bg-failed-50 border border-failed-200 rounded-lg p-4">
          <div className="flex items-start gap-3">
            <AlertTriangle className="w-5 h-5 text-failed-600 mt-0.5 flex-shrink-0" />
            <div className="min-w-0">
              <p className="font-medium text-failed-800">
                Disk information unavailable for {unreachableNodes.length} of {knownNodes.length} nodes
              </p>
              <ul className="mt-1 text-sm text-failed-700 space-y-0.5">
                {unreachableNodes.map(data => (
                  <li key={data.node} className="truncate" title={data.error}>
                    <span className="font-mono font-medium">{data.node}</span>: {data.error}
                  </li>
                ))}
              </ul>
              <p className="mt-2 text-xs text-failed-600">
                Check that the flint-csi-node pod is running on these nodes. Only disks from reachable nodes are shown below.
              </p>
            </div>
          </div>
        </div>
      )}

      {/* Filters and Controls */}
      <div className="bg-white rounded-lg shadow">
        {/* Filter Header */}
        <div className="px-6 py-4 border-b border-gray-200 flex items-center justify-between">
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2">
              <Filter className="w-5 h-5 text-gray-600" />
              <span className="font-medium">Filters</span>
              {activeFilterCount > 0 && (
                <span className="px-2 py-1 text-xs bg-brand-100 text-brand-800 rounded-full">
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
              <Button
                variant="link"
                className="text-gray-600 hover:text-gray-800"
                onClick={clearAllFilters}
              >
                Clear All
              </Button>
            )}
            <Button variant="link" onClick={() => setShowFilters(!showFilters)}>
              {showFilters ? 'Hide' : 'Show'} Filters
            </Button>
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
              className="w-full pl-10 pr-4 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-brand-500"
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
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-brand-500"
                >
                  <option value="all">All Disks</option>
                  <option value="free">Free</option>
                  <option value="driver-bound">Driver Bound</option>
                  <option value="lvs-ready">LVS Ready</option>
                  <option value="needs-unmount">Needs Unmount</option>
                  <option value="system">System Disks</option>
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
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-brand-500"
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
              <SegmentedControl
                aria-label="Disk view mode"
                size="sm"
                iconOnly
                value={viewMode}
                onChange={setViewMode}
                options={[
                  { value: 'grid', label: 'Grid view', icon: Grid },
                  { value: 'compact', label: 'Compact view', icon: List },
                ]}
              />
            </div>

            {/* Group By */}
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium text-gray-700">Group by:</span>
              <select
                value={groupBy}
                onChange={(e) => {
                  setGroupBy(e.target.value as GroupBy);
                  setCollapsedGroups(new Set());
                  setExpandedGroups(new Set());
                  setCurrentPage(1);
                }}
                className="border border-gray-300 rounded px-2 py-1 text-sm"
              >
                <option value="node">Node</option>
                <option value="class">Disk class</option>
                <option value="none">None (paged)</option>
              </select>
            </div>

            {/* Page Size (flat view only) */}
            {groupBy === 'none' && (
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
            )}
          </div>

          {/* Pagination (flat) / group summary (grouped) */}
          {groupBy === 'none' ? (
            <div className="flex items-center gap-2">
              <span className="text-sm text-gray-700">
                {((currentPage - 1) * pageSize) + 1}-{Math.min(currentPage * pageSize, filteredDisks.length)} of {filteredDisks.length}
              </span>
              <IconButton
                icon={ChevronLeft}
                aria-label="Previous page"
                className="p-1"
                iconClass="w-4 h-4"
                onClick={() => setCurrentPage(prev => Math.max(1, prev - 1))}
                disabled={currentPage === 1}
              />
              <span className="px-2 py-1 text-sm">{currentPage} / {totalPages}</span>
              <IconButton
                icon={ChevronRight}
                aria-label="Next page"
                className="p-1"
                iconClass="w-4 h-4"
                onClick={() => setCurrentPage(prev => Math.min(totalPages, prev + 1))}
                disabled={currentPage === totalPages}
              />
            </div>
          ) : (
            <div className="text-sm text-gray-500">
              {filteredDisks.length} disk{filteredDisks.length !== 1 ? 's' : ''} in {groupedDisks.length} group{groupedDisks.length !== 1 ? 's' : ''}
            </div>
          )}
        </div>
        {groupBy !== 'none' && (
          <div className="mt-3 pt-3 border-t border-gray-100 flex items-center gap-4 text-xs text-gray-500">
            <span>Status strip:</span>
            {STRIP_LEGEND.map(({ label, cell }) => (
              <span key={label} className="flex items-center gap-1">
                <span className={`w-2.5 h-2.5 rounded-sm ${cell}`} />
                {label}
              </span>
            ))}
            <span className="ml-auto">Click a cell to select · shift-click a checkbox for ranges</span>
          </div>
        )}
      </div>

      {/* Selection and Setup Controls */}
      {selectedDisks.size > 0 && (
        <div className="bg-white rounded-lg shadow p-4">
          <div className="flex items-center justify-between mb-4">
            <div className="flex items-center gap-4">
              <span className="text-sm font-medium text-gray-700">
                {selectedDisks.size} disk{selectedDisks.size !== 1 ? 's' : ''} selected
              </span>
              <Button
                variant="link"
                className="text-gray-600 hover:text-gray-800"
                onClick={() => setSelectedDisks(new Set())}
              >
                Clear Selection
              </Button>
            </div>
            <div className="flex items-center gap-2">
              {canDeleteSelectedDisk && (
                <Button
                  variant="danger"
                  icon={deleteInProgress.size > 0 ? Settings : Trash2}
                  iconClass={deleteInProgress.size > 0 ? 'animate-spin' : ''}
                  onClick={handleDeleteDisk}
                  disabled={deleteInProgress.size > 0}
                >
                  {deleteInProgress.size > 0 ? 'Deleting...' : 'Delete SPDK Disk'}
                </Button>
              )}
              {eligibleBatchDisks.length > 0 && (
                <Button
                  variant="primary"
                  icon={batchRunning ? Settings : Play}
                  iconClass={batchRunning ? 'animate-spin' : ''}
                  onClick={() => setShowBulkConfirm(true)}
                  disabled={batchRunning}
                  title="Initialize the eligible selected disks (confirmation follows)"
                >
                  {batchRunning
                    ? 'Initializing...'
                    : `Initialize ${eligibleBatchDisks.length} disk${eligibleBatchDisks.length !== 1 ? 's' : ''}`}
                </Button>
              )}
              {eligibleBatchDisks.length === 0 && excludedBatchDisks.length > 0 && (
                <span className="text-sm text-gray-500">
                  No selected disk is eligible for initialization
                </span>
              )}
              {canUnbindDriver && (
                <Button
                  variant="secondary"
                  icon={unbindDriverInProgress.size > 0 ? Settings : RefreshCw}
                  iconClass={unbindDriverInProgress.size > 0 ? 'animate-spin' : ''}
                  onClick={unbindDriverSelectedDisks}
                  disabled={unbindDriverInProgress.size > 0}
                  title="Unbind SPDK driver from selected disks"
                >
                  {unbindDriverInProgress.size > 0 ? 'Unbinding...' : 'Unbind SPDK'}
                </Button>
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
        </div>
      )}



      {/* Batch progress (live per-disk outcomes) */}
      {batchItems && (
        <BatchProgressPanel
          items={batchItems}
          running={batchRunning}
          onCancel={() => { batchCancelRef.current = true; }}
          onRetryFailed={retryFailedBatch}
          onDismiss={() => setBatchItems(null)}
        />
      )}

      {/* Bulk Actions */}
      <div className="bg-white rounded-lg shadow p-4">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="text-sm font-medium text-gray-700">Bulk Actions:</span>
            <Button
              variant="link"
              onClick={selectAllUninitializedCluster}
              title="Select every uninitialized, unmounted, non-system disk on every node"
            >
              Select all uninitialized (cluster)
            </Button>
            {activeFilterCount > 0 && (
              <>
                <span className="text-gray-300">|</span>
                <Button
                  variant="link"
                  onClick={selectFilteredUninitialized}
                  title="Select the uninitialized disks matching the active filters"
                >
                  Select filtered uninitialized
                </Button>
              </>
            )}
            {groupBy === 'none' && (
              <>
                <span className="text-gray-300">|</span>
                <Button variant="link" onClick={() => handleSelectAll(true)}>
                  Select All (This Page)
                </Button>
              </>
            )}
            <span className="text-gray-300">|</span>
            <Button variant="link" onClick={() => setSelectedDisks(new Set())}>
              Deselect All
            </Button>
          </div>

          <div className="text-sm text-gray-500">
            {groupBy === 'none'
              ? `Page ${currentPage} of ${totalPages} • ${paginatedDisks.length} items`
              : 'Tip: shift-click a checkbox to select a range'}
          </div>
        </div>
      </div>

      {/* Setup Results */}
      {Object.keys(setupResults).length > 0 && (
        <div className="bg-white rounded-lg shadow p-4">
          <h3 className="text-section mb-4">Recent Setup Results</h3>
          <div className="space-y-3">
            {Object.entries(setupResults).map(([node, result]) => (
              <div key={node} className={`p-3 rounded border-l-4 ${
                result.success ? 'border-healthy-500 bg-healthy-50' : 'border-failed-500 bg-failed-50'
              }`}>
                <div className="flex items-center justify-between mb-2">
                  <span className="font-medium">{node}</span>
                  <span className="text-xs text-gray-500">
                    {new Date(result.completed_at).toLocaleString()}
                  </span>
                </div>
                {result.setup_disks && result.setup_disks.length > 0 && (
                  <div className="text-sm text-healthy-700 mb-1">
                    ✓ Setup: {result.setup_disks.length} disk{result.setup_disks.length !== 1 ? 's' : ''}
                  </div>
                )}
                {result.failed_disks && result.failed_disks.length > 0 && (
                  <div className="text-sm text-failed-700 mb-1">
                    ✗ Failed: {result.failed_disks.length} disk{result.failed_disks.length !== 1 ? 's' : ''}
                  </div>
                )}
                {result.warnings && result.warnings.length > 0 && (
                  <div className="text-sm text-degraded-700">
                    ⚠ {result.warnings.join(', ')}
                  </div>
                )}
                {result.error && (
                  <div className="text-sm text-failed-700">
                    ❌ Error: {result.error}
                  </div>
                )}
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Disk Display */}
      <div className="bg-white rounded-lg shadow">
        {filteredDisks.length === 0 ? (
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
        ) : groupBy === 'none' ? (
          viewMode === 'grid' ? (
            <div className="p-6">
              <DiskGrid
                disks={paginatedDisks}
                selectedDisks={selectedDisks}
                onSelect={handleDiskSelection}
              />
            </div>
          ) : (
            <DiskTable
              disks={paginatedDisks}
              selectedDisks={selectedDisks}
              onSelect={handleDiskSelection}
              headerCheckbox={
                <input
                  type="checkbox"
                  checked={paginatedDisks.filter(d => !d.is_system_disk).length > 0 &&
                           paginatedDisks.filter(d => !d.is_system_disk).every(d => selectedDisks.has(`${d.nodeName}:${d.pci_address}`))}
                  onChange={(e) => handleSelectAll(e.target.checked)}
                  className="rounded"
                />
              }
            />
          )
        ) : (
          <div className="divide-y divide-gray-200">
            {groupedDisks.map(group => {
              const uninitCount = group.disks.filter(isBulkSelectable).length;
              const selectedInGroup = group.disks.filter(
                disk => selectedDisks.has(`${disk.nodeName}:${disk.pci_address}`)
              ).length;
              const collapsed = collapsedGroups.has(group.key);
              const visibleDisks = renderedGroupDisks(group);
              const hiddenCount = group.disks.length - visibleDisks.length;
              return (
                <div
                  key={group.key}
                  style={{ contentVisibility: 'auto', containIntrinsicSize: '0 320px' }}
                >
                  <div className="px-6 py-3 bg-gray-50 space-y-2">
                    <div className="flex items-center justify-between">
                      <button
                        onClick={() => toggleGroupCollapsed(group.key)}
                        className="flex items-center gap-2 text-left"
                      >
                        {collapsed
                          ? <ChevronRight className="w-4 h-4 text-gray-500" />
                          : <ChevronDown className="w-4 h-4 text-gray-500" />}
                        <span className="font-medium text-gray-900">{group.label}</span>
                        <span className="text-sm text-gray-500">
                          {uninitCount} uninitialized / {group.disks.length} total
                        </span>
                        {selectedInGroup > 0 && (
                          <span className="px-2 py-0.5 text-xs bg-brand-100 text-brand-800 rounded-full">
                            {selectedInGroup} selected
                          </span>
                        )}
                      </button>
                      <div className="flex items-center gap-3">
                        {uninitCount > 0 && (
                          <Button
                            variant="link"
                            onClick={() => selectGroupUninitialized(group)}
                          >
                            Select uninitialized ({uninitCount})
                          </Button>
                        )}
                        {selectedInGroup > 0 && (
                          <Button
                            variant="link"
                            className="text-gray-600 hover:text-gray-800"
                            onClick={() => deselectGroup(group)}
                          >
                            Deselect
                          </Button>
                        )}
                      </div>
                    </div>
                    {/* Every disk in the group, one cell each — the scan
                        surface that stays honest however many rows render */}
                    <DiskStatusStrip
                      disks={group.disks}
                      selectedDisks={selectedDisks}
                      onToggle={(diskKey, selected) => handleDiskSelection(diskKey, selected, false)}
                    />
                  </div>
                  {!collapsed && (
                    <>
                      {viewMode === 'grid' ? (
                        <div className="p-6">
                          <DiskGrid
                            disks={visibleDisks}
                            selectedDisks={selectedDisks}
                            onSelect={handleDiskSelection}
                          />
                        </div>
                      ) : (
                        <DiskTable
                          disks={visibleDisks}
                          selectedDisks={selectedDisks}
                          onSelect={handleDiskSelection}
                        />
                      )}
                      {hiddenCount > 0 && (
                        <div className="px-6 py-3 border-t border-gray-100 bg-gray-50 flex items-center justify-between">
                          <span className="text-sm text-gray-500">
                            Showing first {visibleDisks.length} of {group.disks.length} disks
                            (all are selectable via the strip above)
                          </span>
                          <Button
                            variant="secondary"
                            size="sm"
                            onClick={() =>
                              setExpandedGroups(prev => new Set([...prev, group.key]))
                            }
                          >
                            Show all {group.disks.length}
                          </Button>
                        </div>
                      )}
                    </>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </div>

      {/* Bottom Pagination */}
      {groupBy === 'none' && totalPages > 1 && (
        <div className="bg-white rounded-lg shadow p-4">
          <div className="flex items-center justify-between">
            <div className="text-sm text-gray-700">
              Showing {((currentPage - 1) * pageSize) + 1} to {Math.min(currentPage * pageSize, filteredDisks.length)} of {filteredDisks.length} results
            </div>
            <div className="flex items-center gap-2">
              <Button size="sm" onClick={() => setCurrentPage(1)} disabled={currentPage === 1}>
                First
              </Button>
              <Button
                size="sm"
                onClick={() => setCurrentPage(prev => Math.max(1, prev - 1))}
                disabled={currentPage === 1}
              >
                Previous
              </Button>

              {/* Page numbers */}
              {Array.from({ length: Math.min(5, totalPages) }, (_, i) => {
                const pageNum = Math.max(1, Math.min(totalPages - 4, currentPage - 2)) + i;
                return (
                  <Button
                    key={pageNum}
                    size="sm"
                    variant={pageNum === currentPage ? 'primary' : 'secondary'}
                    onClick={() => setCurrentPage(pageNum)}
                  >
                    {pageNum}
                  </Button>
                );
              })}

              <Button
                size="sm"
                onClick={() => setCurrentPage(prev => Math.min(totalPages, prev + 1))}
                disabled={currentPage === totalPages}
              >
                Next
              </Button>
              <Button
                size="sm"
                onClick={() => setCurrentPage(totalPages)}
                disabled={currentPage === totalPages}
              >
                Last
              </Button>
            </div>
          </div>
        </div>
      )}

      {/* Information Panel */}
      <div className="bg-brand-50 border border-brand-200 rounded-lg p-6">
        <div className="flex items-start gap-3">
          <Info className="w-6 h-6 text-brand-600 mt-1 flex-shrink-0" />
          <div>
            <h4 className="font-medium text-brand-900 mb-2">SPDK Disk Setup Process</h4>
            <div className="text-sm text-brand-800 space-y-2">
              <p>
                <strong>What this does:</strong> Prepares NVMe disks for SPDK usage by unbinding them from the kernel 
                NVMe driver and binding them to a userspace-compatible driver.
              </p>
              <p>
                <strong>Scale:</strong> Each node group shows a status strip with one cell per disk
                and renders the first {GROUP_RENDER_CAP} rows until expanded, so the page stays responsive with
                hundreds of disks. Filters, search, and bulk selection operate on all disks, not
                just the rendered rows.
              </p>
              <p>
                <strong>Safety:</strong> System disks are automatically excluded. Use filters to focus on specific 
                nodes or disk types before performing bulk operations.
              </p>
            </div>
          </div>
        </div>
      </div>

      {/* Bulk Initialization Confirmation */}
      {showBulkConfirm && (
        <BulkConfirmModal
          disks={eligibleBatchDisks}
          excluded={excludedBatchDisks}
          onCancel={() => setShowBulkConfirm(false)}
          onConfirm={() => {
            setShowBulkConfirm(false);
            startBatch(eligibleBatchDisks);
          }}
        />
      )}

      {/* Delete Confirmation Dialog (kit ConfirmModal). Only what the agent's
          /disks/delete actually does is described — it refuses with a 409
          while lvols exist, so there are no migrate/snapshot options here. */}
      {showDeleteConfirmation && diskToDelete && (
        <ConfirmModal
          title="Delete SPDK Disk"
          subtitle={`${diskToDelete.diskName} on ${diskToDelete.nodeName}`}
          danger={
            <>
              <strong>This cannot be undone.</strong> The logical volume store on
              this disk is destroyed and the disk is returned to the kernel
              driver. Deletion is refused while any logical volumes still live
              on the disk.
            </>
          }
          confirmLabel="Delete Disk"
          confirmPhrase={diskToDelete.diskName}
          onConfirm={confirmDeleteDisk}
          onCancel={() => {
            setShowDeleteConfirmation(false);
            setDiskToDelete(null);
          }}
        >
          <div className="space-y-2 text-sm mb-4 bg-gray-50 rounded-lg p-4">
            <div><strong>Device:</strong> <span className="font-mono">{diskToDelete.diskName}</span></div>
            <div><strong>Node:</strong> {diskToDelete.nodeName}</div>
            <div><strong>Model:</strong> {diskToDelete.model}</div>
            <div><strong>Size:</strong> {diskToDelete.size}GB</div>
          </div>
        </ConfirmModal>
      )}
    </div>
  );
};
