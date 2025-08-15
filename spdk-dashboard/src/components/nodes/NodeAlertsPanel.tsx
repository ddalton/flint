import React, { useState, useEffect } from 'react';
import { AlertTriangle, Clock, Zap, Server, ArrowRight, CheckCircle, XCircle, RefreshCw, Plus } from 'lucide-react';
import { NodeTargetSelectionDialog } from '../ui/NodeTargetSelectionDialog';
import { EnhancedRaidMigrationDialog } from '../ui/EnhancedRaidMigrationDialog';
import { useMigrationData } from '../../hooks/useMigrationData';
import type { DetailedRaidInfo } from '../../hooks/useMigrationData';

export interface NodeAlert {
  id: string;
  alert_type: string;
  severity: string;
  message: string;
  volume_id: string;
  raid_name: string;
  source_node: string;
  created_at: string;
  suggested_action: string;
  manual_migration_available: boolean;
  has_external_nvmeof_members?: boolean; // Only set for network partition alerts
  inaccessible_local_members?: number;    // Count of inaccessible local members
  inaccessible_external_members?: number; // Count of inaccessible external members
}

export interface NodeAlertsData {
  node_id: string;
  node_status: string;
  alerts: NodeAlert[];
  total_alerts: number;
  raid_count: number;
  volume_count: number;
}

interface NodeAlertsPanelProps {
  nodeId: string;
  availableNodes: string[];
}

interface MigrationRequest {
  target_node?: string;
  confirmation: boolean;
}

