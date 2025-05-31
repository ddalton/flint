import React, { useState } from 'react';
import { Database } from 'lucide-react';
import { Dashboard } from './components/Dashboard';
import { useAuth, useDashboardData } from './hooks/useDashboardData';

// Login Component
const LoginPage = ({ onLogin }: { onLogin: (username: string, password: string) => Promise<void> }) => {
  const [username, setUsername] = useState('admin');
  const [password, setPassword] = useState('spdk-admin-2025');
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setLoading(true);
    setError('');
    
    try {
      await onLogin(username, password);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Login failed');
    } finally {
      setLoading(false);
    }
  };

  return (
    <div className="min-h-screen flex items-center justify-center bg-gradient-to-br from-blue-600 to-blue-800">
      <div className="bg-white rounded-lg shadow-xl p-8 w-full max-w-md">
        <div className="text-center mb-8">
          <div className="mx-auto w-16 h-16 bg-blue-100 rounded-full flex items-center justify-center mb-4">
            <Database className="w-8 h-8 text-blue-600" />
          </div>
          <h1 className="text-3xl font-bold text-gray-900 mb-2">SPDK CSI Dashboard</h1>
          <p className="text-gray-600">Sign in to access the storage management console</p>
        </div>
        
        {error && (
          <div className="mb-4 p-3 bg-red-100 border border-red-400 text-red-700 rounded">
            {error}
          </div>
        )}
        
        <form onSubmit={handleSubmit}>
          <div className="mb-4">
            <label className="block text-gray-700 text-sm font-bold mb-2">
              Username
            </label>
            <input
              type="text"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
              required
            />
          </div>
          <div className="mb-6">
            <label className="block text-gray-700 text-sm font-bold mb-2">
              Password
            </label>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
              required
            />
          </div>
          <button
            type="submit"
            disabled={loading}
            className="w-full bg-blue-600 text-white py-2 px-4 rounded-md hover:bg-blue-700 focus:outline-none focus:ring-2 focus:ring-blue-500 disabled:opacity-50 flex items-center justify-center"
          >
            {loading ? (
              <div className="animate-spin rounded-full h-5 w-5 border-b-2 border-white"></div>
            ) : (
              'Sign In'
            )}
          </button>
        </form>
        
        <div className="mt-4 p-3 bg-gray-50 rounded-md">
          <p className="text-sm text-gray-600">
            Default credentials: admin / spdk-admin-2025
          </p>
        </div>
      </div>
    </div>
  );
};

// Main App Component
const App: React.FC = () => {
  const { isAuthenticated, login, logout } = useAuth();
  const [autoRefresh, setAutoRefresh] = useState(true);
  
  // Only initialize dashboard data hook when authenticated
  const dashboardHook = useDashboardData(isAuthenticated ? autoRefresh : false);
  
  const handleLogin = async (username: string, password: string) => {
    await login(username, password);
  };
  
  const handleLogout = () => {
    logout();
  };
  
  const handleRefresh = () => {
    if (isAuthenticated) {
      dashboardHook.refreshData();
    }
  };
  
  if (!isAuthenticated) {
    return <LoginPage onLogin={handleLogin} />;
  }
  
  return (
    <Dashboard
      data={dashboardHook.data}
      loading={dashboardHook.loading}
      stats={dashboardHook.stats}
      autoRefresh={autoRefresh}
      onAutoRefreshChange={setAutoRefresh}
      onRefresh={handleRefresh}
      onLogout={handleLogout}
    />
  );
};

export default App;