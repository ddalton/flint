import React, { useState, useMemo } from 'react';
import { Navigate, Route, Routes } from 'react-router';
import { Database, Loader2 } from 'lucide-react';
import { Dashboard } from './components/Dashboard';
import { useAuth, useDashboardData } from './hooks/useDashboardData';
import { OperationsProvider } from './contexts/OperationsContext';
import { Button } from './components/ui/Button';

// Login Component
const LoginPage = ({ onLogin }: { onLogin: (username: string, password: string) => Promise<void> }) => {
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
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
    <div className="min-h-screen bg-gray-50 flex flex-col">
      {/* Same shell chrome as the app header — one visual language. */}
      <header className="bg-white shadow-sm border-b">
        <div className="max-w-screen-2xl mx-auto px-4 sm:px-6 lg:px-8 py-4 flex items-center">
          <Database aria-hidden="true" className="w-8 h-8 text-brand-600 mr-3" />
          <h1 className="text-page-title text-gray-900">Flint Dashboard</h1>
        </div>
      </header>

      <main className="flex-1 flex items-center justify-center px-4 py-12">
        <div className="bg-white rounded-lg shadow w-full max-w-md p-8">
          <h2 className="text-section text-gray-900 mb-1">Sign in</h2>
          <p className="text-sm text-gray-600 mb-6">Storage management console</p>

          {error && (
            <div role="alert" className="mb-4 p-3 bg-failed-50 border border-failed-200 text-failed-800 rounded-md text-sm">
              {error}
            </div>
          )}

          <form onSubmit={handleSubmit} className="space-y-4">
            <div>
              <label htmlFor="login-username" className="block text-sm font-medium text-gray-700 mb-1">
                Username
              </label>
              <input
                id="login-username"
                type="text"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                className="w-full px-3 py-2 border border-gray-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-brand-500"
                autoComplete="username"
                required
              />
            </div>
            <div>
              <label htmlFor="login-password" className="block text-sm font-medium text-gray-700 mb-1">
                Password
              </label>
              <input
                id="login-password"
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                className="w-full px-3 py-2 border border-gray-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-brand-500"
                autoComplete="current-password"
                required
              />
            </div>
            <Button
              type="submit"
              variant="primary"
              disabled={loading}
              className="w-full"
              icon={loading ? Loader2 : undefined}
              iconClass="animate-spin motion-reduce:animate-none"
            >
              {loading ? 'Signing in…' : 'Sign In'}
            </Button>
          </form>
        </div>
      </main>
    </div>
  );
};

// Main App Component
const App: React.FC = () => {
  const { isAuthenticated, login, logout } = useAuth();
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [showNodesWithDisksOnly, setShowNodesWithDisksOnly] = useState(false);
  
  // Memoize filters to prevent infinite loop
  const filters = useMemo(() => ({
    nodesWithDisksOnly: showNodesWithDisksOnly
  }), [showNodesWithDisksOnly]);
  
  // Only initialize dashboard data hook when authenticated
  const dashboardHook = useDashboardData(
    isAuthenticated ? autoRefresh : false,
    filters
  );
  
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
    // The URL is left untouched while logging in, so a deep link
    // (/volumes?filter=degraded) survives the auth gate.
    return <LoginPage onLogin={handleLogin} />;
  }

  const dashboard = (
    <Dashboard
      data={dashboardHook.data}
      loading={dashboardHook.loading}
      stats={dashboardHook.stats}
      autoRefresh={autoRefresh}
      onAutoRefreshChange={setAutoRefresh}
      onRefresh={handleRefresh}
      onLogout={handleLogout}
      connectionError={dashboardHook.connectionError}
      showNodesWithDisksOnly={showNodesWithDisksOnly}
      onShowNodesWithDisksOnlyChange={setShowNodesWithDisksOnly}
    />
  );

  return (
    <OperationsProvider
      autoRefresh={autoRefresh}
      onAutoRefreshChange={setAutoRefresh}
    >
      <Routes>
        <Route path="/:tab?" element={dashboard} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </OperationsProvider>
  );
};

export default App;
