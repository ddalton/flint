import { useState, useEffect, useCallback, useMemo } from 'react';

// Enhanced types to match the backend API exactly
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
  // VHost-NVMe related fields from backend
  vhost_socket?: string;
  vhost_device?: string;
  vhost_enabled?: boolean;
  vhost_type?: string; // "nvme" for vhost-nvme, "blk" for vhost-blk
  nvme_namespaces?: VhostNvmeNamespace[];
  // Enhanced RAID status from backend
  raid_status?: RaidStatus;
}

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

export interface VhostNvmeNamespace {
  nsid: number;
  size: number;
  uuid: string;
  bdev_name: string;
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
}

export interface ProvisionedVolume {
  volume_name: string;
  volume_id: string;
  size: number;
  provisioned_at: string;
  replica_type: string;
  status: string;
}

export interface DashboardData {
  volumes: Volume[];
  disks: Disk[];
  nodes: string[];
}

export interface DashboardStats {
  totalVolumes: number;
  healthyVolumes: number;
  degradedVolumes: number;
  failedVolumes: number;
  faultedVolumes: number;
  volumesWithRebuilding: number;
  localNVMeVolumes: number;
  totalDisks: number;
  healthyDisks: number;
  formattedDisks: number;
}

export type VolumeFilter = 
  | 'all' 
  | 'healthy' 
  | 'degraded' 
  | 'failed' 
  | 'faulted' 
  | 'rebuilding' 
  | 'local-nvme';

export type DiskFilter = string | null;
export type VolumeReplicaFilter = string | null;

