import React, { useState, useEffect } from 'react';
import { AlertTriangle, Clock, Zap, Server, ArrowRight, CheckCircle, XCircle } from 'lucide-react';

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
}

interface MigrationRequest {
  target_node?: string;
  confirmation: boolean;
}

export const NodeAlertsPanel: React.FC<NodeAlertsPanelProps> = ({ nodeId }) => {
  const [alertsData, setAlertsData] = useState<NodeAlertsData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [migrating, setMigrating] = useState<Set<string>>(new Set());

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

  const triggerMigration = async (volumeId: string, targetNode?: string) => {
    if (!confirm(`Are you sure you want to migrate RAID for volume ${volumeId}? This operation cannot be easily undone.`)) {
      return;
    }

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
      
      // Refresh alerts
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

                      {alert.manual_migration_available && (
                        <div className="space-y-3">
                          <div className="flex items-center space-x-3">
                            <button
                              onClick={() => triggerMigration(alert.volume_id)}
                              disabled={migrating.has(alert.volume_id)}
                              className={`px-4 py-2 text-sm font-medium rounded-lg flex items-center space-x-2 ${
                                alert.severity === 'critical' 
                                  ? 'bg-red-600 hover:bg-red-700 text-white'
                                  : 'bg-yellow-600 hover:bg-yellow-700 text-white'
                              } disabled:opacity-50 disabled:cursor-not-allowed`}
                            >
                              {migrating.has(alert.volume_id) ? (
                                <>
                                  <div className="animate-spin rounded-full h-4 w-4 border-b-2 border-white"></div>
                                  <span>Starting Migration...</span>
                                </>
                              ) : (
                                <>
                                  <Zap className="w-4 h-4" />
                                  <span>Migrate RAID</span>
                                  <ArrowRight className="w-4 h-4" />
                                </>
                              )}
                            </button>
                            
                            <div className="text-xs text-gray-500">
                              <Clock className="w-3 h-3 inline mr-1" />
                              Intelligent target selection
                            </div>
                          </div>

                          {/* Smart Target Selection Info */}
                          <div className="bg-blue-50 border border-blue-200 rounded-lg p-3">
                            <h5 className="text-sm font-medium text-blue-900 mb-2">🎯 Intelligent Target Selection</h5>
                            <div className="text-xs text-blue-800 space-y-1">
                              <div className="flex items-center">
                                <span className="font-medium mr-2">1.</span>
                                <span>Prefers nodes with existing healthy replicas (zero data migration)</span>
                              </div>
                              <div className="flex items-center">
                                <span className="font-medium mr-2">2.</span>
                                <span>Chooses node with minimum RAID count (load balancing)</span>
                              </div>
                              <div className="flex items-center">
                                <span className="font-medium mr-2">3.</span>
                                <span>Verifies node health and schedulability</span>
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
    </div>
  );
};
