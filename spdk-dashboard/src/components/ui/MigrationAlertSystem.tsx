import React, { useState, useEffect } from 'react';
import { AlertTriangle, CheckCircle, XCircle, Bell, X, Clock, RefreshCw, Zap, HardDrive, Server } from 'lucide-react';

export interface MigrationAlert {
  id: string;
  type: 'success' | 'warning' | 'error' | 'info';
  operation_type: 'node_migration' | 'member_migration' | 'member_addition';
  raid_name: string;
  volume_id?: string;
  title: string;
  message: string;
  timestamp: string;
  auto_dismiss?: boolean;
  dismiss_after?: number; // seconds
  
  // Migration-specific data
  source_node?: string;
  target_info?: string;
  cleanup_completed?: boolean;
  old_member_removed?: boolean;
  
  // Action buttons
  actions?: {
    label: string;
    action: () => void;
    style?: 'primary' | 'secondary' | 'danger';
  }[];
}

interface MigrationAlertSystemProps {
  alerts: MigrationAlert[];
  onDismiss: (alertId: string) => void;
  maxVisible?: number;
  position?: 'top-right' | 'top-left' | 'bottom-right' | 'bottom-left';
}

export const MigrationAlertSystem: React.FC<MigrationAlertSystemProps> = ({
  alerts,
  onDismiss,
  maxVisible = 5,
  position = 'top-right'
}) => {
  const [visibleAlerts, setVisibleAlerts] = useState<MigrationAlert[]>([]);

  // Auto-dismiss alerts
  useEffect(() => {
    alerts.forEach(alert => {
      if (alert.auto_dismiss && alert.dismiss_after) {
        const timer = setTimeout(() => {
          onDismiss(alert.id);
        }, alert.dismiss_after * 1000);

        return () => clearTimeout(timer);
      }
    });
  }, [alerts, onDismiss]);

  // Manage visible alerts (show most recent)
  useEffect(() => {
    const recent = alerts
      .sort((a, b) => new Date(b.timestamp).getTime() - new Date(a.timestamp).getTime())
      .slice(0, maxVisible);
    setVisibleAlerts(recent);
  }, [alerts, maxVisible]);

  const getAlertIcon = (type: string) => {
    switch (type) {
      case 'success':
        return <CheckCircle className="w-5 h-5 text-green-600" />;
      case 'error':
        return <XCircle className="w-5 h-5 text-red-600" />;
      case 'warning':
        return <AlertTriangle className="w-5 h-5 text-yellow-600" />;
      default:
        return <Bell className="w-5 h-5 text-blue-600" />;
    }
  };

  const getAlertColors = (type: string) => {
    switch (type) {
      case 'success':
        return 'bg-green-50 border-green-200 text-green-800';
      case 'error':
        return 'bg-red-50 border-red-200 text-red-800';
      case 'warning':
        return 'bg-yellow-50 border-yellow-200 text-yellow-800';
      default:
        return 'bg-blue-50 border-blue-200 text-blue-800';
    }
  };

  const getOperationIcon = (operationType: string) => {
    switch (operationType) {
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

  const getPositionClasses = () => {
    switch (position) {
      case 'top-left':
        return 'top-4 left-4';
      case 'bottom-right':
        return 'bottom-4 right-4';
      case 'bottom-left':
        return 'bottom-4 left-4';
      default:
        return 'top-4 right-4';
    }
  };

  const getButtonStyle = (style?: string) => {
    switch (style) {
      case 'danger':
        return 'bg-red-600 hover:bg-red-700 text-white';
      case 'secondary':
        return 'bg-gray-600 hover:bg-gray-700 text-white';
      default:
        return 'bg-blue-600 hover:bg-blue-700 text-white';
    }
  };

  if (visibleAlerts.length === 0) return null;

  return (
    <div className={`fixed ${getPositionClasses()} z-50 space-y-3 w-96 max-w-sm`}>
      {visibleAlerts.map((alert) => (
        <div
          key={alert.id}
          className={`rounded-lg border p-4 shadow-lg ${getAlertColors(alert.type)} transform transition-all duration-300 ease-in-out`}
        >
          {/* Alert Header */}
          <div className="flex items-start justify-between">
            <div className="flex items-center gap-2">
              {getAlertIcon(alert.type)}
              {getOperationIcon(alert.operation_type)}
              <h4 className="font-semibold text-sm">{alert.title}</h4>
            </div>
            <button
              onClick={() => onDismiss(alert.id)}
              className="text-gray-500 hover:text-gray-700"
            >
              <X className="w-4 h-4" />
            </button>
          </div>

          {/* Alert Content */}
          <div className="mt-2">
            <p className="text-sm">{alert.message}</p>
            
            {/* Migration Details */}
            <div className="mt-2 text-xs space-y-1">
              <div className="flex items-center gap-1">
                <span className="font-medium">RAID:</span>
                <span>{alert.raid_name}</span>
                {alert.volume_id && (
                  <>
                    <span>•</span>
                    <span>Volume: {alert.volume_id}</span>
                  </>
                )}
              </div>
              
              {alert.source_node && (
                <div className="flex items-center gap-1">
                  <span className="font-medium">Source:</span>
                  <span>{alert.source_node}</span>
                  {alert.target_info && (
                    <>
                      <span>→</span>
                      <span>{alert.target_info}</span>
                    </>
                  )}
                </div>
              )}

              {/* Cleanup Status */}
              {alert.type === 'success' && (alert.cleanup_completed || alert.old_member_removed) && (
                <div className="mt-2 p-2 bg-green-100 rounded text-xs">
                  <div className="flex items-center gap-2">
                    <CheckCircle className="w-3 h-3 text-green-600" />
                    <span className="font-medium text-green-700">Cleanup Status:</span>
                  </div>
                  <div className="mt-1 space-y-1">
                    {alert.cleanup_completed && (
                      <div className="flex items-center gap-1 text-green-600">
                        <CheckCircle className="w-3 h-3" />
                        <span>Migration cleanup completed</span>
                      </div>
                    )}
                    {alert.old_member_removed && (
                      <div className="flex items-center gap-1 text-green-600">
                        <CheckCircle className="w-3 h-3" />
                        <span>Old RAID member removed</span>
                      </div>
                    )}
                  </div>
                </div>
              )}
            </div>

            {/* Timestamp */}
            <div className="mt-2 text-xs opacity-75">
              {new Date(alert.timestamp).toLocaleTimeString()}
            </div>
          </div>

          {/* Action Buttons */}
          {alert.actions && alert.actions.length > 0 && (
            <div className="mt-3 flex gap-2">
              {alert.actions.map((action, index) => (
                <button
                  key={index}
                  onClick={action.action}
                  className={`px-3 py-1 text-xs rounded ${getButtonStyle(action.style)}`}
                >
                  {action.label}
                </button>
              ))}
            </div>
          )}
        </div>
      ))}
    </div>
  );
};

// Helper function to create migration alerts
export const createMigrationAlert = (
  type: 'success' | 'warning' | 'error' | 'info',
  operationType: 'node_migration' | 'member_migration' | 'member_addition',
  raidName: string,
  title: string,
  message: string,
  additional?: Partial<MigrationAlert>
): MigrationAlert => ({
  id: `alert-${Date.now()}-${Math.random().toString(36).substr(2, 9)}`,
  type,
  operation_type: operationType,
  raid_name: raidName,
  title,
  message,
  timestamp: new Date().toISOString(),
  auto_dismiss: type === 'success' || type === 'info',
  dismiss_after: type === 'success' ? 10 : type === 'info' ? 8 : undefined,
  ...additional
});

// Predefined alert templates
export const MigrationAlerts = {
  migrationStarted: (operationType: string, raidName: string, target: string) =>
    createMigrationAlert(
      'info',
      operationType as any,
      raidName,
      'Migration Started',
      `RAID migration operation has begun targeting ${target}.`,
      { target_info: target }
    ),

  migrationCompleted: (operationType: string, raidName: string, target: string, cleanupCompleted = true) =>
    createMigrationAlert(
      'success',
      operationType as any,
      raidName,
      'Migration Completed',
      `RAID migration completed successfully. All data has been transferred and verified.`,
      { 
        target_info: target,
        cleanup_completed: cleanupCompleted,
        old_member_removed: cleanupCompleted
      }
    ),

  migrationFailed: (operationType: string, raidName: string, error: string, retryAction?: () => void) =>
    createMigrationAlert(
      'error',
      operationType as any,
      raidName,
      'Migration Failed',
      `RAID migration failed: ${error}`,
      {
        auto_dismiss: false,
        actions: retryAction ? [
          { label: 'Retry', action: retryAction, style: 'primary' },
          { label: 'View Details', action: () => {}, style: 'secondary' }
        ] : [
          { label: 'View Details', action: () => {}, style: 'secondary' }
        ]
      }
    ),

  cleanupInProgress: (raidName: string) =>
    createMigrationAlert(
      'info',
      'node_migration',
      raidName,
      'Cleanup In Progress',
      'Removing old RAID members and updating metadata. Please do not interrupt this process.',
      { auto_dismiss: false }
    ),

  cleanupCompleted: (raidName: string, removedMember: string) =>
    createMigrationAlert(
      'success',
      'member_migration',
      raidName,
      'Cleanup Completed',
      `Old RAID member ${removedMember} has been safely removed and metadata updated.`,
      { 
        cleanup_completed: true,
        old_member_removed: true
      }
    ),

  dataVerificationFailed: (raidName: string, details: string) =>
    createMigrationAlert(
      'warning',
      'member_migration',
      raidName,
      'Data Verification Warning',
      `Data verification completed with warnings: ${details}. Manual review recommended.`,
      { 
        auto_dismiss: false,
        actions: [
          { label: 'Review', action: () => {}, style: 'primary' },
          { label: 'Force Complete', action: () => {}, style: 'danger' }
        ]
      }
    )
};
