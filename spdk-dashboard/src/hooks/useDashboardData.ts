import { useState, useEffect, useCallback, useMemo } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import * as api from '../api/client';
import { apiFetch } from '../api/client';

const EMPTY_DASHBOARD: DashboardData = {
  volumes: [],
  raw_volumes: [],
  disks: [],
  nodes: [],
};

// Fetch + transform the aggregate; throws on any non-JSON / non-OK response so
// react-query surfaces the error and keeps the last good data on screen. No
// mock fallback — a healthy-looking dashboard during an outage hides the
// outage (2026-06-12 incident).
const fetchDashboard = async (filters?: DashboardFilters): Promise<DashboardData> => {
  const response = await apiFetch(`/api/dashboard${buildQueryString(filters)}`);
  const contentType = response.headers.get('content-type') || '';
  if (!response.ok || contentType.indexOf('application/json') === -1) {
    throw new Error(
      response.ok ? 'Received non-JSON response from backend' : `Backend error (HTTP ${response.status})`
    );
  }
  return transformBackendData(await response.json());
};

export const computeStats = (data: DashboardData): DashboardStats => {
  const healthyVolumes = data.volumes.filter(v => v.state === 'Healthy').length;
  const degradedVolumes = data.volumes.filter(v => v.state === 'Degraded').length;
  const failedVolumes = data.volumes.filter(v => v.state === 'Failed').length;
  const faultedVolumes = degradedVolumes + failedVolumes;

  const volumesWithRebuilding = data.volumes.filter(v =>
    v.replica_statuses.some(replica =>
      replica.status === 'rebuilding' ||
      replica.rebuild_progress !== null ||
      replica.is_new_replica
    ) || v.raid_status?.rebuild_info !== undefined
  ).length;

  const localNVMeVolumes = data.volumes.filter(v => v.local_nvme).length;
  const orphanedVolumes = data.raw_volumes.length;
  const healthyDisks = data.disks.filter(d => d.healthy).length;
  const formattedDisks = data.disks.filter(d => d.blobstore_initialized).length;

  return {
    totalVolumes: data.volumes.length + data.raw_volumes.length,
    healthyVolumes,
    degradedVolumes,
    failedVolumes,
    faultedVolumes,
    volumesWithRebuilding,
    localNVMeVolumes,
    orphanedVolumes,
    totalDisks: data.disks.length,
    healthyDisks,
    formattedDisks,
  };
};

// --- Start of new/updated interfaces ---

// Represents NVMe-oF target information from the backend
export interface NvmeofTargetInfo {
  nqn: string;
  target_ip: string;
  target_port: number;
  transport: string;
  node: string;
  bdev_name: string;
  active: boolean;
  connection_count: number;
}

export interface Volume {
  id: string;
  name: string;
  size: string;
  state: string;
  replicas: number;
  active_replicas: number;
  local_nvme: boolean;
  access_method: string;
  rebuild_progress: number | null;
  nodes: string[];
  replica_statuses: ReplicaStatus[];
  nvmeof_targets: NvmeofTargetInfo[];
  nvmeof_enabled: boolean;
  raid_status?: RaidStatus;
  
  // Add ublk device information
  ublk_device?: {
    id: number;
    device_path: string;  // e.g., "/dev/ublkb42"
  };
  
  // SPDK validation status for frontend display
  spdk_validation_status: SpdkValidationStatus;
  
  // PV/PVC information for managed volumes
  pvc_info?: PvcInfo;
}

export interface SpdkValidationStatus {
  has_spdk_backing: boolean;
  validation_message?: string;
  validation_severity: 'info' | 'warning' | 'error';
}

export interface SpdkVolumeDetails {
  volume_name: string;
  volume_uuid: string;
  lvs_name: string;
  lvs_uuid: string;
  node: string;
  // Volume-specific information
  allocated_clusters: number;
  cluster_size: number;
  size_bytes: number;
  size_gb: number;
  is_thin_provisioned: boolean;
  is_clone: boolean;
  is_snapshot: boolean;
  // LVS information
  lvs_total_clusters: number;
  lvs_free_clusters: number;
  lvs_block_size: number;
  lvs_base_bdev: string;
  lvs_capacity_gb: number;
  lvs_used_gb: number;
  lvs_utilization_pct: number;
  // SPDK bdev information
  bdev_name: string;
  bdev_alias?: string;
  // Additional metadata
  last_updated: string;
}

