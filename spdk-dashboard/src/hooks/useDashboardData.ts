import { useState, useEffect, useCallback } from 'react';

// Backend API configuration
const API_BASE_URL = process.env.NODE_ENV === 'production' 
  ? '/api' 
  : 'http://localhost:8080/api';

// Types - Export all interfaces that will be used by components
export interface NvmfTarget {
  nqn: string;
  target_ip: string;
  target_port: string;
  transport_type: string;
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
}

export interface Volume {
  id: string;
  name: string;
  size: string;
  state: string;
  replicas: number;
  active_replicas: number;
  local_nvme: boolean;
  rebuild_progress: number | null;
  nodes: string[];
  replica_statuses: ReplicaStatus[];
}

export interface ProvisionedVolume {
  volume_name: string;
  volume_id: string;
  size: number;
  provisioned_at: string;
  replica_type: string;
  status: string;
}

export interface Disk {
  id: string;
  node: string;
  pci_addr: string;
  capacity: number;
  capacity_gb: number;
  allocated_space: number;
  free_space: number;
  free_space_display: string;
  healthy: boolean;
  lvol_store_initialized: boolean;
  lvol_count: number;
  model: string;
  read_iops: number;
  write_iops: number;
  read_latency: number;
  write_latency: number;
  brought_online: string;
  provisioned_volumes: ProvisionedVolume[];
}

export interface DashboardData {
  volumes: Volume[];
  disks: Disk[];
  nodes: string[];
}

// Backend API service
const apiService = {
  token: '', // Store in memory instead of localStorage
  
  login: async (username: string, password: string) => {
    // For now, keep the mock authentication since backend doesn't implement auth yet
    await new Promise(resolve => setTimeout(resolve, 1000));
    if (username === 'admin' && password === 'spdk-admin-2025') {
      apiService.token = 'mock-token';
      return { success: true };
    }
    throw new Error('Invalid credentials');
  },
  
  logout: () => {
    apiService.token = '';
  },
  
  isAuthenticated: () => {
    return !!apiService.token;
  },

  async fetchDashboardData(): Promise<DashboardData> {
    const response = await fetch(`${API_BASE_URL}/dashboard`);
    if (!response.ok) {
      throw new Error(`Failed to fetch dashboard data: ${response.statusText}`);
    }
    return response.json();
  },

  async refreshData() {
    const response = await fetch(`${API_BASE_URL}/refresh`, {
      method: 'POST'
    });
    if (!response.ok) {
      throw new Error(`Failed to refresh data: ${response.statusText}`);
    }
    return response.json();
  },

  async getVolumeDetails(volumeId: string) {
    const response = await fetch(`${API_BASE_URL}/volumes/${volumeId}`);
    if (!response.ok) {
      throw new Error(`Failed to fetch volume details: ${response.statusText}`);
    }
    return response.json();
  },

  async getNodeMetrics(node: string) {
    const response = await fetch(`${API_BASE_URL}/nodes/${node}/metrics`);
    if (!response.ok) {
      throw new Error(`Failed to fetch node metrics: ${response.statusText}`);
    }
    return response.json();
  }
};

// Fallback mock data for development/offline scenarios
const generateFallbackData = (): DashboardData => {
  const volumes: Volume[] = [];
  const disks: Disk[] = [];
  const nodes = ['node-a', 'node-b', 'node-c', 'node-d', 'node-e'];
  
  // Generate mock volumes (representing PVCs -> SPDK logical volumes)
  for (let i = 1; i <= 8; i++) {
    const totalReplicas = Math.floor(Math.random() * 3 + 2);
    const selectedNodes = nodes.slice(0, totalReplicas);
    const isHealthy = Math.random() > 0.3;
    const isRebuilding = !isHealthy && Math.random() > 0.4;
    const hasLocalNVMe = Math.random() > 0.3;
    
    const replicaStatuses: ReplicaStatus[] = selectedNodes.map((node, index) => {
      const isLocal = index === 0 && hasLocalNVMe;
      const status = isHealthy ? 'healthy' : 
                    (Math.random() > 0.6 ? 'failed' : 
                     (Math.random() > 0.5 ? 'rebuilding' : 'healthy'));
      
      return {
        node,
        status,
        is_local: isLocal,
        last_io_timestamp: status !== 'failed' ? new Date(Date.now() - Math.random() * 3600000).toISOString() : null,
        rebuild_progress: status === 'rebuilding' ? Math.floor(Math.random() * 90 + 10) : null,
        rebuild_target: null,
        is_new_replica: false,
        nvmf_target: !isLocal ? {
          nqn: `nqn.2025-05.io.spdk:vol-${i}-replica-${index}`,
          target_ip: `192.168.1.${100 + nodes.indexOf(node)}`,
          target_port: '4420',
          transport_type: 'TCP'
        } : null
      };
    });
    
    volumes.push({
      id: `spdk-vol-${i}`,
      name: `pvc-workload-${i}`,
      size: `${Math.floor(Math.random() * 500 + 100)}GB`,
      state: isRebuilding ? 'Rebuilding' : (isHealthy ? 'Healthy' : 'Degraded'),
      replicas: totalReplicas,
      active_replicas: replicaStatuses.filter(r => r.status === 'healthy' || r.status === 'rebuilding').length,
      local_nvme: hasLocalNVMe,
      rebuild_progress: isRebuilding ? Math.floor(Math.random() * 90 + 10) : null,
      nodes: selectedNodes,
      replica_statuses: replicaStatuses
    });
  }
  
  // Generate mock disks (1:1 with SPDK logical volume stores)
  nodes.forEach((node, nodeIndex) => {
    for (let i = 1; i <= Math.floor(Math.random() * 3 + 2); i++) {
      const isInitialized = Math.random() > 0.2;
      const isHealthy = Math.random() > 0.1;
      const totalCapacity = Math.floor(Math.random() * 1000 + 500);
      const allocatedSpace = isInitialized ? Math.floor(Math.random() * (totalCapacity * 0.7)) : 0;
      
      disks.push({
        id: `${node}-nvme${i}`,
        node,
        pci_addr: `0000:${(nodeIndex * 10 + i).toString(16).padStart(2, '0')}:00.0`,
        capacity: totalCapacity * 1024 * 1024 * 1024, // Backend expects bytes
        capacity_gb: totalCapacity,
        allocated_space: allocatedSpace,
        free_space: totalCapacity - allocatedSpace,
        free_space_display: `${totalCapacity - allocatedSpace}GB`,
        healthy: isHealthy,
        lvol_store_initialized: isInitialized,
        lvol_count: isInitialized ? Math.floor(Math.random() * 5) : 0,
        model: `Samsung NVMe SSD ${Math.floor(Math.random() * 3 + 1)}TB`,
        read_iops: Math.floor(Math.random() * 50000 + 10000),
        write_iops: Math.floor(Math.random() * 40000 + 8000),
        read_latency: Math.floor(Math.random() * 100 + 20),
        write_latency: Math.floor(Math.random() * 150 + 30),
        brought_online: new Date(Date.now() - Math.random() * 30 * 24 * 60 * 60 * 1000).toISOString(),
        provisioned_volumes: []
      });
    }
  });
  
  // Map logical volumes to their corresponding volume stores on disks
  volumes.forEach(volume => {
    volume.replica_statuses.forEach(replica => {
      if (replica.status === 'healthy' || replica.status === 'rebuilding') {
        const availableDisks = disks.filter(d => d.node === replica.node && d.lvol_store_initialized);
        if (availableDisks.length > 0) {
          const disk = availableDisks[Math.floor(Math.random() * availableDisks.length)];
          const volumeSize = parseInt(volume.size.replace('GB', ''));
          
          disk.provisioned_volumes.push({
            volume_name: volume.name,
            volume_id: volume.id,
            size: volumeSize,
            provisioned_at: new Date(Date.now() - Math.random() * 20 * 24 * 60 * 60 * 1000).toISOString(),
            replica_type: replica.is_local ? 'Local NVMe' : 'NVMe-oF',
            status: replica.status
          });
        }
      }
    });
  });
  
  return { volumes, disks, nodes };
};

