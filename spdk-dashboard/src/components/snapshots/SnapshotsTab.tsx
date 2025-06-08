import React, { useState, useEffect, useMemo } from 'react';
import { 
  Camera, RefreshCw, Search, Filter, ChevronDown, FileText, 
  GitBranch, Network, CheckCircle, Layers, Database, Copy, Download
} from 'lucide-react';
import { SnapshotsListView } from './SnapshotsListView';
import { SnapshotsTreeView } from './SnapshotsTreeView';
import { SnapshotDetailModal } from './SnapshotDetailModal';
import type { 
  SnapshotDetails, 
  SnapshotTreeNode, 
  SnapshotTypeFilter, 
  SnapshotViewMode 
} from './types';

export const SnapshotsTab: React.FC = () => {
  const [snapshots, setSnapshots] = useState<SnapshotDetails[]>([]);
  const [snapshotTree, setSnapshotTree] = useState<Record<string, SnapshotTreeNode>>({});
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [activeView, setActiveView] = useState<SnapshotViewMode>('list');
  const [searchTerm, setSearchTerm] = useState('');
  const [typeFilter, setTypeFilter] = useState<SnapshotTypeFilter>('all');
  const [volumeFilter, setVolumeFilter] = useState<string>('all');
  const [selectedSnapshot, setSelectedSnapshot] = useState<SnapshotDetails | null>(null);
  const [expandedVolumes, setExpandedVolumes] = useState<Set<string>>(new Set());
  const [showFilters, setShowFilters] = useState(false);

  useEffect(() => {
    fetchSnapshotData();
  }, []);

  const fetchSnapshotData = async () => {
    try {
      setRefreshing(true);
      
      // Fetch both list and tree data with better error handling
      const [snapshotsResponse, treeResponse] = await Promise.allSettled([
        fetch('/api/snapshots').catch(() => null),
        fetch('/api/snapshots/tree').catch(() => null)
      ]);

      // Handle snapshots response
      if (snapshotsResponse.status === 'fulfilled' && snapshotsResponse.value) {
        const response = snapshotsResponse.value;
        if (response.ok) {
          const contentType = response.headers.get('content-type');
          if (contentType && contentType.includes('application/json')) {
            const snapshotsData = await response.json();
            setSnapshots(snapshotsData);
          } else {
            console.warn('Snapshots API returned non-JSON response, using mock data');
            setSnapshots(mockSnapshots);
          }
        } else {
          console.warn('Snapshots API returned error status:', response.status, 'using mock data');
          setSnapshots(mockSnapshots);
        }
      } else {
        console.warn('Failed to fetch snapshots, using mock data');
        setSnapshots(mockSnapshots);
      }

      // Handle tree response
      if (treeResponse.status === 'fulfilled' && treeResponse.value) {
        const response = treeResponse.value;
        if (response.ok) {
          const contentType = response.headers.get('content-type');
          if (contentType && contentType.includes('application/json')) {
            const treeData = await response.json();
            setSnapshotTree(treeData);
          } else {
            console.warn('Snapshot tree API returned non-JSON response, using mock data');
            setSnapshotTree(mockSnapshotTree);
          }
        } else {
          console.warn('Snapshot tree API returned error status:', response.status, 'using mock data');
          setSnapshotTree(mockSnapshotTree);
        }
      } else {
        console.warn('Failed to fetch snapshot tree, using mock data');
        setSnapshotTree(mockSnapshotTree);
      }
    } catch (error) {
      console.warn('Failed to fetch snapshot data:', error);
      // Use mock data for development
      setSnapshots(mockSnapshots);
      setSnapshotTree(mockSnapshotTree);
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  };

  // Mock data for development
  const mockSnapshots: SnapshotDetails[] = [
    {
      snapshot_id: 'snap-postgres-20250101-120000',
      source_volume_id: 'pvc-postgres-data',
      creation_time: '2025-01-01T12:00:00Z',
      ready_to_use: true,
      size_bytes: 107374182400,
      snapshot_type: 'Bdev',
      replica_bdev_details: [
        {
          node: 'worker-node-1',
          name: 'snap_postgres_replica_0',
          aliases: ['postgres_snap_primary'],
          driver: 'lvol',
          snapshot_source_bdev: 'postgres_replica_0'
        },
        {
          node: 'worker-node-2', 
          name: 'snap_postgres_replica_1',
          aliases: ['postgres_snap_secondary'],
          driver: 'lvol',
          snapshot_source_bdev: 'postgres_replica_1'
        },
        {
          node: 'worker-node-3',
          name: 'snap_postgres_replica_2',
          aliases: ['postgres_snap_tertiary'],
          driver: 'lvol',
          snapshot_source_bdev: 'postgres_replica_2'
        }
      ]
    },
    {
      snapshot_id: 'snap-redis-20250101-140000',
      source_volume_id: 'pvc-redis-cache',
      creation_time: '2025-01-01T14:00:00Z',
      ready_to_use: true,
      size_bytes: 53687091200,
      snapshot_type: 'Bdev',
      replica_bdev_details: [
        {
          node: 'worker-node-1',
          name: 'snap_redis_replica_0',
          aliases: ['redis_snap_primary'],
          driver: 'lvol',
          snapshot_source_bdev: 'redis_replica_0'
        },
        {
          node: 'worker-node-2',
          name: 'snap_redis_replica_1', 
          aliases: ['redis_snap_secondary'],
          driver: 'lvol',
          snapshot_source_bdev: 'redis_replica_1'
        }
      ]
    },
    {
      snapshot_id: 'snap-mysql-clone-20250102-090000',
      source_volume_id: 'pvc-mysql-data',
      creation_time: '2025-01-02T09:00:00Z',
      ready_to_use: true,
      size_bytes: 85899345920,
      snapshot_type: 'LvolClone',
      clone_source_snapshot_id: 'snap-mysql-20250101-180000',
      replica_bdev_details: [
        {
          node: 'worker-node-1',
          name: 'clone_mysql_replica_0',
          aliases: ['mysql_clone_primary'],
          driver: 'lvol',
          snapshot_source_bdev: 'mysql_replica_0'
        },
        {
          node: 'worker-node-3',
          name: 'clone_mysql_replica_1',
          aliases: ['mysql_clone_secondary'],
          driver: 'lvol',
          snapshot_source_bdev: 'mysql_replica_1'
        }
      ]
    }
  ];

  const mockSnapshotTree: Record<string, SnapshotTreeNode> = {
    'pvc-postgres-data': {
      volume_name: 'postgres-data-pvc',
      volume_id: 'pvc-postgres-data',
      volume_size: 107374182400,
      snapshots: [
        {
          snapshot_id: 'snap-postgres-20250101-120000',
          snapshot_type: 'Bdev',
          creation_time: '2025-01-01T12:00:00Z',
          ready_to_use: true,
          size_bytes: 107374182400,
          replica_snapshots: [
            {
              node: 'worker-node-1',
              bdev_name: 'snap_postgres_replica_0',
              source_bdev: 'postgres_replica_0',
              disk: 'nvme0n1'
            },
            {
              node: 'worker-node-2',
              bdev_name: 'snap_postgres_replica_1', 
              source_bdev: 'postgres_replica_1',
              disk: 'nvme1n1'
            },
            {
              node: 'worker-node-3',
              bdev_name: 'snap_postgres_replica_2',
              source_bdev: 'postgres_replica_2',
              disk: 'nvme2n1'
            }
          ],
          children: []
        }
      ]
    },
    'pvc-mysql-data': {
      volume_name: 'mysql-data-pvc',
      volume_id: 'pvc-mysql-data',
      volume_size: 85899345920,
      snapshots: [
        {
          snapshot_id: 'snap-mysql-clone-20250102-090000',
          snapshot_type: 'LvolClone',
          creation_time: '2025-01-02T09:00:00Z',
          ready_to_use: true,
          size_bytes: 85899345920,
          replica_snapshots: [
            {
              node: 'worker-node-1',
              bdev_name: 'clone_mysql_replica_0',
              source_bdev: 'mysql_replica_0',
              disk: 'nvme0n1'
            },
            {
              node: 'worker-node-3',
              bdev_name: 'clone_mysql_replica_1',
              source_bdev: 'mysql_replica_1',
              disk: 'nvme2n1'
            }
          ],
          children: []
        }
      ]
    }
  };

  // Filter and search logic
  const filteredSnapshots = useMemo(() => {
    let result = snapshots;

    // Search filter
    if (searchTerm) {
      const searchLower = searchTerm.toLowerCase();
      result = result.filter(snap => 
        snap.snapshot_id.toLowerCase().includes(searchLower) ||
        snap.source_volume_id.toLowerCase().includes(searchLower) ||
        snap.replica_bdev_details.some(replica => 
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

  // Get unique volumes for filter dropdown
  const availableVolumes = useMemo(() => {
    return Array.from(new Set(snapshots.map(snap => snap.source_volume_id)));
  }, [snapshots]);

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
          <SnapshotsTreeView
            snapshotTree={snapshotTree}
            expandedVolumes={expandedVolumes}
            onToggleVolumeExpansion={toggleVolumeExpansion}
            formatSize={formatSize}
            formatTime={formatTime}
            getSnapshotTypeIcon={getSnapshotTypeIcon}
          />
        );
      default:
        return null;
    }
  };

  if (loading) {
    return (
      <div className="flex justify-center items-center h-64">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
        <span className="ml-3 text-lg">Loading snapshots...</span>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="bg-white rounded-lg shadow p-6">
        <div className="flex items-center justify-between mb-6">
          <div className="flex items-center gap-3">
            <Camera className="w-8 h-8 text-blue-600" />
            <div>
              <h2 className="text-2xl font-bold text-gray-900">Volume Snapshots</h2>
              <p className="text-sm text-gray-600">
                {snapshots.length} snapshot{snapshots.length !== 1 ? 's' : ''} ready to use
              </p>
            </div>
          </div>
          <button
            onClick={fetchSnapshotData}
            disabled={refreshing}
            className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md disabled:opacity-50"
          >
            <RefreshCw className={`w-5 h-5 ${refreshing ? 'animate-spin' : ''}`} />
          </button>
        </div>

        {/* Statistics Cards */}
        <div className="grid grid-cols-1 md:grid-cols-4 gap-4">
          <div className="bg-blue-50 rounded-lg p-4 text-center">
            <Camera className="w-6 h-6 text-blue-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-blue-600">{snapshots.length}</p>
            <p className="text-sm text-gray-600">Total Snapshots</p>
          </div>
          <div className="bg-green-50 rounded-lg p-4 text-center">
            <CheckCircle className="w-6 h-6 text-green-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-green-600">
              {snapshots.filter(s => s.ready_to_use).length}
            </p>
            <p className="text-sm text-gray-600">Ready to Use</p>
          </div>
          <div className="bg-purple-50 rounded-lg p-4 text-center">
            <Layers className="w-6 h-6 text-purple-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-purple-600">
              {snapshots.reduce((sum, s) => sum + s.replica_bdev_details.length, 0)}
            </p>
            <p className="text-sm text-gray-600">Replica Snapshots</p>
          </div>
          <div className="bg-indigo-50 rounded-lg p-4 text-center">
            <Database className="w-6 h-6 text-indigo-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-indigo-600">
              {formatSize(snapshots.reduce((sum, s) => sum + s.size_bytes, 0))}
            </p>
            <p className="text-sm text-gray-600">Total Size</p>
          </div>
        </div>
      </div>

      {/* View Toggle and Filters */}
      <div className="bg-white rounded-lg shadow">
        <div className="p-4 border-b border-gray-200">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-4">
              {/* View Toggle */}
              <div className="flex border border-gray-300 rounded-md overflow-hidden">
                <button
                  onClick={() => setActiveView('list')}
                  className={`px-4 py-2 text-sm font-medium ${
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
              </div>

              <span className="text-sm text-gray-500">
                Showing {filteredSnapshots.length} of {snapshots.length} snapshots
              </span>
            </div>

            <button
              onClick={() => setShowFilters(!showFilters)}
              className="flex items-center gap-2 text-sm text-gray-600 hover:text-gray-800"
            >
              <Filter className="w-4 h-4" />
              Filters
              <ChevronDown className={`w-4 h-4 transition-transform ${showFilters ? 'rotate-180' : ''}`} />
            </button>
          </div>
        </div>

        {/* Filters */}
        {showFilters && (
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

      {/* Snapshot Detail Modal */}
      {selectedSnapshot && (
        <SnapshotDetailModal
          snapshot={selectedSnapshot}
          onClose={() => setSelectedSnapshot(null)}
          formatSize={formatSize}
          formatTime={formatTime}
          getSnapshotTypeIcon={getSnapshotTypeIcon}
        />
      )}

      {/* Information Panel */}
      <div className="bg-blue-50 border border-blue-200 rounded-lg p-6">
        <div className="flex items-start gap-3">
          <div className="w-6 h-6 text-blue-600 mt-1 flex-shrink-0">
            <svg fill="currentColor" viewBox="0 0 20 20">
              <path fillRule="evenodd" d="M18 10a8 8 0 11-16 0 8 8 0 0116 0zm-7-4a1 1 0 11-2 0 1 1 0 012 0zM9 9a1 1 0 000 2v3a1 1 0 001 1h1a1 1 0 100-2v-3a1 1 0 00-1-1H9z" clipRule="evenodd" />
            </svg>
          </div>
          <div>
            <h4 className="font-medium text-blue-900 mb-2">SPDK Multi-Replica Snapshot Architecture</h4>
            <div className="text-sm text-blue-800 space-y-2">
              <p>
                <strong>High Availability:</strong> Each volume snapshot creates individual snapshots 
                across all replica nodes, ensuring no single point of failure.
              </p>
              <p>
                <strong>Atomic Consistency:</strong> All replica snapshots are created simultaneously 
                to guarantee data consistency across the entire volume.
              </p>
              <p>
                <strong>Independent Recovery:</strong> Each replica snapshot can be restored independently, 
                providing flexible recovery options even in multi-node failure scenarios.
              </p>
              <p>
                <strong>Multiple Views:</strong> Use List view for detailed information, Tree view for 
                hierarchical organization, and Topology view for visual architecture understanding.
              </p>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
