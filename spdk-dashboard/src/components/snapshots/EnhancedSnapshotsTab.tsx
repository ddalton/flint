import React, { useState, useEffect, useMemo } from 'react';
import { 
  Camera, RefreshCw, Search, Filter, ChevronDown, FileText, 
  GitBranch, HardDrive, BarChart3, CheckCircle, Layers, Database, 
  Copy, Download, AlertTriangle, TrendingUp
} from 'lucide-react';
import { SnapshotsListView } from './SnapshotsListView';
import { EnhancedSnapshotsTreeView } from './EnhancedSnapshotsTreeView';
import { SnapshotStorageView } from './SnapshotStorageView';
import { SnapshotsTopologyView } from './SnapshotsTopologyView';
import { SnapshotDetailModal } from './SnapshotDetailModal';
import { useOperations } from '../../contexts/OperationsContext';
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
  const [activeView, setActiveView] = useState<SnapshotViewMode>('storage'); // Default to storage view
  const [searchTerm, setSearchTerm] = useState('');
  const [typeFilter, setTypeFilter] = useState<SnapshotTypeFilter>('all');
  const [volumeFilter, setVolumeFilter] = useState<string>('all');
  const [selectedSnapshot, setSelectedSnapshot] = useState<SnapshotDetails | null>(null);
  const [expandedVolumes, setExpandedVolumes] = useState<Set<string>>(new Set());
  const [showFilters, setShowFilters] = useState(false);
  const { setDialogVisible } = useOperations();

  useEffect(() => {
    setDialogVisible(selectedSnapshot !== null);
  }, [selectedSnapshot, setDialogVisible]);

  // Get unique volumes for filter dropdown
  const availableVolumes = useMemo(() => {
    return Array.from(new Set(snapshots.map(snap => snap.source_volume_id)));
  }, [snapshots]);

  const [topologyVolume, setTopologyVolume] = useState<string>('all');

  useEffect(() => {
    fetchSnapshotData();
  }, []);

  const fetchSnapshotData = async () => {
    try {
      setRefreshing(true);
      
      // Fetch both list and tree data with enhanced storage information
      const [snapshotsResponse, treeResponse] = await Promise.all([
        fetch('/api/snapshots'),
        fetch('/api/snapshots/tree')
      ]);

      const snapshotsContentType = snapshotsResponse.headers.get("content-type");
      if (snapshotsResponse.ok && snapshotsContentType && snapshotsContentType.indexOf("application/json") !== -1) {
        const snapshotsData = await snapshotsResponse.json();
        // Transform backend data to include storage relationships
        const enhancedSnapshots = enhanceSnapshotsWithRelationships(snapshotsData);
        setSnapshots(enhancedSnapshots);
      } else {
        setSnapshots(mockSnapshotsWithStorage);
      }

      const treeContentType = treeResponse.headers.get("content-type");
      if (treeResponse.ok && treeContentType && treeContentType.indexOf("application/json") !== -1) {
        const treeData = await treeResponse.json();
        // Transform tree data to include storage analytics
        const enhancedTree = enhanceTreeWithStorageAnalytics(treeData);
        setSnapshotTree(enhancedTree);
      } else {
        // Use enhanced mock tree data
        setSnapshotTree(mockSnapshotTreeWithStorage);
      }
    } catch (error) {
      console.error('Failed to fetch snapshot data:', error);
      // Use enhanced mock data for development
      setSnapshots(mockSnapshotsWithStorage);
      setSnapshotTree(mockSnapshotTreeWithStorage);
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  };

  // Enhanced mock data with storage consumption information
  const mockSnapshotsWithStorage: SnapshotDetails[] = [
    {
      snapshot_id: 'snap-postgres-20250101-120000',
      source_volume_id: 'pvc-postgres-data',
      creation_time: '2025-01-01T12:00:00Z',
      ready_to_use: true,
      size_bytes: 107374182400,
      snapshot_type: 'Bdev',
      storage_consumption: {
        consumed_bytes: 15728640000, // 14.6GB actual consumption
        cluster_size: 4194304, // 4MB clusters
        allocated_clusters: 3750,
        actual_storage_overhead: 8354758400, // ~7.8GB overhead beyond logical data
        compression_ratio: 1.2,
        deduplication_savings: 2147483648 // 2GB saved through dedup
      },
      replica_bdev_details: [
        {
          node: 'worker-node-1',
          name: 'snap_postgres_replica_0',
          aliases: ['postgres_snap_primary'],
          driver: 'lvol',
          snapshot_source_bdev: 'postgres_replica_0',
          storage_info: {
            consumed_bytes: 5242880000,
            cluster_size: 4194304,
            allocated_clusters: 1250
          }
        },
        {
          node: 'worker-node-2', 
          name: 'snap_postgres_replica_1',
          aliases: ['postgres_snap_secondary'],
          driver: 'lvol',
          snapshot_source_bdev: 'postgres_replica_1',
          storage_info: {
            consumed_bytes: 5242880000,
            cluster_size: 4194304,
            allocated_clusters: 1250
          }
        },
        {
          node: 'worker-node-3',
          name: 'snap_postgres_replica_2',
          aliases: ['postgres_snap_tertiary'],
          driver: 'lvol',
          snapshot_source_bdev: 'postgres_replica_2',
          storage_info: {
            consumed_bytes: 5242880000,
            cluster_size: 4194304,
            allocated_clusters: 1250
          }
        }
      ]
    },
    {
      snapshot_id: 'snap-redis-20241231-140000', // Has a parent snapshot
      source_volume_id: 'pvc-redis-cache',
      creation_time: '2024-12-31T14:00:00Z',
      ready_to_use: true,
      size_bytes: 53687091200,
      snapshot_type: 'Bdev',
      storage_consumption: {
        consumed_bytes: 10737418240,
        cluster_size: 4194304,
        allocated_clusters: 2560,
        actual_storage_overhead: 16106127360, // ~15GB overhead - needs attention!
        compression_ratio: 0.8, // Poor compression
        deduplication_savings: 1073741824 // 1GB dedup
      },
      replica_bdev_details: [
        {
          node: 'worker-node-1',
          name: 'snap_redis_replica_0',
          aliases: ['redis_snap_primary'],
          driver: 'lvol',
          snapshot_source_bdev: 'redis_replica_0',
          storage_info: {
            consumed_bytes: 10737418240,
            cluster_size: 4194304,
            allocated_clusters: 2560
          }
        },
        {
          node: 'worker-node-2',
          name: 'snap_redis_replica_1', 
          aliases: ['redis_snap_secondary'],
          driver: 'lvol',
          snapshot_source_bdev: 'redis_replica_1',
          storage_info: {
            consumed_bytes: 10737418240,
            cluster_size: 4194304,
            allocated_clusters: 2560
          }
        }
      ],
      parent_snapshot_id: 'snap-redis-20250101-140000',
    },
    {
      snapshot_id: 'snap-redis-20250101-140000',
      source_volume_id: 'pvc-redis-cache',
      creation_time: '2025-01-01T14:00:00Z',
      ready_to_use: true,
      size_bytes: 53687091200,
      snapshot_type: 'Bdev',
      storage_consumption: {
        consumed_bytes: 21474836480, // 20GB actual consumption (high overhead!)
        cluster_size: 4194304,
        allocated_clusters: 5120,
        actual_storage_overhead: 16106127360, // ~15GB overhead - needs attention!
        compression_ratio: 0.8, // Poor compression
        deduplication_savings: 1073741824 // 1GB dedup
      },
      replica_bdev_details: [
        {
          node: 'worker-node-1',
          name: 'snap_redis_replica_0',
          aliases: ['redis_snap_primary'],
          driver: 'lvol',
          snapshot_source_bdev: 'redis_replica_0',
          storage_info: {
            consumed_bytes: 10737418240,
            cluster_size: 4194304,
            allocated_clusters: 2560
          }
        },
        {
          node: 'worker-node-2',
          name: 'snap_redis_replica_1', 
          aliases: ['redis_snap_secondary'],
          driver: 'lvol',
          snapshot_source_bdev: 'redis_replica_1',
          storage_info: {
            consumed_bytes: 10737418240,
            cluster_size: 4194304,
            allocated_clusters: 2560
          }
        }
      ],
    },

    {
      snapshot_id: 'snap-mysql-clone-20250102-090000',
      source_volume_id: 'pvc-mysql-data',
      creation_time: '2025-01-02T09:00:00Z',
      ready_to_use: true,
      size_bytes: 85899345920,
      snapshot_type: 'LvolClone',
      clone_source_snapshot_id: 'snap-mysql-20250101-180000',
      storage_consumption: {
        consumed_bytes: 4294967296, // 4GB - very efficient clone
        cluster_size: 4194304,
        allocated_clusters: 1024,
        actual_storage_overhead: 85895051264, // Most is shared with parent
        compression_ratio: 2.1, // Excellent compression
        deduplication_savings: 81604378624 // Massive dedup from parent
      },
      replica_bdev_details: [
        {
          node: 'worker-node-1',
          name: 'clone_mysql_replica_0',
          aliases: ['mysql_clone_primary'],
          driver: 'lvol',
          snapshot_source_bdev: 'mysql_replica_0',
          storage_info: {
            consumed_bytes: 2147483648,
            cluster_size: 4194304,
            allocated_clusters: 512
          }
        },
        {
          node: 'worker-node-3',
          name: 'clone_mysql_replica_1',
          aliases: ['mysql_clone_secondary'],
          driver: 'lvol',
          snapshot_source_bdev: 'mysql_replica_1',
          storage_info: {
            consumed_bytes: 2147483648,
            cluster_size: 4194304,
            allocated_clusters: 512
          }
        }
      ],
    }
  ];

  const mockSnapshotTreeWithStorage: Record<string, SnapshotTreeNode> = {
    'pvc-redis-cache': {
      volume_name: 'redis-cache-pvc',
      volume_id: 'pvc-redis-cache',
      volume_size: 53687091200, // 50GB logical
      snapshot_chain: {
        active_lvol: 'lvs_redis/redis-active-lvol',
        chain_depth: 4,
        snapshots: [
          {
            bdev_name: 'lvs_redis/redis-active-lvol',
            snapshot_id: 'current-active-volume',
            details: { creation_time: '2025-01-02T00:00:00Z' },
            children: [
              {
                bdev_name: 'snap_redis_replica_0',
                snapshot_id: 'snap-redis-20250101-140000',
                details: { creation_time: '2025-01-01T14:00:00Z' },
                children: [
                  {
                    bdev_name: 'snap_redis_old_replica_0',
                    snapshot_id: 'snap-redis-20241231-140000',
                    details: { creation_time: '2024-12-31T14:00:00Z' },
                    children: [],
                    storage_info: {
                      consumed_bytes: 10737418240,
                      cluster_size: 4194304,
                      allocated_clusters: 2560
                    },
                    creation_order: 2,
                    is_active_volume: false
                  }
                ],
                storage_info: {
                  consumed_bytes: 21474836480,
                  cluster_size: 4194304,
                  allocated_clusters: 5120
                },
                creation_order: 1,
                is_active_volume: false
              }
            ],
            storage_info: {
              consumed_bytes: 17179869184, // 16GB active data
              cluster_size: 4194304,
              allocated_clusters: 4096
            },
            creation_order: 0,
            is_active_volume: true
          }
        ],
        error: undefined
      },
      storage_analytics: {
        total_volume_size: 53687091200,
        actual_data_size: 17179869184, // Only 16GB actual data
        total_snapshot_overhead: 32212254720, // ~30GB in snapshots - HIGH!
        snapshot_efficiency_ratio: 0.60, // 60% overhead - needs attention!
        storage_breakdown: {
          active_volume_consumption: 17179869184,
          snapshot_consumption: 32212254720,
          metadata_overhead: 268435456, // 256MB metadata
          free_space_in_volume: 4026531840 // ~3.8GB free
        },
        recommendations: [
          'HIGH PRIORITY: 60% snapshot overhead detected',
          'Delete snapshots older than 7 days immediately',
          'Review snapshot retention policy for high-churn workloads',
          'Consider reducing snapshot frequency',
          'Potential storage savings: ~25GB by cleaning old snapshots'
        ]
      }
    },
    'pvc-mysql-data': {
      volume_name: 'mysql-data-pvc',
      volume_id: 'pvc-mysql-data',
      volume_size: 85899345920, // 80GB logical
      snapshot_chain: {
        active_lvol: 'lvs_mysql/mysql-active-lvol',
        chain_depth: 2,
        snapshots: [
          {
            bdev_name: 'lvs_mysql/mysql-active-lvol',
            snapshot_id: 'current-active-volume',
            details: { creation_time: '2025-01-03T00:00:00Z' },
            children: [
              {
                bdev_name: 'clone_mysql_replica_0',
                snapshot_id: 'snap-mysql-clone-20250102-090000',
                details: { creation_time: '2025-01-02T09:00:00Z' },
                children: [],
                storage_info: {
                  consumed_bytes: 15728640000,
                  cluster_size: 4194304,
                  allocated_clusters: 3750
                },
                creation_order: 1,
                is_active_volume: false
              }
            ],
            storage_info: {
              consumed_bytes: 85899345920, // 80GB active data
              cluster_size: 4194304,
              allocated_clusters: 20480
            },
            creation_order: 0,
            is_active_volume: true
          }
        ],
        error: undefined
      },
      storage_analytics: {
        total_volume_size: 107374182400,
        actual_data_size: 85899345920, // 80GB actual data
        total_snapshot_overhead: 15728640000, // ~14.6GB in snapshots
        snapshot_efficiency_ratio: 0.146, // 14.6% overhead - reasonable
        storage_breakdown: {
          active_volume_consumption: 85899345920,
          snapshot_consumption: 15728640000,
          metadata_overhead: 536870912, // 512MB metadata
          free_space_in_volume: 5209292800 // ~4.8GB free
        },
        recommendations: [
          'Storage efficiency is good at 14.6% snapshot overhead',
          'Consider archiving snapshots older than 30 days',
          'Monitor for snapshot chain depth > 10'
        ]
      }
    },
    'pvc-postgres-data': {
      volume_name: 'postgres-data-pvc',
      volume_id: 'pvc-postgres-data',
      volume_size: 107374182400, // 100GB logical
      snapshot_chain: {
        active_lvol: 'lvs_postgres/postgres-active-lvol',
        chain_depth: 3,
        snapshots: [
          {
            bdev_name: 'lvs_postgres/postgres-active-lvol',
            snapshot_id: 'current-active-volume',
            details: { creation_time: '2025-01-02T00:00:00Z' },
            children: [
              {
                bdev_name: 'snap_postgres_replica_0',
                snapshot_id: 'snap-postgres-20250101-120000',
                details: { creation_time: '2025-01-01T12:00:00Z' },
                children: [],
                storage_info: {
                  consumed_bytes: 4294967296, // 4GB - very efficient
                  cluster_size: 4194304,
                  allocated_clusters: 1024
                },
                creation_order: 1,
                is_active_volume: false
              }
            ],
            storage_info: {
              consumed_bytes: 75161927680, // 70GB active data
              cluster_size: 4194304,
              allocated_clusters: 17920
            },
            creation_order: 0,
            is_active_volume: true
          }
        ],
        error: undefined
      },
      storage_analytics: {
        total_volume_size: 85899345920,
        actual_data_size: 75161927680, // 70GB actual data
        total_snapshot_overhead: 4294967296, // Only 4GB in snapshots - excellent!
        snapshot_efficiency_ratio: 0.05, // 5% overhead - very efficient
        storage_breakdown: {
          active_volume_consumption: 75161927680,
          snapshot_consumption: 4294967296,
          metadata_overhead: 134217728, // 128MB metadata
          free_space_in_volume: 6308233216 // ~5.9GB free
        },
        recommendations: [
          'Excellent storage efficiency at 5% snapshot overhead',
          'Clone-based snapshots are working very efficiently',
          'Continue current snapshot strategy',
          'Good candidate for additional snapshot frequency if needed'
        ]
      }
    }
  };

  // Transform backend data to include relationships
  const enhanceSnapshotsWithRelationships = (backendSnapshots: any[]): SnapshotDetails[] => {
    const relationships = new Map<string, { parent?: string; children: string[] }>();
    
    // Build relationship map from clone_source_snapshot_id
    backendSnapshots.forEach(snap => {
      if (!relationships.has(snap.snapshot_id)) {
        relationships.set(snap.snapshot_id, { children: [] });
      }
      
      if (snap.clone_source_snapshot_id) {
        // This snapshot has a parent
        relationships.get(snap.snapshot_id)!.parent = snap.clone_source_snapshot_id;
        
        // Add this as a child to the parent
        if (!relationships.has(snap.clone_source_snapshot_id)) {
          relationships.set(snap.clone_source_snapshot_id, { children: [] });
        }
        relationships.get(snap.clone_source_snapshot_id)!.children.push(snap.snapshot_id);
      }
    });

    // Enhance snapshots with relationship data
    return backendSnapshots.map(snap => ({
      ...snap,
      parent_snapshot_id: relationships.get(snap.snapshot_id)?.parent,
      child_snapshot_ids: relationships.get(snap.snapshot_id)?.children || [],
      // Add mock storage consumption if not provided by backend
      storage_consumption: snap.storage_consumption || {
        consumed_bytes: snap.size_bytes * 0.3, // Mock 30% consumption
        cluster_size: 4194304,
        allocated_clusters: Math.ceil(snap.size_bytes * 0.3 / 4194304),
        actual_storage_overhead: snap.size_bytes * 0.1
      },
      // Ensure replica_bdev_details is always an array
      replica_bdev_details: snap.replica_bdev_details || []
    }));
  };

  // Transform tree data to include storage analytics
  const enhanceTreeWithStorageAnalytics = (backendTree: any): Record<string, SnapshotTreeNode> => {
    const enhanced: Record<string, SnapshotTreeNode> = {};
    
    Object.entries(backendTree).forEach(([volumeId, volumeData]: [string, any]) => {
      // Calculate storage analytics from chain data
      const chainSnapshots = volumeData.snapshot_chain?.snapshots || [];
      const totalSnapshotConsumption = chainSnapshots.reduce((sum: number, snap: any) => 
        sum + (snap.storage_info?.consumed_bytes || 0), 0
      );
      
      const volumeSize = volumeData.volume_size || 0;
      const actualDataSize = volumeSize * 0.7; // Mock 70% actual data usage
      
      enhanced[volumeId] = {
        ...volumeData,
        storage_analytics: volumeData.storage_analytics || {
          total_volume_size: volumeSize,
          actual_data_size: actualDataSize,
          total_snapshot_overhead: totalSnapshotConsumption,
          snapshot_efficiency_ratio: totalSnapshotConsumption / volumeSize,
          storage_breakdown: {
            active_volume_consumption: actualDataSize,
            snapshot_consumption: totalSnapshotConsumption,
            metadata_overhead: volumeSize * 0.01,
            free_space_in_volume: volumeSize - actualDataSize - totalSnapshotConsumption
          },
          recommendations: []
        }
      };
    });
    
    return enhanced;
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
      sum + tree.volume_size, 0
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
      case 'topology':
        return (
          <SnapshotsTopologyView
            snapshots={snapshots}
            formatSize={formatSize}
            selectedVolume={topologyVolume}
            onVolumeChange={setTopologyVolume}
            availableVolumes={availableVolumes}
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
      {/* Header with Storage Insights */}
      <div className="bg-white rounded-lg shadow p-6">
        <div className="flex items-center justify-between mb-6">
          <div className="flex items-center gap-3">
            <Camera className="w-8 h-8 text-blue-600" />
            <div>
              <h2 className="text-2xl font-bold text-gray-900">Volume Snapshots</h2>
              <p className="text-sm text-gray-600">Storage-aware snapshot management</p>
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

        {/* Enhanced Statistics with Storage Information */}
        <div className="grid grid-cols-2 md:grid-cols-6 gap-4">
          <div className="bg-blue-50 rounded-lg p-4 text-center">
            <Database className="w-6 h-6 text-blue-600 mx-auto mb-2" />
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
              {snapshots.reduce((sum, s) => sum + (s.replica_bdev_details || []).length, 0)}
            </p>
            <p className="text-sm text-gray-600">Replica Snapshots</p>
          </div>
          <div className="bg-indigo-50 rounded-lg p-4 text-center">
            <HardDrive className="w-6 h-6 text-indigo-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-indigo-600">
              {formatSize(storageInsights.totalLogicalSize)}
            </p>
            <p className="text-sm text-gray-600">Logical Storage</p>
          </div>
          <div className="bg-orange-50 rounded-lg p-4 text-center">
            <BarChart3 className="w-6 h-6 text-orange-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-orange-600">
              {formatSize(storageInsights.totalSnapshotOverhead)}
            </p>
            <p className="text-sm text-gray-600">Snapshot Overhead</p>
          </div>
          <div className="bg-yellow-50 rounded-lg p-4 text-center">
            <TrendingUp className="w-6 h-6 text-yellow-600 mx-auto mb-2" />
            <p className={`text-xl font-bold ${
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
                  onClick={() => setActiveView('topology')}
                  className={`px-4 py-2 text-sm font-medium border-l border-gray-300 ${
                    activeView === 'topology'
                      ? 'bg-blue-600 text-white'
                      : 'bg-white text-gray-700 hover:bg-gray-50'
                  }`}
                >
                  <TrendingUp className="w-4 h-4 mr-2 inline" />
                  Topology View
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
