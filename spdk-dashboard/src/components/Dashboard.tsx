import React, { lazy, Suspense, useState, useEffect, useRef } from 'react';
import { Navigate, useNavigate, useParams, useSearchParams } from 'react-router';
import { Filter } from 'lucide-react';
import { isFreshCluster, uninitializedDiskCount } from './setup/batchSetup';
import { volumeFilterDisplay } from './ui/status';
import { parseTab, parseVolumeFilter } from '../routes';
import type { DashboardData, VolumeFilter, DiskFilter, VolumeReplicaFilter } from '../hooks/useDashboardData';
import { DashboardHeader } from './layout/DashboardHeader';
import { StatCards } from './stats/StatCards';
import { TabNavigation } from './ui/TabNavigation';
import { Skeleton, TabSkeleton } from './ui/Skeleton';

// Tab contents are code-split (Phase 4): first paint ships only the shell;
// each tab's chunk — and the reactflow/recharts vendors — load on demand.
const VolumeStatusChart = lazy(() =>
  import('./charts/VolumeStatusChart').then(m => ({ default: m.VolumeStatusChart })));
const DiskStatusChart = lazy(() =>
  import('./charts/DiskStatusChart').then(m => ({ default: m.DiskStatusChart })));
const EnhancedRaidTopologyChart = lazy(() =>
  import('./charts/EnhancedRaidTopologyChart').then(m => ({ default: m.EnhancedRaidTopologyChart })));
const VolumesTable = lazy(() =>
  import('./tables/VolumesTable').then(m => ({ default: m.VolumesTable })));
const DisksTable = lazy(() =>
  import('./tables/DisksTable').then(m => ({ default: m.DisksTable })));
const FilteredNodesView = lazy(() =>
  import('./nodes/FilteredNodesView').then(m => ({ default: m.FilteredNodesView })));
const DiskSetupTab = lazy(() =>
  import('./setup/DiskSetupTab').then(m => ({ default: m.DiskSetupTab })));
const EnhancedSnapshotsTab = lazy(() =>
  import('./snapshots/EnhancedSnapshotsTab').then(m => ({ default: m.EnhancedSnapshotsTab })));
const EventsTab = lazy(() =>
  import('./events/EventsTab').then(m => ({ default: m.EventsTab })));

interface DashboardProps {
  data: DashboardData;
  loading: boolean;
  stats: {
    totalVolumes: number;
    healthyVolumes: number;
    degradedVolumes: number;
    failedVolumes: number;
    faultedVolumes: number;
    volumesWithRebuilding: number;
    localNVMeVolumes: number;
    orphanedVolumes: number;
    totalDisks: number;
    healthyDisks: number;
    formattedDisks: number;
  };
  autoRefresh: boolean;
  onAutoRefreshChange: (enabled: boolean) => void;
  onRefresh: () => void;
  onLogout: () => void;
  connectionError?: string | null;
  showNodesWithDisksOnly?: boolean;
  onShowNodesWithDisksOnlyChange?: (enabled: boolean) => void;
}