// --- End of new/updated interfaces ---


export interface RaidStatus {
  raid_level: number;
  state: string;
  num_members: number;
  operational_members: number;
  discovered_members: number;
  members: RaidMember[];
  rebuild_info?: RebuildInfo;
  superblock_version?: number;
  auto_rebuild_enabled: boolean;
}

export interface RaidMember {
  slot: number;
  name: string;
  state: string;
  uuid?: string;
  is_configured: boolean;
  node?: string;
  disk_ref?: string;
  health_status: string;
}

export interface RebuildInfo {
  state: string;
  target_slot: number;
  source_slot: number;
  blocks_remaining: number;
  blocks_total: number;

  progress_percentage: number;
  estimated_time_remaining?: string;
  start_time?: string;
}

export interface ReplicaStatus {
  node: string;
  status: string;
  is_local: boolean;
  last_io_timestamp: string | null;
  rebuild_progress: number | null;
  rebuild_target: string | null;
  is_new_replica: boolean;
  nvmf_target: NvmfTarget | null;
  access_method: string;
  // Enhanced replica storage details from backend
  raid_member_slot?: number;
  raid_member_state: string;
  lvol_uuid?: string;
  disk_ref?: string;
  replica_size?: number;
}

export interface NvmfTarget {
  nqn: string;
  target_ip: string;
  target_port: string;
  transport_type: string;
}

export interface Disk {
  id: string;
  node: string;
  pci_addr: string;
  capacity: number; // bytes
  capacity_gb: number; // GB
  allocated_space: number; // GB (not bytes!)
  free_space: number; // GB (not bytes!)
  free_space_display: string;
  healthy: boolean;
  blobstore_initialized: boolean; // Matches backend field name
  lvol_count: number;
  model: string;
  read_iops: number;
  write_iops: number;
  read_latency: number;
  write_latency: number;
  brought_online: string;
  provisioned_volumes: ProvisionedVolume[];
  // Orphaned SPDK volumes on this disk
  orphaned_spdk_volumes: OrphanedVolumeInfo[];
  device_type: string; // "NVMe", "SCSI/SATA", "VirtIO", "IDE", "Unknown"
}

export interface ProvisionedVolume {
  volume_name: string;
  volume_id: string;
  size: number;
  provisioned_at: string;
  replica_type: string;
  status: string;
}

export interface OrphanedVolumeInfo {
  spdk_volume_name: string;
  spdk_volume_uuid: string;
  size_blocks: number;
  size_gb: number;
  orphaned_since: string;
}

export interface RawSpdkVolume {
  name: string;
  uuid: string;
  node: string;
  lvs_name: string;
  size_blocks: number;
  size_gb: number;
  is_managed: boolean;
}

export interface PvcInfo {
  pvc_name: string;
  pvc_namespace: string;
  pv_name: string;
  storage_class: string;
  access_modes: string[];
  claim_status: string;
  created_at: string;
}

export interface NodeInfo {
  name: string;
  memory_total_mb: number;
  memory_available_mb: number;
  memory_used_mb: number;
  memory_utilization_pct: number;
}

export interface DashboardData {
  volumes: Volume[];
  raw_volumes: RawSpdkVolume[];
  disks: Disk[];
  nodes: string[];
  node_info?: Record<string, NodeInfo>;  // Optional for backward compatibility
}

export interface DashboardStats {
  totalVolumes: number;
  healthyVolumes: number;
  degradedVolumes: number;
  failedVolumes: number;
  faultedVolumes: number;
  volumesWithRebuilding: number;
  localNVMeVolumes: number;
  orphanedVolumes: number;
  totalDisks: number;
  healthyDisks: number;
  formattedDisks: number;
}