export const NodeAlertsPanel: React.FC<NodeAlertsPanelProps> = ({ nodeId, availableNodes }) => {
  const [alertsData, setAlertsData] = useState<NodeAlertsData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [migrating, setMigrating] = useState<Set<string>>(new Set());
  const [showMigrationDialog, setShowMigrationDialog] = useState(false);
  const [showEnhancedMigrationDialog, setShowEnhancedMigrationDialog] = useState(false);
  const [selectedVolumeForMigration, setSelectedVolumeForMigration] = useState<string | null>(null);
  const [selectedRaidForMigration, setSelectedRaidForMigration] = useState<string | null>(null);
  const [migrationType, setMigrationType] = useState<'node_migration' | 'member_migration' | 'member_addition'>('node_migration');
  
  // Fetch migration data when needed
  const { 
    availableDisks, 
    availableNvmeofTargets, 
    raidInfo, 
    loading: migrationDataLoading,
    refreshData: refreshMigrationData 
  } = useMigrationData(
    selectedVolumeForMigration || undefined,
    selectedRaidForMigration || undefined,
    false
  );

  const fetchNodeAlerts = async () => {
    try {
      setLoading(true);
      const response = await fetch(`/api/nodes/${nodeId}/alerts`);
      if (!response.ok) {
        throw new Error(`Failed to fetch alerts: ${response.status}`);
      }
      const data = await response.json();
      setAlertsData(data);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to fetch alerts');
      console.error('Error fetching node alerts:', err);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    fetchNodeAlerts();
    // Refresh every 30 seconds
    const interval = setInterval(fetchNodeAlerts, 30000);
    return () => clearInterval(interval);
  }, [nodeId]);

  const handleMigrateClick = (volumeId: string, operationType: 'node_migration' | 'member_migration' | 'member_addition' = 'node_migration') => {
    setSelectedVolumeForMigration(volumeId);
    
    // Extract RAID name from alerts data for this volume
    const alert = alertsData?.alerts.find(a => a.volume_id === volumeId);
    if (alert?.raid_name) {
      setSelectedRaidForMigration(alert.raid_name);
    }
    
    setMigrationType(operationType);
    setShowEnhancedMigrationDialog(true);
  };

  const handleLegacyMigrateClick = (volumeId: string) => {
    setSelectedVolumeForMigration(volumeId);
    setShowMigrationDialog(true);
  };

  const handleMigrationConfirm = async (targetNode?: string) => {
    if (!selectedVolumeForMigration) return;

    const volumeId = selectedVolumeForMigration;
    setMigrating(prev => new Set(prev).add(volumeId));

    try {
      const request: MigrationRequest = {
        confirmation: true,
      };
      
      if (targetNode) {
        request.target_node = targetNode;
      }

      const response = await fetch(`/api/alerts/${volumeId}/migrate`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify(request),
      });

      if (!response.ok) {
        throw new Error(`Migration failed: ${response.status}`);
      }

      const result = await response.json();
      console.log('Migration started:', result);
      
      // Show detailed success notification with selection reasoning
      const message = `Migration initiated for volume ${volumeId}!\n\n` +
                     `Migration ID: ${result.migration_id}\n` +
                     `Source: ${result.source_node} → Target: ${result.target_node}\n\n` +
                     `Selection Reason: ${result.selection_reason || 'Auto-selected based on availability'}`;
      
      alert(message);
      
      // Close dialog and refresh alerts
      setShowMigrationDialog(false);
      setSelectedVolumeForMigration(null);
      fetchNodeAlerts();
      
    } catch (err) {
      console.error('Error starting migration:', err);
      alert(`Migration failed: ${err instanceof Error ? err.message : 'Unknown error'}`);
    } finally {
      setMigrating(prev => {
        const newSet = new Set(prev);
        newSet.delete(volumeId);
        return newSet;
      });
    }
  };

  const handleEnhancedMigrationConfirm = async (operation: any) => {
    if (!selectedVolumeForMigration) return;

    const volumeId = selectedVolumeForMigration;
    setMigrating(prev => new Set(prev).add(volumeId));

    try {
      const response = await fetch(`/api/alerts/${volumeId}/enhanced-migrate`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          ...operation,
          confirmation: true
        }),
      });

      if (!response.ok) {
        throw new Error(`Enhanced migration failed: ${response.status}`);
      }

      const result = await response.json();
      console.log('Enhanced migration started:', result);
      
      // Show detailed success notification
      const operationName = operation.type === 'node_migration' ? 'Node Migration' :
                           operation.type === 'member_migration' ? 'RAID Member Migration' :
                           operation.type === 'member_addition' ? 'RAID Member Addition' :
                           'Migration';
      
      const targetDescription = operation.target_disk_id 
        ? `Target Disk: ${operation.target_disk_id}`
        : operation.target_nvmeof_nqn
        ? `Target NVMe-oF: ${operation.target_nvmeof_nqn}`
        : `Target Node: ${operation.target_node}`;
      
      const message = `${operationName} initiated for volume ${volumeId}!\n\n` +
                     `Operation ID: ${result.operation_id}\n` +
                     `${targetDescription}\n\n` +
                     `Status: ${result.status}`;
      
      alert(message);
      
      // Close dialog and refresh alerts
      setShowEnhancedMigrationDialog(false);
      setSelectedVolumeForMigration(null);
      setSelectedRaidForMigration(null);
      fetchNodeAlerts();
      
    } catch (err) {
      console.error('Error starting enhanced migration:', err);
      alert(`Enhanced migration failed: ${err instanceof Error ? err.message : 'Unknown error'}`);
    } finally {
      setMigrating(prev => {
        const newSet = new Set(prev);
        newSet.delete(volumeId);
        return newSet;
      });
    }
  };

  const getAlertIcon = (severity: string) => {
    switch (severity) {
      case 'critical':
        return <XCircle className="w-5 h-5 text-red-600" />;
      case 'warning':
        return <AlertTriangle className="w-5 h-5 text-yellow-600" />;
      default:
        return <AlertTriangle className="w-5 h-5 text-gray-600" />;
    }
  };

  const getAlertBgColor = (severity: string) => {
    switch (severity) {
      case 'critical':
        return 'bg-red-50 border-red-200';
      case 'warning':
        return 'bg-yellow-50 border-yellow-200';
      default:
        return 'bg-gray-50 border-gray-200';
    }
  };

  const getAlertTextColor = (severity: string) => {
    switch (severity) {
      case 'critical':
        return 'text-red-800';
      case 'warning':
        return 'text-yellow-800';
      default:
        return 'text-gray-800';
    }
  };

  const getNodeStatusIcon = (status: string) => {
    switch (status) {
      case 'healthy':
        return <CheckCircle className="w-5 h-5 text-green-600" />;
      case 'warning':
        return <AlertTriangle className="w-5 h-5 text-yellow-600" />;
      case 'critical':
        return <XCircle className="w-5 h-5 text-red-600" />;
      case 'idle':
        return <Server className="w-5 h-5 text-gray-600" />;
      default:
        return <Server className="w-5 h-5 text-gray-600" />;
    }
  };

  const formatTimeAgo = (timestamp: string) => {
    const date = new Date(timestamp);
    const now = new Date();
    const diffMinutes = Math.floor((now.getTime() - date.getTime()) / (1000 * 60));
    
    if (diffMinutes < 1) return 'Just now';
    if (diffMinutes < 60) return `${diffMinutes}m ago`;
    const diffHours = Math.floor(diffMinutes / 60);
    if (diffHours < 24) return `${diffHours}h ago`;
    const diffDays = Math.floor(diffHours / 24);
    return `${diffDays}d ago`;
  };

  if (loading) {
    return (
      <div className="bg-white rounded-lg p-6 border border-gray-200">
        <div className="flex items-center justify-center">
          <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
          <span className="ml-2 text-gray-600">Loading alerts...</span>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="bg-red-50 border border-red-200 rounded-lg p-6">
        <div className="flex items-center">
          <XCircle className="w-5 h-5 text-red-600 mr-2" />
          <span className="text-red-800">Failed to load alerts: {error}</span>
        </div>
        <button
          onClick={fetchNodeAlerts}
          className="mt-2 px-3 py-1 text-sm bg-red-100 text-red-700 rounded hover:bg-red-200"
        >
          Retry
        </button>
      </div>
    );
  }

  if (!alertsData) {
    return null;
  }

  return (
    <div className="bg-white rounded-lg border border-gray-200">
      {/* Header */}
      <div className="px-6 py-4 bg-gray-50 border-b border-gray-200">
        <div className="flex items-center justify-between">
          <div className="flex items-center space-x-3">
            {getNodeStatusIcon(alertsData.node_status)}
            <div>
              <h3 className="text-lg font-semibold">Node Alerts & Status</h3>
              <p className="text-sm text-gray-600">
                {alertsData.raid_count} RAIDs • {alertsData.volume_count} Volumes • {alertsData.total_alerts} Alerts
              </p>
            </div>
          </div>
          <div className="flex items-center space-x-2">
            <span className={`px-3 py-1 rounded-full text-sm font-medium ${
              alertsData.node_status === 'healthy' ? 'bg-green-100 text-green-800' :
              alertsData.node_status === 'warning' ? 'bg-yellow-100 text-yellow-800' :
              alertsData.node_status === 'critical' ? 'bg-red-100 text-red-800' :
              'bg-gray-100 text-gray-800'
            }`}>
              {alertsData.node_status.charAt(0).toUpperCase() + alertsData.node_status.slice(1)}
            </span>
            <button
              onClick={fetchNodeAlerts}
              className="px-3 py-1 text-sm bg-blue-100 text-blue-700 rounded hover:bg-blue-200"
            >
              Refresh
            </button>
          </div>
        </div>
      </div>

      {/* Alerts Content */}
      <div className="p-6">
        {alertsData.alerts.length === 0 ? (
          <div className="text-center py-8">
            <CheckCircle className="w-12 h-12 text-green-500 mx-auto mb-3" />
            <h4 className="text-lg font-medium text-gray-900 mb-2">No Active Alerts</h4>
            <p className="text-gray-600">
              All RAIDs and volumes on this node are operating normally.
            </p>
          </div>
        ) : (
          <div className="space-y-4">
            {alertsData.alerts.map((alert) => (
              <div
                key={alert.id}
                className={`p-4 rounded-lg border-2 ${getAlertBgColor(alert.severity)}`}
              >
                <div className="flex items-start justify-between">
                  <div className="flex items-start space-x-3 flex-1">
                    {getAlertIcon(alert.severity)}
                    <div className="flex-1">
                      <div className="flex items-center space-x-2 mb-2">
                        <h4 className={`font-semibold ${getAlertTextColor(alert.severity)}`}>
                          {alert.alert_type === 'raid_host_critical_failure' ? 'Critical RAID Host Failure' :
                           alert.alert_type === 'raid_host_failure' ? 'RAID Host Failure' :
                           'Storage Alert'}
                        </h4>
                        <span className={`px-2 py-1 text-xs rounded-full font-medium ${
                          alert.severity === 'critical' ? 'bg-red-100 text-red-700' :
                          'bg-yellow-100 text-yellow-700'
                        }`}>
                          {alert.severity.toUpperCase()}
                        </span>
                      </div>

                      <p className={`text-sm mb-3 ${getAlertTextColor(alert.severity)}`}>
                        {alert.message}
                      </p>

                      <div className="grid grid-cols-2 gap-4 text-xs text-gray-600 mb-3">
                        <div>
                          <span className="font-medium">Volume:</span> {alert.volume_id}
                        </div>
                        <div>
                          <span className="font-medium">RAID:</span> {alert.raid_name}
                        </div>
                        <div>
                          <span className="font-medium">Created:</span> {formatTimeAgo(alert.created_at)}
                        </div>
                        <div>
                          <span className="font-medium">Action:</span> {alert.suggested_action.replace(/_/g, ' ')}
                        </div>
                      </div>

                      {alert.manual_migration_available ? (
                        <div className="space-y-3">
                          <div className="flex items-center space-x-3">
                            <div className="flex space-x-2">
                              <button
                                onClick={() => handleMigrateClick(alert.volume_id, 'node_migration')}
                                disabled={migrating.has(alert.volume_id)}
                                className={`px-3 py-2 text-sm font-medium rounded-lg flex items-center space-x-2 ${
                                  alert.severity === 'critical' 
                                    ? 'bg-red-600 hover:bg-red-700 text-white'
                                    : 'bg-yellow-600 hover:bg-yellow-700 text-white'
                                } disabled:opacity-50 disabled:cursor-not-allowed`}
                                title="Migrate entire RAID volume to another node"
                              >
                                {migrating.has(alert.volume_id) ? (
                                  <>
                                    <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                                    <span>Migrating...</span>
                                  </>
                                ) : (
                                  <>
                                    <Server className="w-4 h-4" />
                                    <span>Migrate Volume</span>
                                  </>
                                )}
                              </button>
                              
                              <button
                                onClick={() => handleMigrateClick(alert.volume_id, 'member_migration')}
                                disabled={migrating.has(alert.volume_id)}
                                className="px-3 py-2 text-sm font-medium rounded-lg flex items-center space-x-2 bg-blue-600 hover:bg-blue-700 text-white disabled:opacity-50 disabled:cursor-not-allowed"
                                title="Replace individual RAID members with new disks or NVMe-oF targets"
                              >
                                <RefreshCw className="w-4 h-4" />
                                <span>Replace Member</span>
                              </button>
                              
                              <button
                                onClick={() => handleMigrateClick(alert.volume_id, 'member_addition')}
                                disabled={migrating.has(alert.volume_id)}
                                className="px-3 py-2 text-sm font-medium rounded-lg flex items-center space-x-2 bg-green-600 hover:bg-green-700 text-white disabled:opacity-50 disabled:cursor-not-allowed"
                                title="Add new RAID members to increase capacity or redundancy"
                              >
                                <Plus className="w-4 h-4" />
                                <span>Add Members</span>
                              </button>
                            </div>
                            
                            <div className="text-xs text-gray-500">
                              <Clock className="w-3 h-3 inline mr-1" />
                              Advanced RAID operations
                            </div>
                          </div>

                          {/* Migration Options Info */}
                          <div className="bg-blue-50 border border-blue-200 rounded-lg p-3">
                            <h5 className="text-sm font-medium text-blue-900 mb-2">🔧 RAID Migration Options</h5>
                            <div className="text-xs text-blue-800 space-y-2">
                              <div className="flex items-start gap-2">
                                <Server className="w-3 h-3 mt-0.5 text-blue-600" />
                                <div>
                                  <span className="font-medium">Volume Migration:</span> Move entire RAID volume to another node with automatic target selection
                                </div>
                              </div>
                              <div className="flex items-start gap-2">
                                <RefreshCw className="w-3 h-3 mt-0.5 text-blue-600" />
                                <div>
                                  <span className="font-medium">Member Replacement:</span> Replace individual RAID members with local disks or NVMe-oF targets
                                </div>
                              </div>
                              <div className="flex items-start gap-2">
                                <Plus className="w-3 h-3 mt-0.5 text-blue-600" />
                                <div>
                                  <span className="font-medium">Member Addition:</span> Add new members to increase capacity or redundancy
                                </div>
                              </div>
                              <div className="mt-2 pt-2 border-t border-blue-200">
                                <div className="flex items-center">
                                  <span className="mr-2">•</span>
                                  <span>Uses SPDK JSON-RPC methods for safe, efficient operations</span>
                                </div>
                                <div className="flex items-center">
                                  <span className="mr-2">•</span>
                                  <span>Supports local NVMe disks, internal and external NVMe-oF targets</span>
                                </div>
                              </div>
                            </div>
                          </div>
                        </div>
                      ) : (
                        // Show network partition message when migration is not available
                        <div className="space-y-3">
                          <div className="bg-red-50 border border-red-200 rounded-lg p-4">
                            <div className="flex items-start space-x-3">
                              <div className="flex-shrink-0">
                                <div className="w-8 h-8 bg-red-100 rounded-full flex items-center justify-center">
                                  <svg className="w-5 h-5 text-red-600" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                    <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-2.5L13.732 4c-.77-.833-1.964-.833-2.732 0L3.732 16.5c-.77.833.192 2.5 1.732 2.5z" />
                                  </svg>
                                </div>
                              </div>
                              <div className="flex-1 min-w-0">
                                <h4 className="text-sm font-medium text-red-800 mb-2">🚨 Network Partition Detected</h4>
                                <p className="text-sm text-red-700 mb-3">
                                  All RAID members are currently inaccessible. Migration is not possible until at least one member becomes accessible.
                                </p>
                                <div className="bg-red-100 rounded-md p-3">
                                  <h5 className="text-xs font-medium text-red-800 mb-2">Recovery Options:</h5>
                                  <ul className="text-xs text-red-700 space-y-1">
                                    {alert.inaccessible_local_members && alert.inaccessible_local_members > 0 && (
                                      <li className="flex items-center">
                                        <span className="w-1.5 h-1.5 bg-red-400 rounded-full mr-2 flex-shrink-0"></span>
                                        Restore connectivity to {alert.inaccessible_local_members} cluster node{alert.inaccessible_local_members > 1 ? 's' : ''} hosting local RAID members
                                      </li>
                                    )}
                                    {alert.inaccessible_external_members && alert.inaccessible_external_members > 0 && (
                                      <li className="flex items-center">
                                        <span className="w-1.5 h-1.5 bg-red-400 rounded-full mr-2 flex-shrink-0"></span>
                                        Fix {alert.inaccessible_external_members} external NVMe-oF endpoint{alert.inaccessible_external_members > 1 ? 's' : ''} (network connectivity or storage system issues)
                                      </li>
                                    )}
                                    <li className="flex items-center">
                                      <span className="w-1.5 h-1.5 bg-red-400 rounded-full mr-2 flex-shrink-0"></span>
                                      Restore data from backup if available
                                    </li>
                                  </ul>
                                </div>
                              </div>
                            </div>
                          </div>
                        </div>
                      )}
                    </div>
                  </div>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Legacy RAID Migration Target Selection Dialog */}
      <NodeTargetSelectionDialog
        isOpen={showMigrationDialog}
        onClose={() => {
          setShowMigrationDialog(false);
          setSelectedVolumeForMigration(null);
        }}
        onConfirm={handleMigrationConfirm}
        title="Migrate RAID Volume"
        description={
          selectedVolumeForMigration
            ? `Migrate RAID volume ${selectedVolumeForMigration} from ${nodeId} to another node. This operation helps resolve storage alerts and maintain data availability.`
            : 'Migrate RAID volume to another node.'
        }
        confirmText="Start Migration"
        availableNodes={availableNodes}
        currentNode={nodeId}
        infoMessage="The system will intelligently select the best target node based on available capacity, performance, and current workload unless you manually specify a target."
        isLoading={selectedVolumeForMigration ? migrating.has(selectedVolumeForMigration) : false}
      />

      {/* Enhanced RAID Migration Dialog */}
      <EnhancedRaidMigrationDialog
        isOpen={showEnhancedMigrationDialog}
        onClose={() => {
          setShowEnhancedMigrationDialog(false);
          setSelectedVolumeForMigration(null);
          setSelectedRaidForMigration(null);
        }}
        onConfirm={handleEnhancedMigrationConfirm}
        migrationType={migrationType}
        volumeId={selectedVolumeForMigration || undefined}
        raidInfo={raidInfo || undefined}
        currentNode={nodeId}
        availableNodes={availableNodes}
        availableDisks={availableDisks}
        availableNvmeofTargets={availableNvmeofTargets}
        isLoading={migrationDataLoading || (selectedVolumeForMigration ? migrating.has(selectedVolumeForMigration) : false)}
      />
    </div>
  );
};
