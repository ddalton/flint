import { useState, useEffect, useCallback, useMemo } from 'react';

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

// Physical disk in the storage hierarchy
export interface PhysicalDisk {
  id: string;
  node: string;
  pci_addr: string;
  capacity_gb: number;
  model: string;
  healthy: boolean;
  blobstore_initialized: boolean;
}

// RAID device built from physical disks
export interface SpdkRaid {
  name: string;
  raid_level: number;
  state: string;
  member_disks: PhysicalDisk[];
  num_members: number;
  operational_members: number;
  rebuild_info?: RebuildInfo;
  superblock_version?: number;
  auto_rebuild_enabled: boolean;
  node: string;
  capacity_gb: number;
  used_gb: number;
}

// Logical Volume Store built on RAID
export interface LogicalVolumeStore {
  name: string;
  base_raid: string; // References SpdkRaid.name
  capacity_gb: number;
  used_gb: number;
  utilization_pct: number;
  cluster_size: number;
  node: string;
  logical_volumes: string[]; // Array of logical volume UUIDs
}

// Logical Volume (the actual volume in LVS)
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
  
  // Storage hierarchy references
  lvs_name: string; // Which LVS this volume belongs to
  lvol_uuid: string; // Logical volume UUID in SPDK
  
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


// DEPRECATED: Use SpdkRaid instead for correct topology
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
  is_remote?: boolean;
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

export interface RaidMember {
  slot: number;
  name: string;
  state: string;
  uuid?: string;
  node?: string;
}

export interface RaidDisk {
  id: string;
  node: string;
  raid_level: string;
  state: string;
  lvs_name?: string;
  lvs_uuid?: string;
  total_capacity_gb: number;
  usable_capacity_gb: number;
  used_capacity_gb: number;
  degraded: boolean;
  rebuild_progress?: number;
  members: RaidMember[];
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

export interface NodePerformanceMetrics {
  node_id: string;
  raid_count: number;
  volume_count: number;
  total_read_iops: number;
  total_write_iops: number;
  total_read_bandwidth_mbps: number;
  total_write_bandwidth_mbps: number;
  avg_read_latency_ms: number;
  avg_write_latency_ms: number;
  spdk_active: boolean;
  last_updated: string;
  failed_raids: number;
  degraded_raids: number;
  healthy_raids: number;
  performance_score: number;
}

export interface ClusterPerformanceTotals {
  total_read_iops: number;
  total_write_iops: number;
  total_bandwidth_mbps: number;
  avg_cluster_latency_ms: number;
  total_active_nodes: number;
  total_raids: number;
}

export interface NodesPerformanceResponse {
  nodes: NodePerformanceMetrics[];
  cluster_totals: ClusterPerformanceTotals;
  last_updated: string;
}

export interface DashboardData {
  volumes: Volume[];
  raw_volumes: RawSpdkVolume[];
  disks: Disk[];
  nodes: string[];
  
  // New correct storage hierarchy
  physical_disks: PhysicalDisk[];
  spdk_raids: SpdkRaid[];
  logical_volume_stores: LogicalVolumeStore[];
  
