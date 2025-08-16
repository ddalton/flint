import { useState, useEffect, useCallback } from 'react';
import type { MigrationOperation } from '../components/ui/MigrationProgressMonitor';
import type { MigrationAlert } from '../components/ui/MigrationAlertSystem';
import { MigrationAlerts } from '../components/ui/MigrationAlertSystem';

interface MigrationMonitoringResponse {
  operations: MigrationOperation[];
  alerts: MigrationAlert[];
  cleanup_queue: {
    raid_name: string;
    old_members: string[];
    cleanup_status: string;
  }[];
}

interface UseMigrationMonitoringReturn {
  operations: MigrationOperation[];
  alerts: MigrationAlert[];
  cleanupQueue: any[];
  loading: boolean;
  error: string | null;
  
  // Actions
  retryOperation: (operationId: string) => Promise<void>;
  cancelOperation: (operationId: string) => Promise<void>;
  dismissAlert: (alertId: string) => void;
  refreshData: () => Promise<void>;
  
  // Real-time updates
  isConnected: boolean;
  lastUpdate: Date | null;
}

export const useMigrationMonitoring = (
  nodeId?: string,
  autoRefresh: boolean = true,
  refreshInterval: number = 5000
): UseMigrationMonitoringReturn => {
  const [operations, setOperations] = useState<MigrationOperation[]>([]);
  const [alerts, setAlerts] = useState<MigrationAlert[]>([]);
  const [cleanupQueue, setCleanupQueue] = useState<any[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [isConnected, setIsConnected] = useState(false);
  const [lastUpdate, setLastUpdate] = useState<Date | null>(null);

  // Fetch migration data
  const fetchMigrationData = useCallback(async () => {
    setLoading(true);
    setError(null);
    
    try {
      const params = new URLSearchParams();
      if (nodeId) params.append('node_id', nodeId);
      
      const response = await fetch(`/api/migration/monitor?${params}`);
      if (!response.ok) {
        throw new Error(`Failed to fetch migration data: ${response.status}`);
      }

      const data: MigrationMonitoringResponse = await response.json();
      
      // Check for newly completed operations and create success alerts
      data.operations.forEach(operation => {
        const existingOp = operations.find(op => op.id === operation.id);
        if (existingOp && existingOp.status !== 'completed' && operation.status === 'completed') {
          const successAlert = MigrationAlerts.migrationCompleted(
            operation.operation_type,
            operation.raid_name,
            getTargetDescription(operation.target_info),
            operation.cleanup_status?.old_member_removed || false
          );
          setAlerts(prev => [successAlert, ...prev]);
        }
        
        // Check for failures
        if (existingOp && existingOp.status !== 'failed' && operation.status === 'failed') {
          const failureAlert = MigrationAlerts.migrationFailed(
            operation.operation_type,
            operation.raid_name,
            operation.error_message || 'Unknown error',
            () => retryOperation(operation.id)
          );
          setAlerts(prev => [failureAlert, ...prev]);
        }
      });
      
      setOperations(data.operations);
      setCleanupQueue(data.cleanup_queue || []);
      setIsConnected(true);
      setLastUpdate(new Date());
      
    } catch (err) {
      const errorMessage = err instanceof Error ? err.message : 'Failed to fetch migration data';
      setError(errorMessage);
      setIsConnected(false);
      console.error('Error fetching migration data:', err);
    } finally {
      setLoading(false);
    }
  }, [nodeId, operations]);

  // Retry operation
  const retryOperation = useCallback(async (operationId: string) => {
    try {
      const response = await fetch(`/api/migration/operations/${operationId}/retry`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
      });

      if (!response.ok) {
        throw new Error(`Failed to retry operation: ${response.status}`);
      }

      // Create info alert for retry
      const operation = operations.find(op => op.id === operationId);
      if (operation) {
        const retryAlert = MigrationAlerts.migrationStarted(
          operation.operation_type,
          operation.raid_name,
          getTargetDescription(operation.target_info)
        );
        setAlerts(prev => [retryAlert, ...prev]);
      }

      // Refresh data
      await fetchMigrationData();
      
    } catch (err) {
      console.error('Error retrying operation:', err);
      // Could add error alert here
    }
  }, [operations, fetchMigrationData]);

  // Cancel operation
  const cancelOperation = useCallback(async (operationId: string) => {
    try {
      const response = await fetch(`/api/migration/operations/${operationId}/cancel`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
      });

      if (!response.ok) {
        throw new Error(`Failed to cancel operation: ${response.status}`);
      }

      // Refresh data
      await fetchMigrationData();
      
    } catch (err) {
      console.error('Error canceling operation:', err);
    }
  }, [fetchMigrationData]);

  // Dismiss alert
  const dismissAlert = useCallback((alertId: string) => {
    setAlerts(prev => prev.filter(alert => alert.id !== alertId));
  }, []);

  // Auto-refresh setup
  useEffect(() => {
    if (autoRefresh) {
      fetchMigrationData();
      
      const interval = setInterval(() => {
        fetchMigrationData();
      }, refreshInterval);

      return () => clearInterval(interval);
    }
  }, [autoRefresh, refreshInterval, fetchMigrationData]);

  // WebSocket connection for real-time updates (optional enhancement)
  useEffect(() => {
    if (typeof window !== 'undefined' && window.WebSocket) {
      const ws = new WebSocket(`ws://${window.location.host}/api/migration/websocket`);
      
      ws.onopen = () => {
        setIsConnected(true);
        console.log('Migration monitoring WebSocket connected');
      };
      
      ws.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data);
          
          if (data.type === 'operation_update') {
            setOperations(prev => 
              prev.map(op => op.id === data.operation.id ? data.operation : op)
            );
          } else if (data.type === 'cleanup_completed') {
            const cleanupAlert = MigrationAlerts.cleanupCompleted(
              data.raid_name,
              data.removed_member
            );
            setAlerts(prev => [cleanupAlert, ...prev]);
          }
          
          setLastUpdate(new Date());
        } catch (err) {
          console.error('Error parsing WebSocket message:', err);
        }
      };
      
      ws.onclose = () => {
        setIsConnected(false);
        console.log('Migration monitoring WebSocket disconnected');
      };
      
      ws.onerror = (error) => {
        console.error('WebSocket error:', error);
        setIsConnected(false);
      };

      return () => {
        ws.close();
      };
    }
  }, []);

  return {
    operations,
    alerts,
    cleanupQueue,
    loading,
    error,
    retryOperation,
    cancelOperation,
    dismissAlert,
    refreshData: fetchMigrationData,
    isConnected,
    lastUpdate
  };
};