// Enhanced mock data with RAID status and VHost-NVMe details
const mockData: DashboardData = {
  volumes: [
    {
      id: "pvc-12345678-1234-1234-1234-123456789abc",
      name: "postgres-data-pvc",
      size: "100GB",
      state: "Healthy",
      replicas: 3,
      active_replicas: 3,
      local_nvme: true,
      access_method: "vhost-nvme",
      rebuild_progress: null,
      nodes: ["worker-node-1", "worker-node-2", "worker-node-3"],
      vhost_socket: "/var/lib/spdk/vhost/vhost_postgres-data-pvc.sock",
      vhost_device: "/dev/nvme-vhost-postgres-data-pvc",
      vhost_enabled: true,
      vhost_type: "nvme",
      nvme_namespaces: [
        {
          nsid: 1,
          size: 107374182400,
          uuid: "12345678-1234-1234-1234-123456789abc",
          bdev_name: "pvc-12345678-1234-1234-1234-123456789abc"
        }
      ],
      raid_status: {
        raid_level: 1,
        state: "online",
        num_members: 3,
        operational_members: 3,
        discovered_members: 3,
        members: [
          {
            slot: 0,
            name: "nvme0n1",
            state: "online",
            uuid: "11111111-1111-1111-1111-111111111111",
            is_configured: true,
            node: "worker-node-1",
            disk_ref: "nvme0n1",
            health_status: "healthy"
          },
          {
            slot: 1,
            name: "nvme1n1",
            state: "online",
            uuid: "22222222-2222-2222-2222-222222222222",
            is_configured: true,
            node: "worker-node-2",
            disk_ref: "nvme1n1",
            health_status: "healthy"
          },
          {
            slot: 2,
            name: "nvme2n1",
            state: "online",
            uuid: "33333333-3333-3333-3333-333333333333",
            is_configured: true,
            node: "worker-node-3",
            disk_ref: "nvme2n1",
            health_status: "healthy"
          }
        ],
        auto_rebuild_enabled: true,
        superblock_version: 1
      },
      replica_statuses: [
        {
          node: "worker-node-1",
          status: "healthy",
          is_local: true,
          last_io_timestamp: "2025-06-01T10:30:00Z",
          rebuild_progress: null,
          rebuild_target: null,
          is_new_replica: false,
          nvmf_target: null,
          access_method: "local-nvme",
          raid_member_slot: 0,
          raid_member_state: "online",
          lvol_uuid: "11111111-1111-1111-1111-111111111111",
          disk_ref: "nvme0n1",
          replica_size: 107374182400
        },
        {
          node: "worker-node-2",
          status: "healthy",
          is_local: false,
          last_io_timestamp: "2025-06-01T10:29:55Z",
          rebuild_progress: null,
          rebuild_target: null,
          is_new_replica: false,
          nvmf_target: {
            nqn: "nqn.2016-06.io.spdk:cnode2",
            target_ip: "192.168.1.102",
            target_port: "4420",
            transport_type: "TCP"
          },
          access_method: "remote-nvmf",
          raid_member_slot: 1,
          raid_member_state: "online",
          lvol_uuid: "22222222-2222-2222-2222-222222222222",
          disk_ref: "nvme1n1",
          replica_size: 107374182400
        },
        {
          node: "worker-node-3",
          status: "healthy",
          is_local: false,
          last_io_timestamp: "2025-06-01T10:29:50Z",
          rebuild_progress: null,
          rebuild_target: null,
          is_new_replica: false,
          nvmf_target: {
            nqn: "nqn.2016-06.io.spdk:cnode3",
            target_ip: "192.168.1.103",
            target_port: "4420",
            transport_type: "TCP"
          },
          access_method: "remote-nvmf",
          raid_member_slot: 2,
          raid_member_state: "online",
          lvol_uuid: "33333333-3333-3333-3333-333333333333",
          disk_ref: "nvme2n1",
          replica_size: 107374182400
        }
      ]
    },
    {
      id: "pvc-87654321-4321-4321-4321-cba987654321",
      name: "redis-cache-pvc",
      size: "50GB",
      state: "Degraded",
      replicas: 3,
      active_replicas: 2,
      local_nvme: true,
      access_method: "vhost-nvme",
      rebuild_progress: 75.5,
      nodes: ["worker-node-1", "worker-node-2", "worker-node-3"],
      vhost_socket: "/var/lib/spdk/vhost/vhost_redis-cache-pvc.sock",
      vhost_device: "/dev/nvme-vhost-redis-cache-pvc",
      vhost_enabled: true,
      vhost_type: "nvme",
      nvme_namespaces: [
        {
          nsid: 1,
          size: 53687091200,
          uuid: "87654321-4321-4321-4321-cba987654321",
          bdev_name: "pvc-87654321-4321-4321-4321-cba987654321"
        }
      ],
      raid_status: {
        raid_level: 1,
        state: "degraded",
        num_members: 3,
        operational_members: 2,
        discovered_members: 3,
        members: [
          {
            slot: 0,
            name: "nvme0n1",
            state: "online",
            uuid: "44444444-4444-4444-4444-444444444444",
            is_configured: true,
            node: "worker-node-1",
            disk_ref: "nvme0n1",
            health_status: "healthy"
          },
          {
            slot: 1,
            name: "nvme1n1",
            state: "online",
            uuid: "55555555-5555-5555-5555-555555555555",
            is_configured: true,
            node: "worker-node-2",
            disk_ref: "nvme1n1",
            health_status: "healthy"
          },
          {
            slot: 2,
            name: "nvme2n1",
            state: "rebuilding",
            uuid: "66666666-6666-6666-6666-666666666666",
            is_configured: true,
            node: "worker-node-3",
            disk_ref: "nvme2n1",
            health_status: "rebuilding"
          }
        ],
        rebuild_info: {
          state: "rebuilding",
          target_slot: 2,
          source_slot: 0,
          blocks_remaining: 12800000,
          blocks_total: 52428800,
          progress_percentage: 75.5,
          estimated_time_remaining: "15m",
          start_time: "2025-06-01T10:00:00Z"
        },
        auto_rebuild_enabled: true,
        superblock_version: 1
      },
      replica_statuses: [
        {
          node: "worker-node-1",
          status: "healthy",
          is_local: true,
          last_io_timestamp: "2025-06-01T10:30:00Z",
          rebuild_progress: null,
          rebuild_target: null,
          is_new_replica: false,
          nvmf_target: null,
          access_method: "local-nvme",
          raid_member_slot: 0,
          raid_member_state: "online"
        },
        {
          node: "worker-node-2",
          status: "healthy",
          is_local: false,
          last_io_timestamp: "2025-06-01T10:29:55Z",
          rebuild_progress: null,
          rebuild_target: null,
          is_new_replica: false,
          nvmf_target: {
            nqn: "nqn.2016-06.io.spdk:cnode2",
            target_ip: "192.168.1.102",
            target_port: "4420",
            transport_type: "TCP"
          },
          access_method: "remote-nvmf",
          raid_member_slot: 1,
          raid_member_state: "online"
        },
        {
          node: "worker-node-3",
          status: "rebuilding",
          is_local: false,
          last_io_timestamp: "2025-06-01T10:29:50Z",
          rebuild_progress: 75.5,
          rebuild_target: null,
          is_new_replica: true,
          nvmf_target: {
            nqn: "nqn.2016-06.io.spdk:cnode3",
            target_ip: "192.168.1.103",
            target_port: "4420",
            transport_type: "TCP"
          },
          access_method: "remote-nvmf",
          raid_member_slot: 2,
          raid_member_state: "rebuilding"
        }
      ]
    }
  ],
  disks: [
    {
      id: "nvme0n1",
      node: "worker-node-1",
      pci_addr: "0000:3b:00.0",
      capacity: 1024000000000,
      capacity_gb: 1000,
      allocated_space: 512,
      free_space: 488,
      free_space_display: "488GB",
      healthy: true,
      blobstore_initialized: true,
      lvol_count: 2,
      model: "Samsung SSD 980 PRO 1TB",
      read_iops: 45000,
      write_iops: 32000,
      read_latency: 120,
      write_latency: 180,
      brought_online: "2025-06-01T08:00:00Z",
      provisioned_volumes: [
        {
          volume_name: "postgres-data-pvc",
          volume_id: "pvc-12345678-1234-1234-1234-123456789abc",
          size: 100,
          provisioned_at: "2025-06-01T08:15:00Z",
          replica_type: "Local NVMe",
          status: "healthy"
        },
        {
          volume_name: "redis-cache-pvc",
          volume_id: "pvc-87654321-4321-4321-4321-cba987654321",
          size: 50,
          provisioned_at: "2025-06-01T08:20:00Z",
          replica_type: "Local NVMe",
          status: "healthy"
        }
      ]
    },
    {
      id: "nvme1n1",
      node: "worker-node-2",
      pci_addr: "0000:3b:00.0",
      capacity: 1024000000000,
      capacity_gb: 1000,
      allocated_space: 150,
      free_space: 850,
      free_space_display: "850GB",
      healthy: true,
      blobstore_initialized: true,
      lvol_count: 2,
      model: "Samsung SSD 980 PRO 1TB",
      read_iops: 43000,
      write_iops: 30000,
      read_latency: 125,
      write_latency: 185,
      brought_online: "2025-06-01T08:00:00Z",
      provisioned_volumes: [
        {
          volume_name: "postgres-data-pvc",
          volume_id: "pvc-12345678-1234-1234-1234-123456789abc",
          size: 100,
          provisioned_at: "2025-06-01T08:15:00Z",
          replica_type: "Remote NVMe-oF",
          status: "healthy"
        },
        {
          volume_name: "redis-cache-pvc",
          volume_id: "pvc-87654321-4321-4321-4321-cba987654321",
          size: 50,
          provisioned_at: "2025-06-01T08:20:00Z",
          replica_type: "Remote NVMe-oF",
          status: "healthy"
        }
      ]
    },
    {
      id: "nvme2n1",
      node: "worker-node-3",
      pci_addr: "0000:3b:00.0",
      capacity: 1024000000000,
      capacity_gb: 1000,
      allocated_space: 100,
      free_space: 900,
      free_space_display: "900GB",
      healthy: true,
      blobstore_initialized: true,
      lvol_count: 1,
      model: "Samsung SSD 980 PRO 1TB",
      read_iops: 41000,
      write_iops: 28000,
      read_latency: 130,
      write_latency: 190,
      brought_online: "2025-06-01T08:00:00Z",
      provisioned_volumes: [
        {
          volume_name: "postgres-data-pvc",
          volume_id: "pvc-12345678-1234-1234-1234-123456789abc",
          size: 100,
          provisioned_at: "2025-06-01T08:15:00Z",
          replica_type: "Remote NVMe-oF",
          status: "healthy"
        },
        {
          volume_name: "redis-cache-pvc",
          volume_id: "pvc-87654321-4321-4321-4321-cba987654321",
          size: 50,
          provisioned_at: "2025-06-01T08:15:00Z",
          replica_type: "Remote NVMe-oF",
          status: "rebuilding"
        }
      ]
    }
  ],
  nodes: ["worker-node-1", "worker-node-2", "worker-node-3"]
};