export type VolumeFilter = 
  | 'all' 
  | 'orphaned'     // Show only raw/orphaned volumes
  | 'healthy' 
  | 'degraded' 
  | 'failed' 
  | 'faulted'
  | 'rebuilding'
  | 'local-nvme';

export type DiskFilter = string | null;
export type VolumeReplicaFilter = string | null;


// Backend filter options interface
export interface DashboardFilters {
  volumeFilter?: VolumeFilter;
  volumeNode?: string;
  diskNode?: string;
  diskInitialized?: boolean;
  nodesWithDisksOnly?: boolean; // Show only nodes that have disks
  node?: string; // Global node filter
}

// Build query string from filters
const buildQueryString = (filters?: DashboardFilters): string => {
  if (!filters) return '';
  
  const params = new URLSearchParams();
  
  if (filters.volumeFilter && filters.volumeFilter !== 'all') {
    params.append('volume_filter', filters.volumeFilter);
  }
  if (filters.volumeNode) {
    params.append('volume_node', filters.volumeNode);
  }
  if (filters.diskNode) {
    params.append('disk_node', filters.diskNode);
  }
  if (filters.diskInitialized !== undefined) {
    params.append('disk_initialized', filters.diskInitialized.toString());
  }
  if (filters.nodesWithDisksOnly !== undefined) {
    params.append('nodes_with_disks_only', filters.nodesWithDisksOnly.toString());
  }
  if (filters.node) {
    params.append('node', filters.node);
  }
  
  const queryString = params.toString();
  return queryString ? `?${queryString}` : '';
};

// Enhanced hook implementation with API integration and backend filtering
export const useDashboardData = (autoRefresh: boolean = true, filters?: DashboardFilters) => {
  const query = useQuery({
    queryKey: ['dashboard', filters ?? null],
    queryFn: () => fetchDashboard(filters),
    refetchInterval: autoRefresh ? 30_000 : false,
  });

  const data = query.data ?? EMPTY_DASHBOARD;
  const stats = useMemo(() => computeStats(data), [data]);

  return {
    data,
    // Only "loading" before the first successful fetch; a refetch keeps the
    // prior data visible rather than flashing a spinner.
    loading: query.isLoading,
    stats,
    refreshData: query.refetch,
    // Mock data is gone; kept in the return shape as a constant so existing
    // consumers compile unchanged (removed from the UI in a follow-up).
    usingMockData: false,
    connectionError: query.isError ? (query.error as Error).message : null,
  };
};

// Transform backend data structure to frontend interface
const transformBackendData = (backendData: any): DashboardData => {
  return {
    volumes: backendData.volumes?.map((vol: any) => ({
      ...vol,
      // Ensure all fields are properly mapped with safe defaults
      raid_level: vol.raid_status?.raid_level ? `RAID-${vol.raid_status.raid_level}` : undefined,
      primary_replica_uuid: vol.primary_lvol_uuid,
      nvmeof_targets: vol.nvmeof_targets || [],
      replica_statuses: vol.replica_statuses || [],  // Ensure array exists
      nodes: vol.nodes || [],  // Ensure array exists
    })) || [],
    raw_volumes: backendData.raw_volumes || [],
    disks: backendData.disks?.map((disk: any) => {
      // Backend already returns capacity_gb and free_space in GB (not bytes!)
      const sizeGB = disk.capacity_gb || Math.round((disk.capacity || 0) / (1024 * 1024 * 1024));
      const freeGB = disk.free_space || 0;  // Already in GB from backend
      const allocatedGB = disk.allocated_space || (sizeGB - freeGB);
      
      return {
        ...disk,
        // Backend fields are already in correct format
        capacity_gb: sizeGB,
        allocated_space: allocatedGB,
        free_space: freeGB,
        free_space_display: disk.free_space_display || `${freeGB}GB`,
        // Already correct from backend
        blobstore_initialized: disk.blobstore_initialized,
        // Use backend fields (no mapping needed)
        id: disk.id,
        node: disk.node,
        pci_addr: disk.pci_addr,
        capacity: disk.capacity,
        // Ensure arrays exist to prevent crashes
        provisioned_volumes: disk.provisioned_volumes || [],
        orphaned_spdk_volumes: disk.orphaned_spdk_volumes || []
      };
    }) || [],
    nodes: backendData.nodes || [],
    node_info: backendData.node_info || {}
  };
};