  // Deprecated - kept for backward compatibility
  raid_disks?: RaidDisk[];
  node_performance?: NodesPerformanceResponse;
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

// Enhanced mock data with correct storage hierarchy
const mockData: DashboardData = {
  // Physical disks (bottom of hierarchy)
  physical_disks: [
    {
      id: "nvme0n1",
      node: "worker-node-1", 
      pci_addr: "0000:01:00.0",
      capacity_gb: 1000,
      model: "Samsung SSD 980 PRO 1TB",
      healthy: true,
      blobstore_initialized: true
    },
    {
      id: "nvme1n1", 
      node: "worker-node-1",
      pci_addr: "0000:02:00.0", 
      capacity_gb: 1000,
      model: "Samsung SSD 980 PRO 1TB",
      healthy: true,
      blobstore_initialized: true
    },
    {
      id: "nvme2n1",
      node: "worker-node-2",
      pci_addr: "0000:01:00.0",
      capacity_gb: 2000,
      model: "WD Black SN850X 2TB", 
      healthy: true,
      blobstore_initialized: true
    },
    {
      id: "nvme3n1",
      node: "worker-node-2", 
      pci_addr: "0000:02:00.0",
      capacity_gb: 2000,
      model: "WD Black SN850X 2TB",
      healthy: true,
      blobstore_initialized: true
    }
  ],
  
  // RAID devices built from physical disks
  spdk_raids: [
    {
      name: "raid1_node1", 
      raid_level: 1,
      state: "online",
      member_disks: [], // Will be populated with references 
      num_members: 2,
      operational_members: 2,
      auto_rebuild_enabled: true,
      node: "worker-node-1",
      capacity_gb: 1000, // RAID 1 = capacity of smallest disk
      used_gb: 200
    },
    {
      name: "raid1_node2",
      raid_level: 1, 
      state: "online",
      member_disks: [],
      num_members: 2,
      operational_members: 2,
      auto_rebuild_enabled: true,
      node: "worker-node-2", 
      capacity_gb: 2000,
      used_gb: 800
    }
  ],
  
  // Logical Volume Stores built on RAID
  logical_volume_stores: [
    {
      name: "lvs_node1",
      base_raid: "raid1_node1",
      capacity_gb: 1000,
      used_gb: 200,
      utilization_pct: 20,
      cluster_size: 4096,
      node: "worker-node-1",
      logical_volumes: ["77777777-7777-7777-7777-777777777777", "88888888-8888-8888-8888-888888888888"]
    },
    {
      name: "lvs_node2", 
      base_raid: "raid1_node2",
      capacity_gb: 2000,
      used_gb: 800,
      utilization_pct: 40,
      cluster_size: 4096,
      node: "worker-node-2",
      logical_volumes: ["99999999-9999-9999-9999-999999999999", "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"]
    }
  ],
  
  volumes: [
    {
      id: "pvc-single-replica-volume",
      name: "single-replica-volume", 
      size: "20GB",
      state: "Healthy",
      replicas: 1,
      active_replicas: 1,
      local_nvme: true,
      access_method: "ublk",
      rebuild_progress: null,
      nodes: ["worker-node-1"],
      
      // Storage hierarchy references
      lvs_name: "lvs_node1",
      lvol_uuid: "77777777-7777-7777-7777-777777777777",
      
      // ublk device that exposes this logical volume
      ublk_device: {
        id: 42,
        device_path: "/dev/ublkb42"
      },
      
      nvmeof_enabled: false,
      nvmeof_targets: [],
      replica_statuses: [
        {
          node: "worker-node-1",
          status: "healthy", 
          is_local: true,
          last_io_timestamp: "2025-06-01T11:00:00Z",
          rebuild_progress: null,
          rebuild_target: null,
          is_new_replica: false,
          nvmf_target: null,
          access_method: "ublk",
          raid_member_state: "online",
          lvol_uuid: "77777777-7777-7777-7777-777777777777",
          disk_ref: "nvme0n1",
          replica_size: 21474836480
        }
      ],
      spdk_validation_status: {
        has_spdk_backing: true,
        validation_message: "Volume validated successfully",
        validation_severity: "info"
      }
    },
    {
      id: "pvc-12345678-1234-1234-1234-123456789abc",
      name: "postgres-data-pvc",
      size: "100GB",
      state: "Healthy",
      replicas: 3,
      active_replicas: 3,
      local_nvme: true,
      access_method: "ublk",
      rebuild_progress: null,
      nodes: ["worker-node-1", "worker-node-2", "worker-node-3"],
      
      // Storage hierarchy references
      lvs_name: "lvs_node2",
      lvol_uuid: "99999999-9999-9999-9999-999999999999",
      
      // ublk device that exposes this logical volume
      ublk_device: {
        id: 123,
        device_path: "/dev/ublkb123"
      },
      
      nvmeof_enabled: false,
      nvmeof_targets: [],

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
          access_method: "ublk",
          raid_member_slot: 0,
          raid_member_state: "online",
          lvol_uuid: "99999999-9999-9999-9999-999999999999",
          disk_ref: "nvme2n1",
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
          access_method: "nvmeof",
          raid_member_slot: 1,
          raid_member_state: "online",
          lvol_uuid: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
          disk_ref: "nvme3n1",
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
      ],
      spdk_validation_status: {
        has_spdk_backing: true,
        validation_message: "Volume validated successfully",
        validation_severity: "info"
      }
    },
    {
      id: "pvc-87654321-4321-4321-4321-cba987654321",
      name: "redis-cache-pvc",
      size: "50GB",
      state: "Degraded",
      replicas: 3,
      active_replicas: 2,
      local_nvme: true,
      access_method: "nvmeof",
      rebuild_progress: 75.5,
      nodes: ["worker-node-1", "worker-node-2", "worker-node-3"],
      // Add ublk device info
      ublk_device: {
        id: 12,
        device_path: "/dev/ublkb12"
      },
      
      nvmeof_enabled: false,
      nvmeof_targets: [],
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
      ],
      spdk_validation_status: {
        has_spdk_backing: false,
        validation_message: "SPDK backing not found - phantom volume",
        validation_severity: "error"
      }
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
      ],
      orphaned_spdk_volumes: []
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
      ],
      orphaned_spdk_volumes: [
        {
          spdk_volume_name: "orphaned_vol_123",
          spdk_volume_uuid: "abc12345-def6-7890-abcd-ef1234567890",
          size_blocks: 50331648,
          size_gb: 25.50,
          orphaned_since: "2025-06-01T10:00:00Z"
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
      ],
      orphaned_spdk_volumes: []
    }
  ],

  raw_volumes: [
    {
      name: "old_test_volume",
      uuid: "raw-12345678-1234-1234-1234-123456789abc",
      node: "worker-node-2",
      lvs_name: "lvs_worker-node-2-nvme1n1",
      size_blocks: 104857600,
      size_gb: 50.0,
      is_managed: false
    }
  ],

  nodes: ["worker-node-1", "worker-node-2", "worker-node-3"],
  
  // Legacy structures for backward compatibility  
  raid_disks: []
};

