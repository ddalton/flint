import React from 'react';
import { Database, RefreshCw, LogOut, AlertTriangle } from 'lucide-react';

interface DashboardHeaderProps {
  autoRefresh: boolean;
  onAutoRefreshChange: (enabled: boolean) => void;
  onRefresh: () => void;
  onLogout: () => void;
  usingMockData?: boolean;
}

export const DashboardHeader: React.FC<DashboardHeaderProps> = ({
  autoRefresh,
  onAutoRefreshChange,
  onRefresh,
  onLogout,
  usingMockData = false
}) => {
  return (
    <header className="bg-white shadow-sm border-b">
      <div className="max-w-7xl mx-auto px-4 sm:px-6 lg:px-8">
        <div className="flex justify-between items-center py-4">
          <div className="flex items-center gap-4">
            <div className="flex items-center">
              <Database className="w-8 h-8 text-blue-600 mr-3" />
              <h1 className="text-2xl font-bold text-gray-900">Flint Dashboard</h1>
            </div>
            {usingMockData && (
              <div className="flex items-center gap-2 px-3 py-1 bg-yellow-100 border border-yellow-300 rounded-full">
                <AlertTriangle className="w-4 h-4 text-yellow-600" />
                <span className="text-sm font-medium text-yellow-800">Mock Data</span>
              </div>
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
            
            <button
              onClick={onRefresh}
              className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md"
            >
              <RefreshCw className="w-5 h-5" />
            </button>
            
            <button
              onClick={onLogout}
              className="flex items-center gap-2 px-3 py-2 text-sm text-gray-700 hover:text-gray-900 hover:bg-gray-100 rounded-md"
            >
              <LogOut className="w-4 h-4" />
              Logout
            </button>
          </div>
        </div>
      </div>
    </header>
  );
};
