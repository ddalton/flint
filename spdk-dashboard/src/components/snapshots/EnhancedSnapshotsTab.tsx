import React, { useState, useEffect, useMemo } from 'react';
import { useSearchParams } from 'react-router';
import { apiFetch } from '../../api/client';
import { 
  Camera, RefreshCw, Search, Filter, ChevronDown, FileText, 
  GitBranch, HardDrive, BarChart3, CheckCircle, Layers, Database, 
  Copy, Download, AlertTriangle, TrendingUp
} from 'lucide-react';
import { SnapshotsListView } from './SnapshotsListView';
import { EnhancedSnapshotsTreeView } from './EnhancedSnapshotsTreeView';
import { SnapshotStorageView } from './SnapshotStorageView';
import { SnapshotTimelineView } from './SnapshotTimelineView';
import { SnapshotDetailModal } from './SnapshotDetailModal';
import { useOperations } from '../../contexts/OperationsContext';
import { useDashboardData } from '../../hooks/useDashboardData';
import { TabSkeleton } from '../ui/Skeleton';
import { Button, IconButton } from '../ui/Button';
import type { components } from '../../api/schema';
import type {
  SnapshotDetails,
  SnapshotTreeNode,
  SnapshotTypeFilter,
  SnapshotViewMode
} from './types';

