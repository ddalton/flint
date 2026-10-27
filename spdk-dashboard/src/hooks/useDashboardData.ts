import { useState, useEffect, useCallback, useMemo } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import * as api from '../api/client';
import { apiFetch } from '../api/client';

const EMPTY_DASHBOARD: DashboardData = {
  volumes: [],
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

// A replica is mid-recovery when the Tier-2 engine reports it non-in_sync
// (stale → standby → in_sync, or a live hot-rejoin marker). The sync record
// is the ONLY recovery signal the backend emits: single-replica volumes have
// no record and nothing to recover, and the pre-Tier-2 rebuild markers
// (rebuild_progress / is_new_replica / status "rebuilding") were never
// populated by the live backend, so they are no longer read.
export const isReplicaRecovering = (replica: ReplicaStatus): boolean =>
  !!replica.sync && replica.sync.sync_state !== 'in_sync';

export const isVolumeRecovering = (volume: Volume): boolean =>
  volume.replica_statuses.some(isReplicaRecovering);

export const hasRecoveringReplicas = (data: DashboardData | undefined): boolean =>
  !!data?.volumes.some(isVolumeRecovering);

// Orphaned SPDK lvols (flint-shaped names with no owning PV) are reported
// per DISK by the backend; the aggregate's raw_volumes list is always empty.
export const countOrphanedLvols = (disks: Disk[]): number =>
  disks.reduce((sum, d) => sum + d.orphaned_spdk_volumes.length, 0);

export const computeStats = (data: DashboardData): DashboardStats => {
  const healthyVolumes = data.volumes.filter(v => v.state === 'Healthy').length;
  const degradedVolumes = data.volumes.filter(v => v.state === 'Degraded').length;
  const failedVolumes = data.volumes.filter(v => v.state === 'Failed').length;
  const faultedVolumes = degradedVolumes + failedVolumes;
  const recoveringVolumes = data.volumes.filter(isVolumeRecovering).length;
  const orphanedLvols = countOrphanedLvols(data.disks);
  const healthyDisks = data.disks.filter(d => d.healthy).length;
  const formattedDisks = data.disks.filter(d => d.blobstore_initialized).length;

  return {
    totalVolumes: data.volumes.length,
    healthyVolumes,
    degradedVolumes,
    failedVolumes,
    faultedVolumes,
    recoveringVolumes,
    orphanedLvols,
    totalDisks: data.disks.length,
    healthyDisks,
    formattedDisks,
  };
};

// --- Start of new/updated interfaces ---

// Represents NVMe-oF target information from the backend
// --- Backend API types ---
// Generated from the backend's OpenAPI document (api/openapi.json, emitted
// by `cargo run --bin dashboard-openapi`; regenerate with `npm run gen:api`).
// Wire types are ALIASES of the generated schemas so they cannot drift from
// the Rust structs; the few frontend narrowings (string → literal union)
// are explicit `Omit & override` intersections, visible below.
import type { components } from '../api/schema';

type Schemas = components['schemas'];

// The volume as the backend computes it: state and in-sync count from the
// sync record, the replica rows, the consumer raids actually assembled, PVC
// and SPDK-verification details. The pre-Tier-2 placeholders (raid_status /
// rebuild_info, rebuild_progress, local_nvme, the access and transport
// descriptors) left the wire on 2026-10-27: recovery is
// replica_statuses[].sync, the data path is consumer_raids.
export type Volume = Omit<
  Schemas['DashboardVolume'],
  'replica_statuses' | 'spdk_validation_status'
> & {
  replica_statuses: ReplicaStatus[]; // narrowed sync tree (SyncState union)
  spdk_validation_status: SpdkValidationStatus; // narrowed severity union
};

export type ConsumerRaid = Schemas['ConsumerRaid'];
export type ConsumerRaidMember = Schemas['ConsumerRaidMember'];

export type SpdkValidationStatus = Omit<
  Schemas['SpdkValidationStatus'],
  'validation_severity'
> & {
  validation_severity: 'info' | 'warning' | 'error';
};

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


// One replica row: node, the status string ("healthy" / "stale" /
// "standby" projected from the sync record; "active" / "failed" on a
// single-replica volume, from verifying its one lvol), and the Tier-2 sync
// state — absent on single-replica volumes, which have no record. Whether
// a leg is served locally is the consumer's property: isLegServedLocally.
export type ReplicaStatus = Omit<Schemas['DashboardReplicaStatus'], 'sync'> & {
  sync?: ReplicaSyncInfo | null; // narrowed SyncState union
};

export type SyncState = 'in_sync' | 'stale' | 'standby';

// epoch_lag: epochs behind current; 0 when in_sync, null when unknowable
// (no last_epoch — a full build from empty — or trimmed history). Lag → 0 is
// the catch-up. hot_rejoin: E_f epoch name while a hot rejoin is in flight.
// maint_drain: live lease deadline while the leg is fenced out for a planned
// csi-node roll. rebuild_base: what the head was rebuilt from while a
// catch-up is in progress — an epoch name (delta) or "empty" (full build).
export type ReplicaSyncInfo = Omit<Schemas['ReplicaSyncInfo'], 'sync_state'> & {
  sync_state: SyncState;
};

// capacity is bytes; capacity_gb / allocated_space / free_space are GB.
// is_system_disk: root/boot disk — never an init candidate.
export type Disk = Schemas['DashboardDisk'];

export type ProvisionedVolume = Schemas['ProvisionedVolume'];
export type OrphanedVolumeInfo = Schemas['OrphanedVolumeInfo'];

export type PvcInfo = Schemas['PvcInfo'];
export type NodeInfo = Schemas['NodeInfo'];

// The wire aggregate also carries raw_volumes; the backend always sends it
// empty (orphans are attributed per disk), so it is not modeled here.
export interface DashboardData {
  volumes: Volume[];
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
  // Volumes with at least one replica not in_sync (catch-up, standby, or a
  // hot rejoin in flight).
  recoveringVolumes: number;
  // Orphaned SPDK lvols across all disks (cleanup candidates).
  orphanedLvols: number;
  totalDisks: number;
  healthyDisks: number;
  formattedDisks: number;
}

// 'orphaned' is a DISK-side filter (orphans are per-disk lvols with no
// owning volume): it matches no managed volume and lands on the Disks tab.
export type VolumeFilter =
  | 'all'
  | 'orphaned'
  | 'healthy'
  | 'degraded'
  | 'failed'
  | 'faulted'
  | 'rebuilding';

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

// Poll fast while any replica is mid-recovery (stale/standby/rejoining) so
// the sync indicator is live; drop back to the 30s baseline once the cluster
// is fully in_sync. A hot-rejoin window itself is sub-2s — too fast to poll —
// and surfaces after the fact via events, so 2.5s tracks the observable part
// (epoch catch-up) without hammering the backend (the 3s aggregate cache
// absorbs most of it anyway).
const BASE_POLL_MS = 30_000;
const RECOVERY_POLL_MS = 2_500;

// Enhanced hook implementation with API integration and backend filtering
export const useDashboardData = (autoRefresh: boolean = true, filters?: DashboardFilters) => {
  const query = useQuery({
    queryKey: ['dashboard', filters ?? null],
    queryFn: () => fetchDashboard(filters),
    refetchInterval: autoRefresh
      ? (q) => (hasRecoveringReplicas(q.state.data) ? RECOVERY_POLL_MS : BASE_POLL_MS)
      : false,
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
    connectionError: query.isError ? (query.error as Error).message : null,
  };
};

// Normalize the wire aggregate (generated DashboardData schema) into the
// frontend shape: harden array fields against partial payloads and apply
// the documented narrowings (SyncState/severity unions) — the two `as`
// casts below are those narrowings, asserted once at this boundary.
// Exported for tests: this IS the wire boundary.
export const transformBackendData = (backendData: Schemas['DashboardData']): DashboardData => {
  return {
    volumes: (backendData.volumes || []).map(vol => ({
      ...vol,
      replica_statuses: (vol.replica_statuses || []) as ReplicaStatus[],
      nodes: vol.nodes || [],
      consumer_raids: vol.consumer_raids || [],
      spdk_validation_status: vol.spdk_validation_status as SpdkValidationStatus,
    })),
    disks: (backendData.disks || []).map(disk => {
      // Backend already returns capacity_gb and free_space in GB (not bytes!)
      const sizeGB = disk.capacity_gb || Math.round((disk.capacity || 0) / (1024 * 1024 * 1024));
      // A disk without a logical volume store has nothing allocated. The
      // agent reports free_space 0 for it, which the released backend passes
      // through and which read as "100% used" on dash1-cp-1 (2026-10-27).
      const initialized = disk.blobstore_initialized !== false;
      const freeGB = initialized ? (disk.free_space || 0) : 0;  // Already in GB from backend
      const allocatedGB = initialized ? (disk.allocated_space || (sizeGB - freeGB)) : 0;

      return {
        ...disk,
        capacity_gb: sizeGB,
        allocated_space: allocatedGB,
        free_space: freeGB,
        free_space_display: disk.free_space_display || `${freeGB}GB`,
        // Ensure arrays exist to prevent crashes
        provisioned_volumes: disk.provisioned_volumes || [],
        orphaned_spdk_volumes: disk.orphaned_spdk_volumes || []
      };
    }),
    nodes: backendData.nodes || [],
    node_info: backendData.node_info || {}
  };
};

// Authentication hook. Boots authenticated when a sessionStorage session
// survived the refresh — if its token is stale (backend restarted), the
// first API call 401s and the expiry hook drops back to login.
export const useAuth = () => {
  const [isAuthenticated, setIsAuthenticated] = useState(api.hasSession());
  const [role, setRole] = useState<api.Role | null>(api.getRole());
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

// THE volume-filter predicate — every filtered view (volumes table, disks
// table, nodes view) routes through here so a filter always matches the
// stat card that advertised it. 'rebuilding' is isVolumeRecovering, exactly
// like computeStats.recoveringVolumes. 'orphaned' matches no managed
// volume: orphans are per-disk lvols (DisksTable filters disks by them).
export const filterVolumesByType = <V extends Volume>(volumes: V[], filter: VolumeFilter): V[] => {
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
      return volumes.filter(isVolumeRecovering);
    case 'orphaned':
      return [];
    case 'all':
    default:
      return volumes;
  }
};

// The consumer raid the volume's data path runs through. The backend emits
// one ConsumerRaid per node that currently has `raid_<pv>` assembled
// (normally one; more than one only mid-cutover). None → the volume is not
// staged anywhere.
export const primaryConsumerRaid = (volume: Volume): ConsumerRaid | null =>
  volume.consumer_raids[0] ?? null;

// A leg is served locally when a consumer raid is assembled on its own node;
// the other legs are reached over NVMe-oF. null when the volume has no
// consumer at all (nothing is being served).
export const isLegServedLocally = (volume: Volume, node: string): boolean | null =>
  volume.consumer_raids.length === 0 ? null : volume.consumer_raids.some(c => c.node === node);

// Disk Setup Types and Hook
// The agent's /api/disks/status row plus the node name the tab attaches.
// (The old hand-written shape claimed bdev_name/lvs_name fields the agent
// has never sent on this endpoint.)
export type UnimplementedDisk = Schemas['NodeDiskStatus'] & {
  nodeName?: string; // Added for frontend display
};

// What the agent actually accepts (extra fields are silently ignored
// server-side — the old huge_pages_mb/driver_override options never did
// anything).
export type DiskSetupRequest = Schemas['DiskSetupRequest'];

// failed_disks holds PCI addresses only; human-readable causes are in
// warnings (the old tuple shape was fiction).
export type DiskSetupResult = Schemas['DiskSetupResponse'];

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
        // Attach the node name; the agent already sends the derived
        // driver_ready/spdk_ready flags (see NodeDiskStatus in the spec).
        const enhancedDisks = data.disks.map((disk: UnimplementedDisk) => ({ ...disk, nodeName }));

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
          failed_disks: request.pci_addresses ?? [],
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
        failed_disks: request.pci_addresses ?? [],
        warnings: [`Connection error: ${errorMsg}`],
        completed_at: new Date().toISOString()
      };
    }
  }, [refreshAfterMutation]);

  // Agent-side reset is 501/unimplemented in minimal state; the shape below
  // mirrors DiskSetupResponse with the legacy reset_disks field.
  const resetDisksOnNode = useCallback(async (
    nodeName: string,
    pciAddresses: string[]
  ): Promise<Partial<DiskSetupResult> & { success: boolean; reset_disks?: string[] }> => {
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
          failed_disks: pciAddresses,
          warnings: [String(errorMsg)],
          completed_at: new Date().toISOString()
        };
      }
    } catch (apiError) {
      console.error(`Disk reset API error for ${nodeName}:`, apiError);

      const errorMsg = apiError instanceof Error ? apiError.message : 'Unknown error';
      return {
        success: false,
        reset_disks: [],
        failed_disks: pciAddresses,
        warnings: [errorMsg],
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
        failed_disks: pciAddresses,
        warnings: [`Initialize failed: ${errorMsg}`],
        completed_at: new Date().toISOString()
      };
    }
  }, [refreshAfterMutation]);

  const deleteDiskOnNode = useCallback(async (
    nodeName: string,
    pciAddress: string
  ): Promise<{ success: boolean; error?: string; message?: string; completed_at?: string }> => {
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
