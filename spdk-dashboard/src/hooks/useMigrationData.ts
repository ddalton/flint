import { useState, useEffect } from 'react';

// Migration-specific interfaces
export interface AvailableDisk {
  id: string;
  node: string;
  pci_addr: string;
  capacity_gb: number;
  model: string;
  healthy: boolean;
  blobstore_initialized: boolean;
  available: boolean; // Not currently in use
  free_space_gb: number;
}

export interface AvailableNvmeofTarget {
  id: string;
  nqn: string;
  target_ip: string;
  target_port: number;
  transport: string;
  node: string;
  bdev_name: string;
  active: boolean;
  capacity_gb?: number;
  type: 'internal' | 'external';
  last_connected?: string;
  connection_status: 'connected' | 'disconnected' | 'error';
}

export interface RaidMemberInfo {
  slot: number;
  name: string;
  state: 'online' | 'degraded' | 'failed' | 'rebuilding';
  node?: string;
  disk_ref?: string;
  health_status: 'healthy' | 'degraded' | 'failed';
  capacity_gb?: number;
}

export interface DetailedRaidInfo {
  name: string;
  raid_level: number;
  state: 'online' | 'degraded' | 'failed' | 'rebuilding';
  members: RaidMemberInfo[];
  node: string;
  capacity_gb: number;
  used_gb: number;
  rebuild_progress?: number;
  auto_rebuild_enabled: boolean;
}

interface MigrationDataResponse {
  available_disks: AvailableDisk[];
  available_nvmeof_targets: AvailableNvmeofTarget[];
  raid_info?: DetailedRaidInfo;
}

interface UseMigrationDataReturn {
  availableDisks: AvailableDisk[];
  availableNvmeofTargets: AvailableNvmeofTarget[];
  raidInfo: DetailedRaidInfo | null;
  loading: boolean;
  error: string | null;
  refreshData: () => Promise<void>;
}

export const useMigrationData = (
  volumeId?: string,
  raidName?: string,
  includeCurrentNode: boolean = false
): UseMigrationDataReturn => {
  const [availableDisks, setAvailableDisks] = useState<AvailableDisk[]>([]);
  const [availableNvmeofTargets, setAvailableNvmeofTargets] = useState<AvailableNvmeofTarget[]>([]);
  const [raidInfo, setRaidInfo] = useState<DetailedRaidInfo | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchMigrationData = async () => {
    setLoading(true);
    setError(null);
    
    try {
      const params = new URLSearchParams();
      if (volumeId) params.append('volume_id', volumeId);
      if (raidName) params.append('raid_name', raidName);
      if (includeCurrentNode) params.append('include_current_node', 'true');

      const response = await fetch(`/api/migration/targets?${params}`);
      if (!response.ok) {
        throw new Error(`Failed to fetch migration data: ${response.status}`);
      }

      const data: MigrationDataResponse = await response.json();
      
      setAvailableDisks(data.available_disks || []);
      setAvailableNvmeofTargets(data.available_nvmeof_targets || []);
      setRaidInfo(data.raid_info || null);
      
    } catch (err) {
      const errorMessage = err instanceof Error ? err.message : 'Failed to fetch migration data';
      setError(errorMessage);
      console.error('Error fetching migration data:', err);
    } finally {
      setLoading(false);
    }
  };

  const refreshData = async () => {
    await fetchMigrationData();
  };

  useEffect(() => {
    if (volumeId || raidName) {
      fetchMigrationData();
    }
  }, [volumeId, raidName, includeCurrentNode]);

  return {
    availableDisks,
    availableNvmeofTargets,
    raidInfo,
    loading,
    error,
    refreshData
  };
};

// Mock data for development/testing
export const getMockMigrationData = (): MigrationDataResponse => ({
  available_disks: [
    {
      id: 'nvme2n1',
      node: 'worker-node-2',
      pci_addr: '0000:03:00.0',
      capacity_gb: 1000,
      model: 'Samsung SSD 980 PRO 1TB',
      healthy: true,
      blobstore_initialized: true,
      available: true,
      free_space_gb: 800
    },
    {
      id: 'nvme3n1',
      node: 'worker-node-3',
      pci_addr: '0000:01:00.0',
      capacity_gb: 2000,
      model: 'WD Black SN850X 2TB',
      healthy: true,
      blobstore_initialized: true,
      available: true,
      free_space_gb: 1500
    }
  ],
  available_nvmeof_targets: [
    {
      id: 'nvmeof-internal-1',
      nqn: 'nqn.2023.io.spdk:internal-storage-1',
      target_ip: '192.168.1.100',
      target_port: 4420,
      transport: 'tcp',
      node: 'worker-node-2',
      bdev_name: 'nvmeof-internal-1',
      active: true,
      capacity_gb: 5000,
      type: 'internal',
      connection_status: 'connected'
    },
    {
      id: 'nvmeof-external-1',
      nqn: 'nqn.2023.io.external:san-array-1',
      target_ip: '10.0.1.50',
      target_port: 4420,
      transport: 'tcp',
      node: 'external',
      bdev_name: 'san-array-1-ns1',
      active: true,
      capacity_gb: 10000,
      type: 'external',
      connection_status: 'connected'
    }
  ],
  raid_info: {
    name: 'raid1_node1',
    raid_level: 1,
    state: 'online',
    members: [
      {
        slot: 0,
        name: 'nvme0n1',
        state: 'online',
        node: 'worker-node-1',
        disk_ref: 'nvme0n1',
        health_status: 'healthy',
        capacity_gb: 1000
      },
      {
        slot: 1,
        name: 'nvme1n1',
        state: 'online',
        node: 'worker-node-1',
        disk_ref: 'nvme1n1',
        health_status: 'healthy',
        capacity_gb: 1000
      }
    ],
    node: 'worker-node-1',
    capacity_gb: 1000,
    used_gb: 200,
    auto_rebuild_enabled: true
  }
});

// SPDK JSON-RPC operation mapping
export const getSpdkRpcMethod = (operationType: string, targetType: string): string => {
  switch (operationType) {
    case 'node_migration':
      return 'bdev_raid_create'; // Create RAID on new node, then migrate data
    case 'member_migration':
      switch (targetType) {
        case 'local_disk':
          return 'bdev_raid_replace_member';
        case 'internal_nvmeof':
        case 'external_nvmeof':
          return 'bdev_raid_replace_member'; // Same method, different bdev type
        default:
          return 'bdev_raid_replace_member';
      }
    case 'member_addition':
      return 'bdev_raid_add_member';
    default:
      return 'bdev_raid_create';
  }
};

// Validation helpers
export const validateMigrationTarget = (
  operationType: string,
  targetType: string,
  targetId: string,
  availableTargets: any[]
): { valid: boolean; message?: string } => {
  if (!targetId) {
    return { valid: false, message: 'No target selected' };
  }

  const target = availableTargets.find(t => 
    t.id === targetId || t.nqn === targetId
  );

  if (!target) {
    return { valid: false, message: 'Selected target not found' };
  }

  if (targetType.includes('nvmeof') && target.connection_status !== 'connected') {
    return { valid: false, message: 'NVMe-oF target is not connected' };
  }

  if (targetType === 'local_disk' && (!target.available || !target.healthy)) {
    return { valid: false, message: 'Selected disk is not available or healthy' };
  }

  return { valid: true };
};