export const EnhancedSnapshotsTab: React.FC = () => {
  const [snapshots, setSnapshots] = useState<SnapshotDetails[]>([]);
  const [snapshotTree, setSnapshotTree] = useState<Record<string, SnapshotTreeNode>>({});
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [activeView, setActiveView] = useState<SnapshotViewMode>('storage'); // Default to storage view
  const [searchTerm, setSearchTerm] = useState('');
  const [typeFilter, setTypeFilter] = useState<SnapshotTypeFilter>('all');
  const [volumeFilter, setVolumeFilter] = useState<string>('all');
  // Selection is by id in the URL (?snapshot=) — the modal re-derives its
  // snapshot from the fetched list each render, so an open detail survives
  // refresh and can be pasted as a link.
  const [searchParams, setSearchParams] = useSearchParams();
  const selectedSnapshotId = searchParams.get('snapshot');
  const setSelectedSnapshot = (snap: SnapshotDetails | null) => {
    setSearchParams(prev => {
      const next = new URLSearchParams(prev);
      if (snap) next.set('snapshot', snap.snapshot_id);
      else next.delete('snapshot');
      return next;
    });
  };
  const selectedSnapshot = useMemo(
    () => snapshots.find(snap => snap.snapshot_id === selectedSnapshotId) ?? null,
    [snapshots, selectedSnapshotId]
  );
  const [expandedVolumes, setExpandedVolumes] = useState<Set<string>>(new Set());
  const [showFilters, setShowFilters] = useState(false);
  const { setDialogVisible } = useOperations();

  useEffect(() => {
    setDialogVisible(selectedSnapshotId !== null);
  }, [selectedSnapshotId, setDialogVisible]);

  // Get unique volumes for filter dropdown
  const availableVolumes = useMemo(() => {
    return Array.from(new Set(snapshots.map(snap => snap.source_volume_id)));
  }, [snapshots]);

  // PV id -> PVC name for the timeline's search (operators know PVC names,
  // the timeline keys on pv ids). Shares the app-wide dashboard query cache;
  // no polling from this tab.
  const { data: dashboardData } = useDashboardData(false);
  const volumeNames = useMemo(
    () =>
      Object.fromEntries(
        (dashboardData?.volumes ?? []).map(v => [v.id, v.name])
      ) as Record<string, string>,
    [dashboardData]
  );

  const [topologyVolume, setTopologyVolume] = useState<string>('all');

  useEffect(() => {
    fetchSnapshotData();
  }, []);

  const fetchSnapshotData = async () => {
    setRefreshing(true);
    setError(null);
    try {
      // Fetch both list and tree data with enhanced storage information
      const [snapshotsResponse, treeResponse] = await Promise.all([
        apiFetch('/api/snapshots'),
        apiFetch('/api/snapshots/tree')
      ]);

      const snapshotsContentType = snapshotsResponse.headers.get("content-type") || '';
      if (snapshotsResponse.ok && snapshotsContentType.indexOf("application/json") !== -1) {
        const snapshotsData = await snapshotsResponse.json();
        setSnapshots(toSnapshotDetails(snapshotsData));
      } else {
        throw new Error(
          snapshotsResponse.ok ? 'Snapshots: non-JSON response' : `Snapshots unavailable (HTTP ${snapshotsResponse.status})`
        );
      }

      const treeContentType = treeResponse.headers.get("content-type") || '';
      if (treeResponse.ok && treeContentType.indexOf("application/json") !== -1) {
        const treeData = await treeResponse.json();
        // The tree arrives with real storage_analytics (computed backend-side
        // from SPDK bdev consumption) — no client-side synthesis.
        setSnapshotTree(treeData);
      } else {
        throw new Error(
          treeResponse.ok ? 'Snapshot tree: non-JSON response' : `Snapshot tree unavailable (HTTP ${treeResponse.status})`
        );
      }
    } catch (error) {
      // Never substitute fabricated snapshots for a failed backend — surface
      // the failure and keep whatever real data is already on screen.
      console.error('Failed to fetch snapshot data:', error);
      setError(error instanceof Error ? error.message : String(error));
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  };

  // The typed /api/snapshots contract (generated schema): one row per
  // logical snapshot, per-node copies under replica_bdev_details — a
  // drifted assumption here is a compile error.
  type BackendSnapshot = components['schemas']['DashboardSnapshot'];

  // Map wire rows onto the tab's SnapshotDetails shape. This listing
  // carries no clone lineage (SPDK lvol listings don't expose it), so
  // parent/child relationships stay empty.
  const toSnapshotDetails = (backendSnapshots: BackendSnapshot[]): SnapshotDetails[] =>
    backendSnapshots.map(snap => ({
      ...snap,
      snapshot_id: snap.snapshot_uuid,
      snapshot_type: 'Bdev',
      child_snapshot_ids: [],
    }));

  // Filter and search logic
  const filteredSnapshots = useMemo(() => {
    let result = snapshots;

    // Search filter
    if (searchTerm) {
      const searchLower = searchTerm.toLowerCase();
      result = result.filter(snap => 
        snap.snapshot_id.toLowerCase().includes(searchLower) ||
        snap.source_volume_id.toLowerCase().includes(searchLower) ||
        (snap.replica_bdev_details || []).some(replica => 
          replica.node.toLowerCase().includes(searchLower) ||
          replica.name.toLowerCase().includes(searchLower)
        )
      );
    }

    // Type filter
    if (typeFilter !== 'all') {
      result = result.filter(snap => snap.snapshot_type === typeFilter);
    }

    // Volume filter
    if (volumeFilter !== 'all') {
      result = result.filter(snap => snap.source_volume_id === volumeFilter);
    }

    return result.sort((a, b) => 
      new Date(b.creation_time).getTime() - new Date(a.creation_time).getTime()
    );
  }, [snapshots, searchTerm, typeFilter, volumeFilter]);

  // Calculate storage insights
  const storageInsights = useMemo(() => {
    const totalLogicalSize = Object.values(snapshotTree).reduce((sum, tree) => 
      sum + (tree.volume_size || 0), 0
    );
    const totalSnapshotOverhead = Object.values(snapshotTree).reduce((sum, tree) => 
      sum + (tree.storage_analytics?.total_snapshot_overhead || 0), 0
    );
    const totalActualData = Object.values(snapshotTree).reduce((sum, tree) => 
      sum + (tree.storage_analytics?.actual_data_size || 0), 0
    );
    
    const inefficientVolumes = Object.values(snapshotTree).filter(tree => 
      (tree.storage_analytics?.snapshot_efficiency_ratio || 0) > 0.3
    ).length;

    return {
      totalLogicalSize,
      totalSnapshotOverhead,
      totalActualData,
      inefficientVolumes,
      overallEfficiency: totalLogicalSize > 0 ? totalSnapshotOverhead / totalLogicalSize : 0
    };
  }, [snapshotTree]);

  const toggleVolumeExpansion = (volumeId: string) => {
    const newExpanded = new Set(expandedVolumes);
    if (newExpanded.has(volumeId)) {
      newExpanded.delete(volumeId);
    } else {
      newExpanded.add(volumeId);
    }
    setExpandedVolumes(newExpanded);
  };

  const formatSize = (bytes: number) => {
    const gb = bytes / (1024 * 1024 * 1024);
    return `${gb.toFixed(1)}GB`;
  };

  const formatTime = (timeString: string) => {
    return new Date(timeString).toLocaleString();
  };

  const getSnapshotTypeIcon = (type: string) => {
    switch (type) {
      case 'Bdev': return <Camera className="w-4 h-4 text-blue-600" />;
      case 'LvolClone': return <Copy className="w-4 h-4 text-green-600" />;
      case 'External': return <Download className="w-4 h-4 text-purple-600" />;
      default: return <Database className="w-4 h-4 text-gray-600" />;
    }
  };

  const renderActiveView = () => {
    switch (activeView) {
      case 'list':
        return (
          <SnapshotsListView
            snapshots={filteredSnapshots}
            onSnapshotSelect={setSelectedSnapshot}
            formatSize={formatSize}
            formatTime={formatTime}
            getSnapshotTypeIcon={getSnapshotTypeIcon}
          />
        );
      case 'tree':
        return (
          <EnhancedSnapshotsTreeView
            snapshotTree={snapshotTree}
            expandedVolumes={expandedVolumes}
            onToggleVolumeExpansion={toggleVolumeExpansion}
            formatSize={formatSize}
            formatTime={formatTime}
            getSnapshotTypeIcon={getSnapshotTypeIcon}
          />
        );
      case 'storage':
        return (
          <SnapshotStorageView
            snapshots={filteredSnapshots}
            snapshotTree={snapshotTree}
            onSnapshotSelect={setSelectedSnapshot}
            formatSize={formatSize}
          />
        );
      case 'timeline':
        return (
          <SnapshotTimelineView
            selectedVolume={topologyVolume}
            onVolumeChange={setTopologyVolume}
            availableVolumes={availableVolumes}
            volumeNames={volumeNames}
          />
        );
      default:
        return null;
    }
  };

  if (loading) {
    return <TabSkeleton />;
  }

  return (
    <div className="space-y-6">
      {/* Header with Storage Insights */}
      <div className="bg-white rounded-lg shadow p-6">
        <div className="flex items-center justify-between mb-6">
          <div className="flex items-center gap-3">
            <Camera className="w-8 h-8 text-blue-600" />
            <div>
              <h2 className="text-page-title text-gray-900">Volume Snapshots</h2>
              <p className="text-sm text-gray-600">Storage-aware snapshot management</p>
            </div>
          </div>
          <IconButton
            icon={RefreshCw}
            aria-label="Refresh snapshots"
            onClick={fetchSnapshotData}
            disabled={refreshing}
            iconClass={refreshing ? 'animate-spin' : ''}
          />
        </div>

        {error && (
          <div className="mb-6 flex items-center gap-2 p-3 bg-red-50 border border-red-200 text-red-700 rounded-md text-sm">
            <AlertTriangle className="w-4 h-4 flex-shrink-0" />
            <span>Could not load snapshot data: {error}. Showing last known data.</span>
          </div>
        )}

        {/* Enhanced Statistics with Storage Information */}
        <div className="grid grid-cols-2 md:grid-cols-6 gap-4">
          <div className="bg-blue-50 rounded-lg p-4 text-center">
            <Database className="w-6 h-6 text-blue-600 mx-auto mb-2" />
            <p className="text-stat text-blue-600">{snapshots.length}</p>
            <p className="text-sm text-gray-600">Total Snapshots</p>
          </div>
          <div className="bg-green-50 rounded-lg p-4 text-center">
            <CheckCircle className="w-6 h-6 text-green-600 mx-auto mb-2" />
            <p className="text-stat text-green-600">
              {snapshots.filter(s => s.ready_to_use).length}
            </p>
            <p className="text-sm text-gray-600">Ready to Use</p>
          </div>
          <div className="bg-purple-50 rounded-lg p-4 text-center">
            <Layers className="w-6 h-6 text-purple-600 mx-auto mb-2" />
            <p className="text-stat text-purple-600">
              {snapshots.reduce((sum, s) => sum + (s.replica_bdev_details || []).length, 0)}
            </p>
            <p className="text-sm text-gray-600">Replica Snapshots</p>
          </div>
          <div className="bg-indigo-50 rounded-lg p-4 text-center">
            <HardDrive className="w-6 h-6 text-indigo-600 mx-auto mb-2" />
            <p className="text-stat text-indigo-600">
              {formatSize(storageInsights.totalLogicalSize)}
            </p>
            <p className="text-sm text-gray-600">Logical Storage</p>
          </div>
          <div className="bg-orange-50 rounded-lg p-4 text-center">
            <BarChart3 className="w-6 h-6 text-orange-600 mx-auto mb-2" />
            <p className="text-stat text-orange-600">
              {formatSize(storageInsights.totalSnapshotOverhead)}
            </p>
            <p className="text-sm text-gray-600">Snapshot Overhead</p>
          </div>
          <div className="bg-yellow-50 rounded-lg p-4 text-center">
            <TrendingUp className="w-6 h-6 text-yellow-600 mx-auto mb-2" />
            <p className={`text-stat ${
              storageInsights.overallEfficiency < 0.1 ? 'text-green-600' :
              storageInsights.overallEfficiency < 0.3 ? 'text-yellow-600' : 'text-red-600'
            }`}>
              {(storageInsights.overallEfficiency * 100).toFixed(1)}%
            </p>
            <p className="text-sm text-gray-600">Overhead Ratio</p>
          </div>
        </div>

        {/* Storage Efficiency Alert */}
        {storageInsights.inefficientVolumes > 0 && (
          <div className="mt-4 p-4 bg-red-50 border border-red-200 rounded-lg">
            <div className="flex items-center gap-2">
              <AlertTriangle className="w-5 h-5 text-red-600" />
              <span className="font-medium text-red-800">
                Storage Efficiency Warning
              </span>
            </div>
            <p className="text-sm text-red-700 mt-1">
              {storageInsights.inefficientVolumes} volume{storageInsights.inefficientVolumes !== 1 ? 's have' : ' has'} high 
              snapshot overhead (&gt;30%). Switch to Storage View for detailed analysis and recommendations.
            </p>
          </div>
        )}
      </div>

      {/* View Toggle and Filters */}
      <div className="bg-white rounded-lg shadow">
        <div className="p-4 border-b border-gray-200">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-4">
              {/* Enhanced View Toggle */}
              <div className="flex border border-gray-300 rounded-md overflow-hidden">
                <button
                  onClick={() => setActiveView('storage')}
                  className={`px-4 py-2 text-sm font-medium ${
                    activeView === 'storage'
                      ? 'bg-blue-600 text-white'
                      : 'bg-white text-gray-700 hover:bg-gray-50'
                  }`}
                >
                  <BarChart3 className="w-4 h-4 mr-2 inline" />
                  Storage Analysis
                </button>
                <button
                  onClick={() => setActiveView('list')}
                  className={`px-4 py-2 text-sm font-medium border-l border-gray-300 ${
                    activeView === 'list'
                      ? 'bg-blue-600 text-white'
                      : 'bg-white text-gray-700 hover:bg-gray-50'
                  }`}
                >
                  <FileText className="w-4 h-4 mr-2 inline" />
                  List View
                </button>
                <button
                  onClick={() => setActiveView('tree')}
                  className={`px-4 py-2 text-sm font-medium border-l border-gray-300 ${
                    activeView === 'tree'
                      ? 'bg-blue-600 text-white'
                      : 'bg-white text-gray-700 hover:bg-gray-50'
                  }`}
                >
                  <GitBranch className="w-4 h-4 mr-2 inline" />
                  Tree View
                </button>
                <button
                  onClick={() => setActiveView('timeline')}
                  className={`px-4 py-2 text-sm font-medium border-l border-gray-300 ${
                    activeView === 'timeline'
                      ? 'bg-blue-600 text-white'
                      : 'bg-white text-gray-700 hover:bg-gray-50'
                  }`}
                >
                  <TrendingUp className="w-4 h-4 mr-2 inline" />
                  Timeline
                </button>
              </div>

              <span className="text-sm text-gray-500">
                Showing {filteredSnapshots.length} of {snapshots.length} snapshots
              </span>
            </div>

            <Button
              variant="link"
              icon={Filter}
              className="text-gray-600 hover:text-gray-800"
              onClick={() => setShowFilters(!showFilters)}
            >
              Filters
              <ChevronDown className={`w-4 h-4 transition-transform ${showFilters ? 'rotate-180' : ''}`} />
            </Button>
          </div>
        </div>

        {/* Filters (only show for list/tree views) */}
        {showFilters && (activeView === 'list' || activeView === 'tree') && (
          <div className="p-4 border-b border-gray-200 bg-gray-50">
            <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
              {/* Search */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">
                  Search
                </label>
                <div className="relative">
                  <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
                  <input
                    type="text"
                    placeholder="Search snapshots..."
                    value={searchTerm}
                    onChange={(e) => setSearchTerm(e.target.value)}
                    className="w-full pl-10 pr-4 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
                  />
                </div>
              </div>

              {/* Type Filter */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">
                  Snapshot Type
                </label>
                <select
                  value={typeFilter}
                  onChange={(e) => setTypeFilter(e.target.value as SnapshotTypeFilter)}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="all">All Types</option>
                  <option value="Bdev">Standard Snapshots</option>
                  <option value="LvolClone">Clones</option>
                  <option value="External">External</option>
                </select>
              </div>

              {/* Volume Filter */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">
                  Source Volume
                </label>
                <select
                  value={volumeFilter}
                  onChange={(e) => setVolumeFilter(e.target.value)}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="all">All Volumes</option>
                  {availableVolumes.map(volume => (
                    <option key={volume} value={volume}>{volume}</option>
                  ))}
                </select>
              </div>
            </div>

            {/* Active Filters Summary */}
            {(searchTerm || typeFilter !== 'all' || volumeFilter !== 'all') && (
              <div className="mt-4 pt-4 border-t border-gray-200">
                <div className="flex items-center gap-2 text-sm">
                  <span className="text-gray-600">Active filters:</span>
                  {searchTerm && (
                    <span className="px-2 py-1 bg-blue-100 text-blue-800 rounded-full text-xs">
                      Search: "{searchTerm}"
                    </span>
                  )}
                  {typeFilter !== 'all' && (
                    <span className="px-2 py-1 bg-green-100 text-green-800 rounded-full text-xs">
                      Type: {typeFilter}
                    </span>
                  )}
                  {volumeFilter !== 'all' && (
                    <span className="px-2 py-1 bg-purple-100 text-purple-800 rounded-full text-xs">
                      Volume: {volumeFilter}
                    </span>
                  )}
                  <button
                    onClick={() => {
                      setSearchTerm('');
                      setTypeFilter('all');
                      setVolumeFilter('all');
                    }}
                    className="px-2 py-1 bg-gray-100 text-gray-700 rounded-full text-xs hover:bg-gray-200"
                  >
                    Clear all
                  </button>
                </div>
              </div>
            )}
          </div>
        )}

        {/* Content */}
        <div className="p-6">
          {renderActiveView()}
        </div>
      </div>

      {/* Snapshot Detail Modal — re-derived from the fetched list so a
          deep-linked id waits for data instead of crashing */}
      {selectedSnapshot && (
        <SnapshotDetailModal
          snapshot={selectedSnapshot}
          onClose={() => setSelectedSnapshot(null)}
          formatSize={formatSize}
          formatTime={formatTime}
          getSnapshotTypeIcon={getSnapshotTypeIcon}
        />
      )}

      {/* Information Panel with Storage Focus */}
      <div className="bg-blue-50 border border-blue-200 rounded-lg p-6">
        <div className="flex items-start gap-3">
          <div className="w-6 h-6 text-blue-600 mt-1 flex-shrink-0">
            <svg fill="currentColor" viewBox="0 0 20 20">
              <path fillRule="evenodd" d="M18 10a8 8 0 11-16 0 8 8 0 0116 0zm-7-4a1 1 0 11-2 0 1 1 0 012 0zM9 9a1 1 0 000 2v3a1 1 0 001 1h1a1 1 0 100-2v-3a1 1 0 00-1-1H9z" clipRule="evenodd" />
            </svg>
          </div>
          <div>
            <h4 className="font-medium text-blue-900 mb-2">SPDK Storage-Aware Snapshot Management</h4>
            <div className="text-sm text-blue-800 space-y-2">
              <p>
                <strong>Storage Analysis:</strong> Track actual storage consumption vs logical snapshot size. 
                Monitor snapshot overhead and identify inefficient storage usage patterns.
              </p>
              <p>
                <strong>Relationship Mapping:</strong> Visualize parent-child relationships in snapshot chains. 
                Understand how clone operations share storage with their source snapshots.
              </p>
              <p>
                <strong>Multi-Replica Architecture:</strong> Each snapshot creates individual copies across 
                replica nodes for high availability, with detailed per-replica storage tracking.
              </p>
              <p>
                <strong>Storage Optimization:</strong> Use the Storage Analysis view to identify volumes with 
                high snapshot overhead and get actionable recommendations for cleanup.
              </p>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