// Enhanced hook implementation with API integration
export const useDashboardData = (autoRefresh: boolean = true) => {
  const [data, setData] = useState<DashboardData>({
    volumes: [],
    raw_volumes: [],
    disks: [],
    nodes: [],
    physical_disks: [],
    spdk_raids: [],
    logical_volume_stores: [],
    raid_disks: []
  });
  const [loading, setLoading] = useState(true);
  const [usingMockData, setUsingMockData] = useState(false);
  
  // No need for complex pause logic - auto-refresh checkbox is managed automatically

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
  }, [data]);

  const refreshData = useCallback(async () => {
    try {
      setLoading(true);
      
      // Try to fetch from API, fall back to mock data
      try {
        const response = await fetch('/api/dashboard');
        if (response.ok) {
          // Check if response is actually JSON
          const contentType = response.headers.get('content-type');
          if (contentType && contentType.includes('application/json')) {
            const dashboardData = await response.json();
            const transformedData = transformBackendData(dashboardData);
            setData(transformedData);
            setUsingMockData(false);
          } else {
            // Got HTML or other non-JSON response, likely from proxy error
            throw new Error('Backend server not available (received HTML instead of JSON)');
          }
        } else {
          throw new Error(`Backend server error: ${response.status} ${response.statusText}`);
        }
      } catch (apiError) {
        // Provide user-friendly error messages
        let errorMessage = 'Dashboard API not available';
        if (apiError instanceof Error) {
          if (apiError.message.includes('Failed to fetch') || apiError.name === 'TypeError') {
            errorMessage = 'Backend server not reachable';
          } else if (apiError.message.includes('Unexpected token')) {
            errorMessage = 'Backend server returned invalid response';
          } else {
            errorMessage = apiError.message;
          }
        }
        
        console.warn(`${errorMessage}, using mock data for demo:`, apiError);
        // Use mock data for development/demo
        setData(mockData);
        setUsingMockData(true);
      }
    } catch (error) {
      console.error('Failed to fetch dashboard data:', error);
      // Use mock data as fallback
      setData(mockData);
      setUsingMockData(true);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refreshData();
  }, [refreshData]);

  useEffect(() => {
    if (!autoRefresh) return;

    const interval = setInterval(() => {
      console.log('✅ [DASHBOARD_AUTO_REFRESH] Running main dashboard auto-refresh');
      refreshData();
    }, 30000); // Refresh every 30 seconds
    
    return () => clearInterval(interval);
  }, [autoRefresh, refreshData]);

  return {
    data,
    loading,
    stats,
    refreshData,
    usingMockData
  };
};

