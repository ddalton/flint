import { useState, useEffect } from 'react';

export interface NodeStatusData {
  status: string;
  count: number;
  percentage: number;
  color: string;
}

export interface ClusterHealth {
  status: string;
  total_nodes: number;
  healthy_nodes: number;
  degraded_nodes: number;
  failed_nodes: number;
  node_status_chart: NodeStatusData[];
}

export interface NodeStats {
  total_raids: number;
  healthy_raids: number;
  degraded_raids: number;
  total_volumes: number;
  active_volumes: number;
  failed_volumes: number;
}

export interface AlertSummary {
  total_alerts: number;
  critical_alerts: number;
  warning_alerts: number;
  nodes_with_alerts: number;
}

export interface RecentEvent {
  timestamp: string;
  event_type: string;
  message: string;
  node_id?: string;
  volume_id?: string;
}

export interface DashboardOverview {
  cluster_health: ClusterHealth;
  node_stats: NodeStats;
  alert_summary: AlertSummary;
  recent_events: RecentEvent[];
}

interface UseDashboardOverviewResult {
  overview: DashboardOverview | null;
  loading: boolean;
  error: string | null;
  refresh: () => void;
}

export const useDashboardOverview = (autoRefresh: boolean = false): UseDashboardOverviewResult => {
  const [overview, setOverview] = useState<DashboardOverview | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const fetchOverview = async () => {
    try {
      setLoading(true);
      const response = await fetch('/api/dashboard/overview');
      
      if (!response.ok) {
        throw new Error(`Failed to fetch dashboard overview: ${response.status}`);
      }
      
      const data = await response.json();
      setOverview(data);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch overview');
      console.error('Error fetching dashboard overview:', err);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    fetchOverview();

    if (autoRefresh) {
      const interval = setInterval(fetchOverview, 30000); // Refresh every 30 seconds
      return () => clearInterval(interval);
    }
  }, [autoRefresh]);

  return {
    overview,
    loading,
    error,
    refresh: fetchOverview,
  };
};