// Authentication hook (unchanged)
export const useAuth = () => {
  const [isAuthenticated, setIsAuthenticated] = useState(false);
  const [role, setRole] = useState<api.Role | null>(null);
  const [loading, setLoading] = useState(false);

  // Any API call answering 401 (expired/revoked token, backend restart)
  // drops the app back to the login page.
  useEffect(() => {
    api.setOnSessionExpired(() => {
      setIsAuthenticated(false);
      setRole(null);
    });
    return () => api.setOnSessionExpired(null);
  }, []);

  const login = useCallback(async (username: string, password: string) => {
    setLoading(true);
    try {
      const grantedRole = await api.login(username, password);
      setRole(grantedRole);
      setIsAuthenticated(true);
    } finally {
      setLoading(false);
    }
  }, []);

  const logout = useCallback(() => {
    api.logout();
    setIsAuthenticated(false);
    setRole(null);
  }, []);

  return {
    isAuthenticated,
    role,
    loading,
    login,
    logout
  };
};

// Enhanced utility functions
export const filterVolumesByType = (volumes: Volume[], filter: VolumeFilter): Volume[] => {
  switch (filter) {
    case 'healthy':
      return volumes.filter(v => v.state === 'Healthy');
    case 'degraded':
      return volumes.filter(v => v.state === 'Degraded');
    case 'failed':
      return volumes.filter(v => v.state === 'Failed');
    case 'faulted':
      return volumes.filter(v => v.state === 'Degraded' || v.state === 'Failed');
    case 'rebuilding':
      return volumes.filter(v => 
        v.replica_statuses.some(replica => 
          replica.status === 'rebuilding' || 
          replica.rebuild_progress !== null ||
          replica.is_new_replica
        ) || v.raid_status?.rebuild_info !== undefined
      );
    case 'local-nvme':
      return volumes.filter(v => v.local_nvme);
    case 'all':
    default:
      return volumes;
  }
};

export const getNvmeofStatus = (volume: Volume): {
  enabled: boolean;
  targets: NvmeofTargetInfo[];
} => {
  const enabled = Boolean(
    volume.nvmeof_enabled ||
    (volume.nvmeof_targets && volume.nvmeof_targets.length > 0) ||
    volume.access_method === 'nvmeof'
  );
  
  return {
    enabled,
    targets: volume.nvmeof_targets || []
  };
};

export const getAccessMethodDisplayName = (accessMethod: string): string => {
  switch (accessMethod) {
    case 'nvmeof':
      return 'NVMe-oF';
    case 'remote-nvmeof':
      return 'Remote NVMe-oF';
    case 'local-nvmeof':
      return 'Local NVMe-oF';
    case 'local-nvme':
      return 'Local NVMe';
    case 'iscsi':
      return 'iSCSI';
    default:
      return accessMethod || 'Unknown';
  }
};

export const hasHighPerformanceAccess = (volume: Volume): boolean => {
  return volume.local_nvme && (
    volume.access_method === 'nvmeof' || 
    volume.nvmeof_enabled
  );
};

export const getRaidDisplayName = (raidStatus?: RaidStatus): string => {
  if (!raidStatus) return 'No RAID';
  return `RAID-${raidStatus.raid_level}`;
};

export const getRaidHealthStatus = (raidStatus?: RaidStatus): {
  status: string;
  color: string;
  severity: 'healthy' | 'degraded' | 'failed';
} => {
  if (!raidStatus) {
    return { status: 'Unknown', color: 'gray', severity: 'failed' };
  }

  const { state, operational_members, num_members } = raidStatus;
  
  if (state === 'online' && operational_members === num_members) {
    return { status: 'Healthy', color: 'green', severity: 'healthy' };
  } else if (state === 'degraded' || operational_members < num_members) {
    return { status: 'Degraded', color: 'yellow', severity: 'degraded' };
  } else if (state === 'failed' || operational_members === 0) {
    return { status: 'Failed', color: 'red', severity: 'failed' };
  } else {
    return { status: state, color: 'blue', severity: 'healthy' };
  }
};

