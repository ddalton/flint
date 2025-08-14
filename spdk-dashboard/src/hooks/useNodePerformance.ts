import { useState, useEffect, useCallback } from 'react';
import type { NodesPerformanceResponse, NodePerformanceMetrics } from './useDashboardData';

interface UseNodePerformanceReturn {
  performanceData: NodesPerformanceResponse | null;
  loading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
  getNodeMetrics: (nodeId: string) => NodePerformanceMetrics | null;
}

export const useNodePerformance = (refreshInterval: number = 30000): UseNodePerformanceReturn => {
  const [performanceData, setPerformanceData] = useState<NodesPerformanceResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const fetchPerformanceData = useCallback(async () => {
    try {
      const response = await fetch('/api/nodes/performance');
      
      if (!response.ok) {
        throw new Error(`Failed to fetch performance data: ${response.statusText}`);
      }
      
      const data: NodesPerformanceResponse = await response.json();
      setPerformanceData(data);
      setError(null);
    } catch (err) {
      console.error('Failed to fetch node performance data:', err);
      setError(err instanceof Error ? err.message : 'Unknown error');
      
      // Fallback to mock data for development
      setPerformanceData(getMockPerformanceData());
    } finally {
      setLoading(false);
    }
  }, []);

  const refresh = useCallback(async () => {
    setLoading(true);
    await fetchPerformanceData();
  }, [fetchPerformanceData]);

  const getNodeMetrics = useCallback((nodeId: string): NodePerformanceMetrics | null => {
    return performanceData?.nodes.find(node => node.node_id === nodeId) || null;
  }, [performanceData]);

  // Initial load
  useEffect(() => {
    fetchPerformanceData();
  }, [fetchPerformanceData]);

  // Auto-refresh
  useEffect(() => {
    if (refreshInterval > 0) {
      const interval = setInterval(fetchPerformanceData, refreshInterval);
      return () => clearInterval(interval);
    }
  }, [fetchPerformanceData, refreshInterval]);

  return {
    performanceData,
    loading,
    error,
    refresh,
    getNodeMetrics,
  };
};

// Mock data for development/fallback
function getMockPerformanceData(): NodesPerformanceResponse {
  return {
    nodes: [
      {
        node_id: 'worker-node-1',
        raid_count: 3,
        volume_count: 8,
        total_read_iops: 2500,
        total_write_iops: 1800,
        total_read_bandwidth_mbps: 125.5,
        total_write_bandwidth_mbps: 89.2,
        avg_read_latency_ms: 2.1,
        avg_write_latency_ms: 3.4,
        spdk_active: true,
        last_updated: new Date().toISOString(),
        failed_raids: 0,
        degraded_raids: 1,
        healthy_raids: 2,
        performance_score: 87.5,
      },
      {
        node_id: 'worker-node-2',
        raid_count: 5,
        volume_count: 12,
        total_read_iops: 4200,
        total_write_iops: 3100,
        total_read_bandwidth_mbps: 210.8,
        total_write_bandwidth_mbps: 155.3,
        avg_read_latency_ms: 1.8,
        avg_write_latency_ms: 2.9,
        spdk_active: true,
        last_updated: new Date().toISOString(),
        failed_raids: 0,
        degraded_raids: 0,
        healthy_raids: 5,
        performance_score: 92.3,
      },
      {
        node_id: 'worker-node-3',
        raid_count: 7,
        volume_count: 15,
        total_read_iops: 1800,
        total_write_iops: 1200,
        total_read_bandwidth_mbps: 90.4,
        total_write_bandwidth_mbps: 60.1,
        avg_read_latency_ms: 5.2,
        avg_write_latency_ms: 7.8,
        spdk_active: true,
        last_updated: new Date().toISOString(),
        failed_raids: 1,
        degraded_raids: 2,
        healthy_raids: 4,
        performance_score: 68.7,
      },
      {
        node_id: 'worker-node-4',
        raid_count: 0,
        volume_count: 0,
        total_read_iops: 0,
        total_write_iops: 0,
        total_read_bandwidth_mbps: 0,
        total_write_bandwidth_mbps: 0,
        avg_read_latency_ms: 0,
        avg_write_latency_ms: 0,
        spdk_active: false,
        last_updated: new Date().toISOString(),
        failed_raids: 0,
        degraded_raids: 0,
        healthy_raids: 0,
        performance_score: 0,
      },
    ],
    cluster_totals: {
      total_read_iops: 8500,
      total_write_iops: 6100,
      total_bandwidth_mbps: 641.3,
      avg_cluster_latency_ms: 3.1,
      total_active_nodes: 3,
      total_raids: 15,
    },
    last_updated: new Date().toISOString(),
  };
}

export default useNodePerformance;