// Transform backend data structure to frontend interface
const transformBackendData = (backendData: any): DashboardData => {
  return {
    volumes: backendData.volumes?.map((vol: any) => ({
      ...vol,
      // Map new hierarchy fields
      lvs_name: vol.lvs_name || "unknown",
      lvol_uuid: vol.lvol_uuid || vol.primary_lvol_uuid || "unknown",
      // Remove old raid_status field - RAID is now separate from volumes
      nvmeof_targets: vol.nvmeof_targets || [],
    })) || [],
    raw_volumes: backendData.raw_volumes || [],
    disks: backendData.disks?.map((disk: any) => ({
      ...disk,
      // Ensure compatibility with existing frontend code
      blobstore_initialized: disk.blobstore_initialized
    })) || [],
    nodes: backendData.nodes || [],
    
    // New storage hierarchy
    physical_disks: backendData.physical_disks || [],
    spdk_raids: backendData.spdk_raids || [],
    logical_volume_stores: backendData.logical_volume_stores || [],
    
    // Legacy compatibility
    raid_disks: backendData.raid_disks || []
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
  
  // Get dashboard data to cross-reference SpdkDisk CRD status
  const { data: dashboardData } = useDashboardData(false);
  
  // No need for complex pause logic - auto-refresh checkbox is managed automatically

  const refreshNodeDisks = useCallback(async (nodeName: string) => {
    console.log(`🚨 [REFRESH_TRIGGER] refreshNodeDisks called for: ${nodeName}`);
    console.log(`🔍 [REFRESH_TRIGGER] Call stack:`, new Error().stack);
    
    try {
      setRefreshing(prev => new Set([...prev, nodeName]));
      setNodeData(prev => ({
        ...prev,
        [nodeName]: { ...prev[nodeName], loading: true, error: undefined }
      }));

      // Enhanced mock API call for development/demo with varied disk states
      const mockDisks: UnimplementedDisk[] = [
        // FREE DISKS - Need full setup (driver + LVS)
        {
          pci_address: "0000:3d:00.0",
          device_name: "nvme0n1",
          vendor_id: "0x144d",
          device_id: "0xa80a",
          subsystem_vendor_id: "0x144d", 
          subsystem_device_id: "0xa801",
          numa_node: 0,
          driver: "nvme",
          size_bytes: 1000204886016,
          model: "Samsung SSD 980 PRO 1TB",
          serial: "S5P2NG0R123456",
          firmware_version: "5B2QGXA7",
          namespace_id: 1,
          mounted_partitions: [],
          filesystem_type: undefined,
          is_system_disk: false,
          spdk_ready: false,
          discovered_at: new Date().toISOString(),
          nodeName,
          driver_ready: false,
          blobstore_initialized: false
        },
        {
          pci_address: "0000:3f:00.0",
          device_name: "nvme2n1",
          vendor_id: "0x15b7",
          device_id: "0x5006",
          subsystem_vendor_id: "0x15b7",
          subsystem_device_id: "0x5006",
          numa_node: 1,
          driver: "nvme",
          size_bytes: 512110190592,
          model: "WD Black SN750 500GB",
          serial: "WDS500G3X0C123",
          firmware_version: "111130WD",
          namespace_id: 1,
          mounted_partitions: [],
          filesystem_type: undefined,
          is_system_disk: false,
          spdk_ready: false,
          discovered_at: new Date().toISOString(),
          nodeName,
          driver_ready: false,
          blobstore_initialized: false
        },
        
        // DRIVER READY DISKS - Have SPDK driver, need LVS initialization
        {
          pci_address: "0000:3e:00.0", 
          device_name: "nvme1n1",
          vendor_id: "0x144d",
          device_id: "0xa80a",
          subsystem_vendor_id: "0x144d",
          subsystem_device_id: "0xa801", 
          numa_node: 0,
          driver: "vfio-pci",
          size_bytes: 1000204886016,
          model: "Samsung SSD 980 PRO 1TB",
          serial: "S5P2NG0R654321",
          firmware_version: "5B2QGXA7",
          namespace_id: 1,
          mounted_partitions: [],
          filesystem_type: undefined,
          is_system_disk: false,
          spdk_ready: true,
          discovered_at: new Date().toISOString(),
          nodeName,
          driver_ready: true,
          blobstore_initialized: false
        },
        {
          pci_address: "0000:5a:00.0",
          device_name: "nvme3n1",
          vendor_id: "0x15b7",
          device_id: "0x5006",
          subsystem_vendor_id: "0x15b7",
          subsystem_device_id: "0x5006",
          numa_node: 1,
          driver: "uio_pci_generic",
          size_bytes: 2000398934016,
          model: "WD Black SN850 2TB",
          serial: "WDS200T1X0E456",
          firmware_version: "613000WD",
          namespace_id: 1,
          mounted_partitions: [],
          filesystem_type: undefined,
          is_system_disk: false,
          spdk_ready: true,
          discovered_at: new Date().toISOString(),
          nodeName,
          driver_ready: true,
          blobstore_initialized: false
        },
        
        // LVS READY DISKS - Fully configured and ready for volumes
        {
          pci_address: "0000:4b:00.0",
          device_name: "nvme4n1",
          vendor_id: "0x144d",
          device_id: "0xa80a",
          subsystem_vendor_id: "0x144d",
          subsystem_device_id: "0xa801",
          numa_node: 0,
          driver: "vfio-pci",
          size_bytes: 1000204886016,
          model: "Samsung SSD 980 PRO 1TB",
          serial: "S5P2NG0R789012",
          firmware_version: "5B2QGXA7",
          namespace_id: 1,
          mounted_partitions: [],
          filesystem_type: undefined,
          is_system_disk: false,
          spdk_ready: true,
          discovered_at: new Date().toISOString(),
          nodeName,
          driver_ready: true,
          blobstore_initialized: true
        },
        {
          pci_address: "0000:6c:00.0",
          device_name: "nvme5n1",
          vendor_id: "0x1c5c",
          device_id: "0x1327",
          subsystem_vendor_id: "0x1c5c",
          subsystem_device_id: "0x0000",
          numa_node: 1,
          driver: "vfio-pci",
          size_bytes: 3840755982336,
          model: "Micron 7450 PRO 3.84TB",
          serial: "MSA2642KFXG45T",
          firmware_version: "E013",
          namespace_id: 1,
          mounted_partitions: [],
          filesystem_type: undefined,
          is_system_disk: false,
          spdk_ready: true,
          discovered_at: new Date().toISOString(),
          nodeName,
          driver_ready: true,
          blobstore_initialized: true
        },
        
        // NEEDS UNMOUNT - Has mounted filesystems
        {
          pci_address: "0000:7d:00.0",
          device_name: "nvme6n1",
          vendor_id: "0x144d",
          device_id: "0xa80a",
          subsystem_vendor_id: "0x144d",
          subsystem_device_id: "0xa801",
          numa_node: 0,
          driver: "nvme",
          size_bytes: 500107862016,
          model: "Samsung SSD 980 500GB",
          serial: "S5P2NG0R345678",
          firmware_version: "5B2QGXA7",
          namespace_id: 1,
          mounted_partitions: ["/data", "/logs"],
          filesystem_type: "ext4",
          is_system_disk: false,
          spdk_ready: false,
          discovered_at: new Date().toISOString(),
          nodeName,
          driver_ready: false,
          blobstore_initialized: false
        },
        
        // SYSTEM DISK - Cannot be used for SPDK
        {
          pci_address: "0000:8e:00.0",
          device_name: "nvme7n1",
          vendor_id: "0x144d",
          device_id: "0xa80a",
          subsystem_vendor_id: "0x144d",
          subsystem_device_id: "0xa801",
          numa_node: 0,
          driver: "nvme",
          size_bytes: 256060514304,
          model: "Samsung SSD 980 256GB",
          serial: "S5P2NG0R567890",
          firmware_version: "5B2QGXA7",
          namespace_id: 1,
          mounted_partitions: ["/", "/boot", "/var"],
          filesystem_type: "ext4",
          is_system_disk: true,
          spdk_ready: false,
          discovered_at: new Date().toISOString(),
          nodeName,
          driver_ready: false,
          blobstore_initialized: false
        }
      ];

      // Discovery calls disabled: reflect only current CRD-backed disks for this node (no discovery)
      const crdBackedDisks: UnimplementedDisk[] = dashboardData.disks
        .filter(d => d.node === nodeName)
        .map(d => ({
          pci_address: d.pci_addr || '',
          device_name: d.id,
          vendor_id: '',
          device_id: '',
          subsystem_vendor_id: '',
          subsystem_device_id: '',
          numa_node: undefined,
          driver: d.blobstore_initialized ? 'vfio-pci' : 'nvme',
          size_bytes: d.capacity,
          model: d.model,
          serial: '',
          namespace_id: undefined,
          mounted_partitions: [],
          filesystem_type: undefined,
          is_system_disk: false,
          spdk_ready: d.blobstore_initialized,
          discovered_at: new Date().toISOString(),
          nodeName,
          driver_ready: d.blobstore_initialized,
          blobstore_initialized: d.blobstore_initialized,
        }));

      setNodeData(prev => ({
        ...prev,
        [nodeName]: {
          node: nodeName,
          disks: crdBackedDisks,
          loading: false,
          last_updated: new Date().toISOString()
        }
      }));

    } catch (error) {
      console.error(`Failed to refresh disks for ${nodeName}:`, error);
      setNodeData(prev => ({
        ...prev,
        [nodeName]: {
          ...prev[nodeName],
          loading: false,
          error: error instanceof Error ? error.message : 'Unknown error'
        }
      }));
    } finally {
      setRefreshing(prev => {
        const newSet = new Set(prev);
        newSet.delete(nodeName);
        return newSet;
      });
    }
  }, [dashboardData.disks]);

  const setupDisksOnNode = useCallback(async (
    nodeName: string, 
    request: DiskSetupRequest
  ): Promise<DiskSetupResult> => {
    try {
      const response = await fetch(`/api/nodes/${nodeName}/disks/setup`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request)
      });

      if (response.ok) {
        const result = await response.json();
        
        // Refresh node data after setup to show new status
        if (result.success) {
          setTimeout(() => refreshNodeDisks(nodeName), 2000);
        }
        
        return result;
      } else {
        throw new Error(`Setup request failed: ${response.statusText}`);
      }
    } catch (apiError) {
      console.warn(`Disk setup API not available for ${nodeName}, using mock result:`, apiError);
      
      // Mock successful setup for demo
      const mockResult: DiskSetupResult = {
        success: true,
        setup_disks: request.pci_addresses,
        failed_disks: [],
        warnings: [],
        huge_pages_configured: request.huge_pages_mb,
        completed_at: new Date().toISOString()
      };

      // Simulate the setup by updating local state
      setTimeout(() => {
        setNodeData(prev => {
          const nodeDisks = prev[nodeName]?.disks || [];
          const updatedDisks = nodeDisks.map(disk => {
            if (request.pci_addresses.includes(disk.pci_address)) {
              return {
                ...disk,
                driver: request.driver_override || 'vfio-pci',
                spdk_ready: true,
                mounted_partitions: request.force_unmount ? [] : disk.mounted_partitions
              };
            }
            return disk;
          });

          return {
            ...prev,
            [nodeName]: {
              ...prev[nodeName],
              disks: updatedDisks,
              last_updated: new Date().toISOString()
            }
          };
        });
      }, 1000);

      return mockResult;
    }
  }, [refreshNodeDisks]);

  const resetDisksOnNode = useCallback(async (
    nodeName: string, 
    pciAddresses: string[]
  ): Promise<any> => {
    try {
      const response = await fetch(`/api/nodes/${nodeName}/disks/reset`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ pci_addresses: pciAddresses })
      });

      if (response.ok) {
        const result = await response.json();
        
        // Refresh node data after reset to show new status
        if (result.success) {
          setTimeout(() => refreshNodeDisks(nodeName), 2000);
        }
        
        return result;
      } else {
        throw new Error(`Reset request failed: ${response.statusText}`);
      }
    } catch (apiError) {
      console.warn(`Disk reset API not available for ${nodeName}, using mock result:`, apiError);
      
      // Mock successful reset for demo
      const mockResult = {
        success: true,
        reset_disks: pciAddresses,
        failed_disks: [],
        completed_at: new Date().toISOString()
      };

      // Simulate the reset by updating local state
      setTimeout(() => {
        setNodeData(prev => {
          const nodeDisks = prev[nodeName]?.disks || [];
          const updatedDisks = nodeDisks.map(disk => {
            if (pciAddresses.includes(disk.pci_address)) {
              return {
                ...disk,
                driver: 'nvme',
                spdk_ready: false,
                mounted_partitions: []
              };
            }
            return disk;
          });

          return {
            ...prev,
            [nodeName]: {
              ...prev[nodeName],
              disks: updatedDisks,
              last_updated: new Date().toISOString()
            }
          };
        });
      }, 1000);

      return mockResult;
    }
  }, [refreshNodeDisks, setNodeData]);

  const initializeBlobstoreOnNode = useCallback(async (
    nodeName: string, 
    pciAddresses: string[]
  ): Promise<DiskSetupResult> => {
    try {
      const response = await fetch(`/api/nodes/${nodeName}/disks/initialize`, {
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
          setTimeout(() => refreshNodeDisks(nodeName), 2000);
        }
        
        return normalizedResult;
      } else {
        throw new Error(`Initialize blobstore request failed: ${response.statusText}`);
      }
    } catch (apiError) {
      console.warn(`Disk initialize blobstore API not available for ${nodeName}, using mock result:`, apiError);
      
      // Mock successful initialization for demo
      const mockResult: DiskSetupResult = {
        success: true,
        setup_disks: pciAddresses,
        failed_disks: [],
        warnings: ["This is a mock result for development"],
        completed_at: new Date().toISOString()
      };

      // Simulate progress updates
      setTimeout(() => {
        mockResult.warnings.push("Blobstore initialization completed (mock)");
      }, 1000);

      return mockResult;
    }
  }, [refreshNodeDisks]);

  const deleteDiskOnNode = useCallback(async (
    nodeName: string,
    pciAddress: string
  ): Promise<any> => {
    try {
      const response = await fetch(`/api/nodes/${nodeName}/disks/delete`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ pci_address: pciAddress })
      });

      if (response.ok) {
        const result = await response.json();
        
        // Refresh node data after deletion to show new status
        if (result.success) {
          setTimeout(() => refreshNodeDisks(nodeName), 2000);
        }
        
        return result;
      } else {
        throw new Error(`Delete request failed: ${response.statusText}`);
      }
    } catch (apiError) {
      console.warn(`Disk delete API not available for ${nodeName}, using mock result:`, apiError);
      
      // Mock successful deletion for demo
      const mockResult = {
        success: true,
        message: 'SPDK disk successfully deleted and reset to kernel mode',
        deleted_volumes: [],
        cleanup_performed: {
          lvs_deleted: true,
          volumes_deleted: 0,
          disk_reset: true
        },
        completed_at: new Date().toISOString()
      };

      // Simulate the deletion by updating local state
      setTimeout(() => {
        setNodeData(prev => {
          const nodeDisks = prev[nodeName]?.disks || [];
          const updatedDisks = nodeDisks.map(disk => {
            if (disk.pci_address === pciAddress) {
              return {
                ...disk,
                driver: 'nvme',
                spdk_ready: false,
                mounted_partitions: []
              };
            }
            return disk;
          });

          return {
            ...prev,
            [nodeName]: {
              ...prev[nodeName],
              disks: updatedDisks,
              last_updated: new Date().toISOString()
            }
          };
        });
      }, 1000);

      return mockResult;
    }
  }, [refreshNodeDisks, setNodeData]);

  return {
    nodeData,
    setNodeData,
    refreshNodeDisks,
    setupDisksOnNode,
    resetDisksOnNode,
    initializeBlobstoreOnNode,
    deleteDiskOnNode,
    refreshing: refreshing.size > 0
  };
};

export default useDashboardData;
