import React, { useState, useEffect } from 'react';
import { CheckCircle, XCircle, AlertTriangle, Clock, Zap, RefreshCw, Trash2, Server, HardDrive } from 'lucide-react';

export interface MigrationOperation {
  id: string;
  operation_type: 'node_migration' | 'member_migration' | 'member_addition';
  volume_id?: string;
  raid_name: string;
  source_node: string;
  target_info: {
    type: 'node' | 'local_disk' | 'internal_nvmeof' | 'external_nvmeof';
    target_node?: string;
    target_disk_id?: string;
    target_nvmeof_nqn?: string;
  };
  status: 'pending' | 'executing' | 'cleanup' | 'completed' | 'failed';
  progress_percent: number;
  stage: string;
  started_at: string;
  estimated_completion?: string;
  error_message?: string;
  
  // Cleanup tracking
  cleanup_status?: {
    old_member_removed: boolean;
    data_verified: boolean;
    metadata_updated: boolean;
    rebuild_completed: boolean;
  };
  
  // Performance metrics
  throughput_mbps?: number;
  data_copied_gb?: number;
  total_data_gb?: number;
}

interface MigrationProgressMonitorProps {
  operations: MigrationOperation[];
  onRetry?: (operationId: string) => void;
  onCancel?: (operationId: string) => void;
  refreshInterval?: number;
}

