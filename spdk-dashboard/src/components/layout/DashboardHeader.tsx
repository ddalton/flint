import React from 'react';
import { Database, RefreshCw, LogOut, AlertTriangle } from 'lucide-react';
import { useOperations } from '../../contexts/OperationsContext';
import { Button, IconButton } from '../ui/Button';
import { Chip } from '../ui/Chip';

interface DashboardHeaderProps {
  autoRefresh: boolean;
  onAutoRefreshChange: (enabled: boolean) => void;
  onRefresh: () => void;
  onLogout: () => void;
  connectionError?: string | null;
}

export const DashboardHeader: React.FC<DashboardHeaderProps> = ({
  autoRefresh,
  onAutoRefreshChange,
  onRefresh,
  onLogout,
  connectionError = null
}) => {
  const { shouldPauseRefresh } = useOperations();
  return (
    <header className="bg-white shadow-sm border-b">
      <div className="max-w-screen-2xl mx-auto px-4 sm:px-6 lg:px-8">
        <div className="flex justify-between items-center py-4">
          <div className="flex items-center gap-4">
            <div className="flex items-center">
              <Database className="w-8 h-8 text-brand-600 mr-3" />
              <h1 className="text-page-title text-gray-900">Flint Dashboard</h1>
            </div>
            {connectionError && (
              <Chip
                icon={AlertTriangle}
                iconClass="text-failed-600"
                label="Backend unreachable — showing last known data"
                chip="bg-failed-100 text-failed-800 border-failed-300"
                title={connectionError}
              />
            )}
          </div>
          
          <div className="flex items-center gap-4">
            <label className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={autoRefresh}
                onChange={(e) => onAutoRefreshChange(e.target.checked)}
                className="rounded"
              />
              Auto-refresh
            </label>
            
            <IconButton
              icon={RefreshCw}
              aria-label="Refresh dashboard"
              onClick={onRefresh}
              disabled={shouldPauseRefresh}
              className="disabled:hover:bg-transparent"
              title={shouldPauseRefresh ? "Refresh paused while dialog is open" : "Refresh dashboard"}
            />

            <Button variant="ghost" icon={LogOut} onClick={onLogout}>
              Logout
            </Button>
          </div>
        </div>
      </div>
    </header>
  );
};