// Disk Setup Types and Hook
export interface UnimplementedDisk {
  pci_address: string;
  device_name: string;
  vendor_id: string;
  device_id: string;
  subsystem_vendor_id: string;
  subsystem_device_id: string;
  numa_node?: number;
  driver: string;
  size_bytes: number;
  model: string;
  serial: string;
  firmware_version: string;
  namespace_id?: number;
  mounted_partitions: string[];
  filesystem_type?: string;
  is_system_disk: boolean;
  spdk_ready: boolean;
  discovered_at: string;
  nodeName?: string; // Added for frontend display
  // Enhanced status tracking
  driver_ready?: boolean; // True if driver is SPDK-compatible (original spdk_ready)
  blobstore_initialized?: boolean; // True if LVS/blobstore is initialized
  bdev_name?: string; // SPDK bdev name if driver bound
  lvs_name?: string | null; // LVS name if blobstore initialized
  free_space?: number; // Free space in bytes
}

export interface DiskSetupRequest {
  pci_addresses: string[];
  force_unmount: boolean;
  backup_data: boolean;
  huge_pages_mb?: number;
  driver_override?: string;
}

export interface DiskSetupResult {
  success: boolean;
  setup_disks: string[];
  failed_disks: Array<[string, string]>;
  warnings: string[];
  huge_pages_configured?: number;
  completed_at: string;
}

export interface NodeDiskData {
  node: string;
  disks: UnimplementedDisk[];
  loading: boolean;
  error?: string;
  last_updated?: string;
}