export const MigrationProgressMonitor: React.FC<MigrationProgressMonitorProps> = ({
  operations,
  onRetry,
  onCancel,
  refreshInterval = 5000
}) => {
  const [expandedOperations, setExpandedOperations] = useState<Set<string>>(new Set());
  const [lastUpdate, setLastUpdate] = useState(new Date());

  // Auto-refresh progress data
  useEffect(() => {
    const interval = setInterval(() => {
      setLastUpdate(new Date());
    }, refreshInterval);

    return () => clearInterval(interval);
  }, [refreshInterval]);

  const toggleExpanded = (operationId: string) => {
    const newExpanded = new Set(expandedOperations);
    if (newExpanded.has(operationId)) {
      newExpanded.delete(operationId);
    } else {
      newExpanded.add(operationId);
    }
    setExpandedOperations(newExpanded);
  };

  const getStatusIcon = (status: string) => {
    switch (status) {
      case 'completed':
        return <CheckCircle className="w-5 h-5 text-green-600" />;
      case 'failed':
        return <XCircle className="w-5 h-5 text-red-600" />;
      case 'executing':
        return <RefreshCw className="w-5 h-5 text-blue-600 animate-spin" />;
      case 'cleanup':
        return <Trash2 className="w-5 h-5 text-orange-600 animate-pulse" />;
      default:
        return <Clock className="w-5 h-5 text-gray-600" />;
    }
  };

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'completed':
        return 'bg-green-100 text-green-800 border-green-200';
      case 'failed':
        return 'bg-red-100 text-red-800 border-red-200';
      case 'executing':
        return 'bg-blue-100 text-blue-800 border-blue-200';
      case 'cleanup':
        return 'bg-orange-100 text-orange-800 border-orange-200';
      default:
        return 'bg-gray-100 text-gray-800 border-gray-200';
    }
  };

  const getOperationTypeIcon = (type: string) => {
    switch (type) {
      case 'node_migration':
        return <Server className="w-4 h-4" />;
      case 'member_migration':
        return <RefreshCw className="w-4 h-4" />;
      case 'member_addition':
        return <HardDrive className="w-4 h-4" />;
      default:
        return <Zap className="w-4 h-4" />;
    }
  };

  const formatDuration = (startTime: string) => {
    const start = new Date(startTime);
    const now = new Date();
    const diffMs = now.getTime() - start.getTime();
    const diffMins = Math.floor(diffMs / 60000);
    const diffHours = Math.floor(diffMins / 60);
    
    if (diffHours > 0) {
      return `${diffHours}h ${diffMins % 60}m`;
    }
    return `${diffMins}m`;
  };

  const renderCleanupStatus = (cleanup: MigrationOperation['cleanup_status']) => {
    if (!cleanup) return null;

    const checks = [
      { key: 'rebuild_completed', label: 'Data Rebuild Complete', icon: RefreshCw },
      { key: 'data_verified', label: 'Data Integrity Verified', icon: CheckCircle },
      { key: 'old_member_removed', label: 'Old Member Removed', icon: Trash2 },
      { key: 'metadata_updated', label: 'Metadata Updated', icon: Server }
    ];

    return (
      <div className="mt-3 p-3 bg-gray-50 rounded-lg">
        <h5 className="text-sm font-medium text-gray-700 mb-2">Cleanup Status</h5>
        <div className="grid grid-cols-2 gap-2">
          {checks.map(({ key, label, icon: Icon }) => (
            <div key={key} className="flex items-center gap-2 text-xs">
              <Icon className={`w-3 h-3 ${
                cleanup[key as keyof typeof cleanup] ? 'text-green-600' : 'text-gray-400'
              }`} />
              <span className={cleanup[key as keyof typeof cleanup] ? 'text-green-700' : 'text-gray-500'}>
                {label}
              </span>
              {cleanup[key as keyof typeof cleanup] && (
                <CheckCircle className="w-3 h-3 text-green-600" />
              )}
            </div>
          ))}
        </div>
      </div>
    );
  };

  const renderTargetInfo = (targetInfo: MigrationOperation['target_info']) => {
    const getTargetDisplay = () => {
      switch (targetInfo.type) {
        case 'node':
          return `Node: ${targetInfo.target_node}`;
        case 'local_disk':
          return `Disk: ${targetInfo.target_disk_id} (${targetInfo.target_node})`;
        case 'internal_nvmeof':
          return `Internal NVMe-oF: ${targetInfo.target_nvmeof_nqn}`;
        case 'external_nvmeof':
          return `External NVMe-oF: ${targetInfo.target_nvmeof_nqn}`;
        default:
          return 'Unknown target';
      }
    };

    return (
      <div className="text-xs text-gray-600">
        <span className="font-medium">Target:</span> {getTargetDisplay()}
      </div>
    );
  };

  if (operations.length === 0) {
    return (
      <div className="text-center py-8">
        <Zap className="w-12 h-12 text-gray-400 mx-auto mb-4" />
        <p className="text-gray-600">No active migration operations</p>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h3 className="text-lg font-semibold">Migration Operations</h3>
        <div className="text-xs text-gray-500">
          Last updated: {lastUpdate.toLocaleTimeString()}
        </div>
      </div>

      {operations.map((operation) => (
        <div
          key={operation.id}
          className={`border rounded-lg p-4 ${getStatusColor(operation.status)}`}
        >
          {/* Operation Header */}
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-3">
              {getStatusIcon(operation.status)}
              <div>
                <div className="flex items-center gap-2">
                  {getOperationTypeIcon(operation.operation_type)}
                  <span className="font-medium">
                    {operation.operation_type.replace('_', ' ').toUpperCase()}
                  </span>
                  {operation.volume_id && (
                    <span className="text-sm text-gray-600">({operation.volume_id})</span>
                  )}
                </div>
                <div className="text-sm">
                  RAID: {operation.raid_name} • {operation.stage}
                </div>
              </div>
            </div>
            
            <div className="flex items-center gap-2">
              <span className="text-sm font-medium">
                {operation.progress_percent.toFixed(1)}%
              </span>
              {operation.status === 'executing' && operation.throughput_mbps && (
                <span className="text-xs text-gray-600">
                  {operation.throughput_mbps.toFixed(0)} MB/s
                </span>
              )}
              <button
                onClick={() => toggleExpanded(operation.id)}
                className="px-2 py-1 text-xs bg-white bg-opacity-50 rounded hover:bg-opacity-75"
              >
                {expandedOperations.has(operation.id) ? 'Less' : 'More'}
              </button>
            </div>
          </div>

          {/* Progress Bar */}
          <div className="mt-3">
            <div className="w-full bg-white bg-opacity-30 rounded-full h-2">
              <div
                className={`h-2 rounded-full transition-all duration-300 ${
                  operation.status === 'completed' ? 'bg-green-500' :
                  operation.status === 'failed' ? 'bg-red-500' :
                  operation.status === 'cleanup' ? 'bg-orange-500' :
                  'bg-blue-500'
                }`}
                style={{ width: `${operation.progress_percent}%` }}
              />
            </div>
            
            {operation.data_copied_gb && operation.total_data_gb && (
              <div className="flex justify-between text-xs mt-1">
                <span>
                  {operation.data_copied_gb.toFixed(1)} GB / {operation.total_data_gb.toFixed(1)} GB
                </span>
                {operation.estimated_completion && (
                  <span>ETA: {operation.estimated_completion}</span>
                )}
              </div>
            )}
          </div>

          {/* Error Message */}
          {operation.error_message && (
            <div className="mt-3 p-2 bg-red-50 border border-red-200 rounded text-sm text-red-700">
              <AlertTriangle className="w-4 h-4 inline mr-2" />
              {operation.error_message}
            </div>
          )}

          {/* Expanded Details */}
          {expandedOperations.has(operation.id) && (
            <div className="mt-4 space-y-3">
              {/* Basic Info */}
              <div className="grid grid-cols-2 gap-4 text-sm">
                <div>
                  <span className="font-medium">Duration:</span> {formatDuration(operation.started_at)}
                </div>
                <div>
                  <span className="font-medium">Source:</span> {operation.source_node}
                </div>
              </div>

              {/* Target Info */}
              {renderTargetInfo(operation.target_info)}

              {/* Cleanup Status */}
              {operation.cleanup_status && renderCleanupStatus(operation.cleanup_status)}

              {/* Action Buttons */}
              {(operation.status === 'failed' || operation.status === 'executing') && (
                <div className="flex gap-2 pt-2">
                  {operation.status === 'failed' && onRetry && (
                    <button
                      onClick={() => onRetry(operation.id)}
                      className="px-3 py-1 text-xs bg-blue-600 text-white rounded hover:bg-blue-700"
                    >
                      Retry
                    </button>
                  )}
                  {operation.status === 'executing' && onCancel && (
                    <button
                      onClick={() => onCancel(operation.id)}
                      className="px-3 py-1 text-xs bg-red-600 text-white rounded hover:bg-red-700"
                    >
                      Cancel
                    </button>
                  )}
                </div>
              )}
            </div>
          )}
        </div>
      ))}
    </div>
  );
};