// Data fetching function
const fetchDashboardData = async (): Promise<DashboardData> => {
  try {
    return await apiService.fetchDashboardData();
  } catch (error) {
    console.error('Failed to fetch real data, falling back to mock data:', error);
    return generateFallbackData();
  }
};

// Authentication hook
export const useAuth = () => {
  const [isAuthenticated, setIsAuthenticated] = useState(apiService.isAuthenticated());

  const login = async (username: string, password: string) => {
    await apiService.login(username, password);
    setIsAuthenticated(true);
  };

  const logout = () => {
    apiService.logout();
    setIsAuthenticated(false);
  };

  return { isAuthenticated, login, logout };
};

// Dashboard data hook
export const useDashboardData = (autoRefresh: boolean = true) => {
  const [data, setData] = useState<DashboardData>({ volumes: [], disks: [], nodes: [] });
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const fetchData = useCallback(async () => {
    try {
      setLoading(true);
      setError(null);
      
      // Simulate network delay for better UX
      await new Promise(resolve => setTimeout(resolve, 500));
      
      const dashboardData = await fetchDashboardData();
      setData(dashboardData);
    } catch (err) {
      console.error('Failed to fetch dashboard data:', err);
      setError(err instanceof Error ? err.message : 'Unknown error occurred');
      
      // Use fallback data on error
      setData(generateFallbackData());
    } finally {
      setLoading(false);
    }
  }, []);

  const refreshData = useCallback(async () => {
    try {
      await apiService.refreshData();
      await fetchData();
    } catch (err) {
      console.error('Failed to refresh data:', err);
      setError('Failed to refresh data');
    }
  }, [fetchData]);

  const getVolumeDetails = useCallback(async (volumeId: string) => {
    try {
      return await apiService.getVolumeDetails(volumeId);
    } catch (err) {
      console.error('Failed to get volume details:', err);
      throw err;
    }
  }, []);

  const getNodeMetrics = useCallback(async (node: string) => {
    try {
      return await apiService.getNodeMetrics(node);
    } catch (err) {
      console.error('Failed to get node metrics:', err);
      throw err;
    }
  }, []);

  useEffect(() => {
    fetchData();
  }, [fetchData]);

  useEffect(() => {
    if (autoRefresh) {
      const interval = setInterval(fetchData, 30000); // Refresh every 30 seconds
      return () => clearInterval(interval);
    }
  }, [autoRefresh, fetchData]);

  // Computed values
  const faultedVolumes = data.volumes.filter(v => v.state === 'Degraded' || v.state === 'Failed');
  const rebuildingVolumes = data.volumes.filter(v => v.state === 'Rebuilding');
  const localNVMeVolumes = data.volumes.filter(v => v.local_nvme);
  const healthyDisks = data.disks.filter(d => d.healthy).length;
  const formattedDisks = data.disks.filter(d => d.lvol_store_initialized).length;

  return {
    data,
    loading,
    error,
    refreshData,
    fetchData,
    getVolumeDetails,
    getNodeMetrics,
    // Computed statistics
    stats: {
      totalVolumes: data.volumes.length,
      faultedVolumes: faultedVolumes.length,
      rebuildingVolumes: rebuildingVolumes.length,
      localNVMeVolumes: localNVMeVolumes.length,
      totalDisks: data.disks.length,
      healthyDisks,
      formattedDisks
    }
  };
};