// Enhanced disk setup hook
export const useDiskSetup = () => {
  const [nodeData, setNodeData] = useState<Record<string, NodeDiskData>>({});
  const [refreshing, setRefreshing] = useState<Set<string>>(new Set());
  const queryClient = useQueryClient();

  const refreshNodeDisks = useCallback(async (nodeName: string) => {
    console.log(`🚨 [REFRESH_TRIGGER] refreshNodeDisks called for: ${nodeName}`);

    setRefreshing(prev => new Set([...prev, nodeName]));
    setNodeData(prev => ({
      ...prev,
      [nodeName]: {
        ...prev[nodeName],
        node: nodeName,
        disks: prev[nodeName]?.disks ?? [],
        loading: true,
        error: undefined
      }
    }));

    try {
      const response = await apiFetch(`/api/nodes/${nodeName}/disks/status`);
      const data = await response.json().catch(() => null);

      if (response.ok && data?.disks) {
        // Use disk data directly from minimal state API
        const enhancedDisks = data.disks.map((disk: UnimplementedDisk) => {
          const enhancedDisk = { ...disk, nodeName };

          // Minimal state mode: Use values directly from the API
          // blobstore_initialized is set by backend when LVS exists
          enhancedDisk.blobstore_initialized = disk.blobstore_initialized || false;
          // driver_ready is true if blobstore is initialized OR if bdev exists
          enhancedDisk.driver_ready = enhancedDisk.blobstore_initialized || !!disk.bdev_name || disk.spdk_ready;
          enhancedDisk.spdk_ready = enhancedDisk.blobstore_initialized;

          console.log(`Disk ${disk.device_name}: driver_ready=${enhancedDisk.driver_ready}, blobstore_initialized=${enhancedDisk.blobstore_initialized}, bdev=${disk.bdev_name}`);

          return enhancedDisk;
        });

        setNodeData(prev => ({
          ...prev,
          [nodeName]: {
            node: nodeName,
            disks: enhancedDisks,
            loading: false,
            last_updated: new Date().toISOString()
          }
        }));
        return;
      }

      // No usable data from the node agent (pod down, unreachable, or error
      // response) — surface the failure instead of substituting fake disks
      throw new Error(
        data?.error ||
          (response.ok
            ? 'Malformed response from node agent'
            : `Node agent unavailable (HTTP ${response.status})`)
      );
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Unknown error';
      console.warn(`Failed to refresh disks for ${nodeName}: ${message}`);
      setNodeData(prev => ({
        ...prev,
        [nodeName]: {
          node: nodeName,
          disks: [],
          loading: false,
          error: message,
          last_updated: new Date().toISOString()
        }
      }));
    } finally {
      setRefreshing(prev => {
        const newSet = new Set(prev);
        newSet.delete(nodeName);
        return newSet;
      });
    }
  }, []);

  // Post-mutation reconcile: the node ops await backend completion, so refresh
  // the node's disks immediately (no timing guess) and invalidate the main
  // dashboard query, whose LVS/volume counts a disk op may have changed.
  const refreshAfterMutation = useCallback(async (nodeName: string) => {
    await refreshNodeDisks(nodeName);
    queryClient.invalidateQueries({ queryKey: ['dashboard'] });
  }, [refreshNodeDisks, queryClient]);

  const setupDisksOnNode = useCallback(async (
    nodeName: string, 
    request: DiskSetupRequest
  ): Promise<DiskSetupResult> => {
    try {
      const response = await apiFetch(`/api/nodes/${nodeName}/disks/setup`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request)
      });

      if (response.ok) {
        const result = await response.json();
        
        // Refresh node data after setup to show new status
        if (result.success) {
          await refreshAfterMutation(nodeName);
        }
        
        return result;
      } else {
        // Try to get error details from response
        const errorData = await response.json().catch(() => ({}));
        const errorMsg = errorData.error || response.statusText;
        
        // Return error result instead of mock success
        return {
          success: false,
          setup_disks: [],
          failed_disks: request.pci_addresses.map(addr => [addr, errorMsg]),
          warnings: [`API error: ${errorMsg}`],
          completed_at: new Date().toISOString()
        };
      }
    } catch (apiError) {
      console.error(`Disk setup API error for ${nodeName}:`, apiError);
      
      // Return error result instead of mock success
      const errorMsg = apiError instanceof Error ? apiError.message : 'Unknown error';
      return {
        success: false,
        setup_disks: [],
        failed_disks: request.pci_addresses.map(addr => [addr, errorMsg]),
        warnings: [`Connection error: ${errorMsg}`],
        completed_at: new Date().toISOString()
      };
    }
  }, [refreshAfterMutation]);

  const resetDisksOnNode = useCallback(async (
    nodeName: string, 
    pciAddresses: string[]
  ): Promise<any> => {
    try {
      const response = await apiFetch(`/api/nodes/${nodeName}/disks/reset`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ pci_addresses: pciAddresses })
      });

      if (response.ok) {
        const result = await response.json();
        
        // Refresh node data after reset to show new status
        if (result.success) {
          await refreshAfterMutation(nodeName);
        }
        
        return result;
      } else {
        // Try to get error details from response
        const errorData = await response.json().catch(() => ({}));
        const errorMsg = errorData.error || response.statusText;
        
        return {
          success: false,
          reset_disks: [],
          failed_disks: pciAddresses.map(addr => [addr, errorMsg]),
          completed_at: new Date().toISOString()
        };
      }
    } catch (apiError) {
      console.error(`Disk reset API error for ${nodeName}:`, apiError);
      
      const errorMsg = apiError instanceof Error ? apiError.message : 'Unknown error';
      return {
        success: false,
        reset_disks: [],
        failed_disks: pciAddresses.map(addr => [addr, errorMsg]),
        completed_at: new Date().toISOString()
      };
    }
  }, [refreshAfterMutation]);

  const initializeBlobstoreOnNode = useCallback(async (
    nodeName: string, 
    pciAddresses: string[]
  ): Promise<DiskSetupResult> => {
    try {
      const response = await apiFetch(`/api/nodes/${nodeName}/disks/initialize`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ 
          pci_addresses: pciAddresses,
          force_unmount: false,
          backup_data: false
        })
      });

      if (response.ok) {
        const result = await response.json();
        
        // Normalize response to ensure it has the expected structure
        const normalizedResult: DiskSetupResult = {
          success: result.success || false,
          setup_disks: result.setup_disks || [],
          failed_disks: result.failed_disks || [],
          warnings: result.warnings || (result.error ? [result.error] : []),
          huge_pages_configured: result.huge_pages_configured,
          completed_at: result.completed_at || new Date().toISOString()
        };
        
        // Refresh node data after initialization to show new status
        if (normalizedResult.success) {
          await refreshAfterMutation(nodeName);
        }
        
        return normalizedResult;
      } else {
        const errorData = await response.json().catch(() => ({}));
        throw new Error(errorData.error || `Initialize blobstore request failed: ${response.statusText}`);
      }
    } catch (apiError) {
      // Never fabricate success for a destructive/provisioning op: report the
      // real failure so the operator sees it.
      const errorMsg = apiError instanceof Error ? apiError.message : String(apiError);
      console.error(`Disk initialize blobstore failed for ${nodeName}:`, errorMsg);
      return {
        success: false,
        setup_disks: [],
        failed_disks: pciAddresses.map(addr => [addr, errorMsg]),
        warnings: [`Initialize failed: ${errorMsg}`],
        completed_at: new Date().toISOString()
      };
    }
  }, [refreshAfterMutation]);

  const deleteDiskOnNode = useCallback(async (
    nodeName: string,
    pciAddress: string
  ): Promise<any> => {
    try {
      const response = await apiFetch(`/api/nodes/${nodeName}/disks/delete`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ pci_address: pciAddress })
      });

      if (response.ok) {
        const result = await response.json();
        
        // Refresh node data after deletion to show new status
        if (result.success) {
          await refreshAfterMutation(nodeName);
        }
        
        return result;
      } else {
        const errorData = await response.json().catch(() => ({}));
        throw new Error(errorData.error || `Delete request failed: ${response.statusText}`);
      }
    } catch (apiError) {
      // Never fabricate success for a destructive op.
      const errorMsg = apiError instanceof Error ? apiError.message : String(apiError);
      console.error(`Disk delete failed for ${nodeName}:`, errorMsg);
      return {
        success: false,
        error: errorMsg,
        message: `Disk delete failed: ${errorMsg}`,
        completed_at: new Date().toISOString()
      };
    }
  }, [refreshAfterMutation]);

  const createMemoryDisk = useCallback(async (
    nodeName: string,
    name: string,
    sizeMB: number,
    blockSize?: number
  ): Promise<{ success: boolean; error?: string; bdev_name?: string }> => {
    try {
      const response = await apiFetch(`/api/nodes/${nodeName}/memory_disks/create`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name, size_mb: sizeMB, block_size: blockSize })
      });

      const result = await response.json();

      if (response.ok && result.success) {
        // Refresh node disks after creating memory disk
        await refreshAfterMutation(nodeName);
        return { success: true, bdev_name: result.bdev_name };
      } else {
        return { success: false, error: result.error || 'Failed to create memory disk' };
      }
    } catch (err) {
      console.error('Error creating memory disk:', err);
      return { success: false, error: String(err) };
    }
  }, [refreshAfterMutation]);

  const deleteMemoryDisk = useCallback(async (
    nodeName: string,
    name: string
  ): Promise<{ success: boolean; error?: string }> => {
    try {
      const response = await apiFetch(`/api/nodes/${nodeName}/memory_disks/delete`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name })
      });

      const result = await response.json();

      if (response.ok && result.success) {
        // Refresh node disks after deleting memory disk
        await refreshAfterMutation(nodeName);
        return { success: true };
      } else {
        return { success: false, error: result.error || 'Failed to delete memory disk' };
      }
    } catch (err) {
      console.error('Error deleting memory disk:', err);
      return { success: false, error: String(err) };
    }
  }, [refreshAfterMutation]);

  return {
    nodeData,
    setNodeData,
    refreshNodeDisks,
    setupDisksOnNode,
    resetDisksOnNode,
    initializeBlobstoreOnNode,
    deleteDiskOnNode,
    createMemoryDisk,
    deleteMemoryDisk,
    refreshing: refreshing.size > 0
  };
};

export default useDashboardData;