export const Dashboard: React.FC<DashboardProps> = ({
  data,
  loading,
  stats,
  autoRefresh,
  onAutoRefreshChange,
  onRefresh,
  onLogout,
  connectionError = null,
  showNodesWithDisksOnly = false,
  onShowNodesWithDisksOnlyChange
}) => {
  // URL state (Phase 3): the tab is the path segment, filters are search
  // params — a pasted link reproduces the exact view, and refresh loses
  // nothing.
  const { tab: tabSegment } = useParams();
  const activeTab = parseTab(tabSegment);
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();

  const volumeFilter: VolumeFilter = parseVolumeFilter(searchParams.get('filter'));
  const diskFilter: DiskFilter = searchParams.get('disk');
  const volumeReplicaFilter: VolumeReplicaFilter = searchParams.get('replicas');

  const setFilterParams = (
    updates: Record<string, string | null>,
    options: { tab?: string } = {}
  ) => {
    const next = new URLSearchParams(searchParams);
    for (const [key, value] of Object.entries(updates)) {
      if (value === null || value === 'all') next.delete(key);
      else next.set(key, value);
    }
    const qs = next.toString();
    if (options.tab !== undefined) {
      navigate(`/${options.tab}${qs ? `?${qs}` : ''}`);
    } else {
      setSearchParams(next);
    }
  };

  // State-aware landing (plan Decision 2): decided once, from the first
  // real data, and ONLY on the bare "/" entry point — an explicit deep link
  // (/overview, /volumes?...) is a user choice and is never hijacked. A
  // fresh cluster (zero initialized lvstores) lands on Disk Setup with
  // onboarding; a provisioned cluster stays on Overview so an operator
  // arriving mid-incident is never dropped into a wizard.
  const [onboardingLanding, setOnboardingLanding] = useState(false);
  const landingDecided = useRef(false);
  const bareEntry = tabSegment === undefined;
  useEffect(() => {
    if (landingDecided.current || loading || connectionError || !bareEntry) return;
    landingDecided.current = true;
    if (data.nodes.length > 0 && isFreshCluster(data.disks)) {
      setOnboardingLanding(true);
      navigate('/disk-setup', { replace: true });
    }
  }, [loading, connectionError, data.nodes, data.disks, bareEntry, navigate]);

  // Persistent nav nudge while any node still has uninitialized disks
  const uninitializedDisks = uninitializedDiskCount(data.disks);

  // Unknown path segment: back to the landing entry point.
  if (activeTab === null) {
    return <Navigate to="/" replace />;
  }

  if (loading && data.volumes.length === 0) {
    return (
      <div className="min-h-screen bg-gray-50 p-8" role="status" aria-label="Loading dashboard">
        <div className="max-w-screen-2xl mx-auto space-y-6">
          <Skeleton className="h-12 w-72" />
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
            <Skeleton className="h-24" />
            <Skeleton className="h-24" />
            <Skeleton className="h-24" />
            <Skeleton className="h-24" />
          </div>
          <Skeleton className="h-96 w-full" />
        </div>
      </div>
    );
  }

  const handleFilterClick = (filter: VolumeFilter) => {
    // Clicking the active filter clears it; applying a filter jumps to the
    // Volumes tab. Either way the other filters reset.
    const toggled = volumeFilter === filter ? 'all' : filter;
    setFilterParams(
      { filter: toggled, disk: null, replicas: null },
      toggled !== 'all' ? { tab: 'volumes' } : {}
    );
  };

  const handleClearFilter = () => {
    setFilterParams({ filter: null });
  };

  const handleClearDiskFilter = () => {
    setFilterParams({ disk: null });
  };

  const handleClearVolumeReplicaFilter = () => {
    setFilterParams({ replicas: null });
  };

  const handleDiskClick = (diskId: string) => {
    // Filter volumes down to one disk, over on the Volumes tab.
    setFilterParams({ disk: diskId, replicas: null }, { tab: 'volumes' });
  };

  const handleReplicaClick = (volumeId: string) => {
    // Show one volume's replica disks, over on the Disks tab.
    setFilterParams({ replicas: volumeId, disk: null }, { tab: 'disks' });
  };

  // Get volumes that are on the selected disk
  const getVolumesOnDisk = (diskId: string) => {
    const disk = data.disks.find(d => d.id === diskId);
    if (!disk) return [];
    
    // Return volumes that have replicas on this disk
    return data.volumes.filter(volume => 
      disk.provisioned_volumes.some(pv => pv.volume_id === volume.id)
    );
  };

  // One vocabulary for filter naming/coloring across all views (status.ts).
  const filterInfo = volumeFilterDisplay(volumeFilter);

  const renderTabContent = () => {
    switch (activeTab) {
      case 'overview':
        return (
          <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
            <VolumeStatusChart volumes={data.volumes} />
            <DiskStatusChart disks={data.disks} />
            <div className="lg:col-span-2">
              <EnhancedRaidTopologyChart volumes={data.volumes} disks={data.disks} nodeNames={data.nodes} nodeInfo={data.node_info}/>
            </div>
          </div>
        );

      case 'volumes':
        return (
          <VolumesTable 
            volumes={diskFilter ? getVolumesOnDisk(diskFilter) : data.volumes}
            rawVolumes={data.raw_volumes}
            disks={data.disks}
            activeFilter={volumeFilter}
            diskFilter={diskFilter}
            onClearFilter={handleClearFilter}
            onClearDiskFilter={handleClearDiskFilter}
            onReplicaClick={handleReplicaClick}
            onRefresh={onRefresh}
          />
        );

      case 'disks':
        return (
          <DisksTable 
            disks={data.disks}
            volumes={data.volumes}
            stats={stats}
            volumeFilter={volumeFilter}
            volumeReplicaFilter={volumeReplicaFilter}
            onDiskClick={handleDiskClick}
            onClearVolumeReplicaFilter={handleClearVolumeReplicaFilter}
            onDiskVolumeFilter={handleDiskClick}
          />
        );

      case 'disk-setup':
        return <DiskSetupTab onboarding={onboardingLanding} />;

      case 'nodes':
        return (
          <FilteredNodesView 
            data={data}
            activeFilter={volumeFilter}
            onClearFilter={handleClearFilter}
            onDiskVolumeFilter={handleDiskClick}
            showNodesWithDisksOnly={showNodesWithDisksOnly}
            onShowNodesWithDisksOnlyChange={onShowNodesWithDisksOnlyChange}
          />
        );

      case 'events':
        return <EventsTab />;

      case 'snapshots':
        return <EnhancedSnapshotsTab />;

      default:
        return null;
    }
  };

  return (
    <div className="min-h-screen bg-gray-50">
      <DashboardHeader
        autoRefresh={autoRefresh}
        onAutoRefreshChange={onAutoRefreshChange}
        onRefresh={onRefresh}
        onLogout={onLogout}
        connectionError={connectionError}
      />

      <div className="max-w-screen-2xl mx-auto px-4 sm:px-6 lg:px-8 py-8">
        {/* Enhanced filter indication */}
        {volumeFilter !== 'all' && (
          <div className="mb-6">
            <div className={`p-4 rounded-lg border-2 ${filterInfo.bgColor} ${filterInfo.borderColor}`}>
              <div className="flex items-center justify-between">
                <div className="flex items-center gap-3">
                  <span className="text-2xl">{filterInfo.icon}</span>
                  <div>
                    <h3 className="font-semibold text-lg">
                      {filterInfo.name}
                    </h3>
                    <p className="text-sm text-gray-600">
                      {filterInfo.description}
                    </p>
                  </div>
                </div>
                <div className="flex items-center gap-4">
                  {volumeFilter === 'faulted' && (
                    <div className="text-sm">
                      <div className="text-yellow-700">🟡 {stats.degradedVolumes} Degraded</div>
                      <div className="text-red-700">🔴 {stats.failedVolumes} Failed</div>
                    </div>
                  )}
                  <button
                    onClick={handleClearFilter}
                    className="px-4 py-2 bg-white border border-gray-300 rounded-md hover:bg-gray-50 text-sm font-medium"
                  >
                    Clear Filter
                  </button>
                </div>
              </div>
            </div>
          </div>
        )}

        {/* Filter Cards Section - Enhanced with Gradient */}
        <div className="bg-gradient-to-r from-blue-50 via-indigo-50 to-purple-50 rounded-lg p-6 mb-8 border border-indigo-200 shadow-md">
          <div className="flex items-center justify-between mb-4">
            <div className="flex items-center gap-3">
              <div className="p-2 bg-white rounded-lg shadow-sm">
                <Filter className="w-5 h-5 text-indigo-600" />
              </div>
              <div>
                <h3 className="text-lg font-semibold text-gray-900">Quick Filters</h3>
                <p className="text-sm text-gray-600">Click any card to filter volumes</p>
              </div>
              {volumeFilter !== 'all' && (
                <span className="px-3 py-1 text-sm bg-indigo-100 text-indigo-800 rounded-full font-medium ml-4">
                  Active: {filterInfo.name}
                </span>
              )}
            </div>
            {volumeFilter !== 'all' && (
              <button
                onClick={handleClearFilter}
                className="px-4 py-2 bg-white text-indigo-600 rounded-lg shadow-sm hover:shadow-md transition-shadow font-medium text-sm"
              >
                Clear filter
              </button>
            )}
          </div>
          
          <StatCards 
            stats={stats} 
            activeFilter={volumeFilter}
            onFilterClick={handleFilterClick}
          />
        </div>

        {/* Main Content Panel */}
        <div className="bg-white rounded-lg shadow-lg overflow-hidden">
          {/* Tab Navigation with Background */}
          <div className="bg-gray-50 border-b border-gray-200">
            <TabNavigation
              activeTab={activeTab}
              badges={{ 'disk-setup': uninitializedDisks }}
            />
          </div>
          
          <div className="p-6">
            <Suspense fallback={<TabSkeleton />}>
              {renderTabContent()}
            </Suspense>
          </div>
        </div>

      </div>
    </div>
  );
};