// Helper function to describe migration targets
const getTargetDescription = (targetInfo: MigrationOperation['target_info']): string => {
  switch (targetInfo.type) {
    case 'node':
      return targetInfo.target_node || 'Unknown node';
    case 'local_disk':
      return `${targetInfo.target_disk_id} (${targetInfo.target_node})`;
    case 'internal_nvmeof':
      return `Internal NVMe-oF: ${targetInfo.target_nvmeof_nqn}`;
    case 'external_nvmeof':
      return `External NVMe-oF: ${targetInfo.target_nvmeof_nqn}`;
    default:
      return 'Unknown target';
  }
};

// Mock data for development
export const getMockMigrationData = (): MigrationMonitoringResponse => ({
  operations: [
    {
      id: 'migration-001',
      operation_type: 'member_migration',
      raid_name: 'raid1_node1',
      volume_id: 'vol-12345',
      source_node: 'worker-node-1',
      target_info: {
        type: 'local_disk',
        target_disk_id: 'nvme2n1',
        target_node: 'worker-node-2'
      },
      status: 'executing',
      progress_percent: 75.5,
      stage: 'Data synchronization',
      started_at: new Date(Date.now() - 10 * 60 * 1000).toISOString(), // 10 minutes ago
      estimated_completion: '5 minutes',
      throughput_mbps: 850,
      data_copied_gb: 150.5,
      total_data_gb: 200,
      cleanup_status: {
        old_member_removed: false,
        data_verified: true,
        metadata_updated: false,
        rebuild_completed: false
      }
    },
    {
      id: 'migration-002',
      operation_type: 'node_migration',
      raid_name: 'raid1_node3',
      source_node: 'worker-node-3',
      target_info: {
        type: 'node',
        target_node: 'worker-node-4'
      },
      status: 'cleanup',
      progress_percent: 95.0,
      stage: 'Removing old members',
      started_at: new Date(Date.now() - 25 * 60 * 1000).toISOString(), // 25 minutes ago
      cleanup_status: {
        old_member_removed: false,
        data_verified: true,
        metadata_updated: true,
        rebuild_completed: true
      }
    }
  ],
  alerts: [],
  cleanup_queue: [
    {
      raid_name: 'raid1_node2',
      old_members: ['nvme1n1-old', 'nvme2n1-old'],
      cleanup_status: 'pending'
    }
  ]
});