// Enhanced hook implementation with API integration
export const useDashboardData = (autoRefresh: boolean = true) => {
  const [data, setData] = useState<DashboardData>({
    volumes: [],
    disks: [],
    nodes: []
  });
  const [loading, setLoading] = useState(true);

  const stats = useMemo((): DashboardStats => {
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
    
    const healthyDisks = data.disks.filter(d => d.healthy).length;
    const formattedDisks = data.disks.filter(d => d.blobstore_initialized).length;

    return {
      totalVolumes: data.volumes.length,
      healthyVolumes,
      degradedVolumes,
      failedVolumes,
      faultedVolumes,
      volumesWithRebuilding,
      localNVMeVolumes,
      totalDisks: data.disks.length,
      healthyDisks,
      formattedDisks,
    };
  }, [data]);

  const refreshData = useCallback(async () => {
    try {
      setLoading(true);
      
      // Try to fetch from API, fall back to mock data
      try {
        const response = await fetch('/api/dashboard');
        if (!response.ok) {
          throw new Error(`HTTP error! status: ${response.status}`);
        }
        const dashboardData = await response.json();
        
        // Transform backend data to match frontend interface if needed
        const transformedData = transformBackendData(dashboardData);
        setData(transformedData);
      } catch (apiError) {
        console.warn('API not available, using mock data:', apiError);
        // Use mock data for development/demo
        setData(mockData);
      }
    } catch (error) {
      console.error('Failed to fetch dashboard data:', error);
      // Use mock data as fallback
      setData(mockData);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refreshData();
  }, [refreshData]);

  useEffect(() => {
    if (!autoRefresh) return;

    const interval = setInterval(refreshData, 30000); // Refresh every 30 seconds
    return () => clearInterval(interval);
  }, [autoRefresh, refreshData]);

  return {
    data,
    loading,
    stats,
    refreshData
  };
};

// Transform backend data structure to frontend interface
const transformBackendData = (backendData: any): DashboardData => {
  return {
    volumes: backendData.volumes?.map((vol: any) => ({
      ...vol,
      // Ensure all fields are properly mapped
      raid_level: vol.raid_status?.raid_level ? `RAID-${vol.raid_status.raid_level}` : undefined,
      primary_replica_uuid: vol.primary_lvol_uuid,
      nvme_namespaces: vol.nvme_namespaces || []
    })) || [],
    disks: backendData.disks?.map((disk: any) => ({
      ...disk,
      // Ensure compatibility with existing frontend code
      lvol_store_initialized: disk.blobstore_initialized
    })) || [],
    nodes: backendData.nodes || []
  };
};

// Authentication hook (unchanged)
export const useAuth = () => {
  const [isAuthenticated, setIsAuthenticated] = useState(false);
  const [loading, setLoading] = useState(false);

  const login = useCallback(async (username: string, password: string) => {
    setLoading(true);
    try {
      // Simulate API call
      await new Promise(resolve => setTimeout(resolve, 1000));
      
      if (username === 'admin' && password === 'spdk-admin-2025') {
        setIsAuthenticated(true);
        // Note: In production, avoid localStorage for sensitive auth tokens
        if (typeof window !== 'undefined') {
          localStorage.setItem('spdk_auth', 'true');
        }
      } else {
        throw new Error('Invalid credentials');
      }
    } catch (error) {
      throw error;
    } finally {
      setLoading(false);
    }
  }, []);

  const logout = useCallback(() => {
    setIsAuthenticated(false);
    if (typeof window !== 'undefined') {
      localStorage.removeItem('spdk_auth');
    }
  }, []);

  useEffect(() => {
    if (typeof window !== 'undefined') {
      const stored = localStorage.getItem('spdk_auth');
      if (stored === 'true') {
        setIsAuthenticated(true);
      }
    }
  }, []);

  return {
    isAuthenticated,
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

export const getVHostNvmeStatus = (volume: Volume): {
  enabled: boolean;
  socket?: string;
  device?: string;
  method: string;
  type: string;
  namespaces?: VhostNvmeNamespace[];
} => {
  const enabled = Boolean(
    volume.vhost_enabled || 
    volume.vhost_socket || 
    volume.access_method === 'vhost-nvme' ||
    volume.vhost_type === 'nvme'
  );
  
  return {
    enabled,
    socket: volume.vhost_socket,
    device: volume.vhost_device,
    method: volume.access_method || 'unknown',
    type: volume.vhost_type || 'nvme',
    namespaces: volume.nvme_namespaces
  };
};

export const getAccessMethodDisplayName = (accessMethod: string): string => {
  switch (accessMethod) {
    case 'vhost-nvme':
      return 'VHost-NVMe';
    case 'vhost':
      return 'VHost-NVMe'; // Default vhost to NVMe
    case 'nvmf':
      return 'NVMe-oF';
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
    volume.access_method === 'vhost-nvme' || 
    volume.vhost_enabled ||
    volume.vhost_socket ||
    volume.vhost_type === 'nvme'
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

export default useDashboardData;