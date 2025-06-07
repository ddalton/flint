import React, { useState, useEffect, useMemo, useCallback } from 'react';
import { 
  Camera, Search, Filter, RefreshCw, Clock, Database, 
  GitBranch, Eye, Trash2, Plus, ChevronDown, 
  AlertTriangle, CheckCircle, X, Info, FileImage, Layers, Maximize2
} from 'lucide-react';
import ReactFlow, {
  Controls,
  Background,
  useNodesState,
  useEdgesState,
  addEdge,
  MarkerType,
  Position
} from 'reactflow';
import type { Connection, Node, Edge, NodeTypes } from 'reactflow';
import 'reactflow/dist/style.css';

interface Snapshot {
  snapshot_id: string;
  source_volume_id: string;
  creation_time: string | null;
  ready_to_use: boolean;
  size_bytes: number;
  snapshot_type: 'Bdev' | 'LvolClone' | 'External';
  clone_source_snapshot_id: string | null;
  spdk_bdev_details: {
    node: string;
    name: string;
    aliases: string[];
    driver: string;
    snapshot_source_bdev: string | null;
  } | null;
}

interface Volume {
  id: string;
  name: string;
  size: string;
  state: string;
}

interface SnapshotsTabProps {
  volumes: Volume[];
}

// Custom node component for volumes
const VolumeNode = ({ data }: { data: any }) => {
  const { volume, snapshots } = data;
  const snapshotCount = snapshots?.length || 0;
  
  return (
    <div className="px-4 py-3 bg-blue-50 border-2 border-blue-200 rounded-lg shadow-md min-w-[200px]">
      <div className="flex items-center gap-2 mb-2">
        <Database className="w-5 h-5 text-blue-600" />
        <div className="font-semibold text-blue-900">{volume.name}</div>
      </div>
      <div className="text-xs text-blue-700 space-y-1">
        <div>Size: {volume.size}</div>
        <div>State: {volume.state}</div>
        <div className="flex items-center gap-1">
          <Camera className="w-3 h-3" />
          {snapshotCount} snapshot{snapshotCount !== 1 ? 's' : ''}
        </div>
      </div>
    </div>
  );
};

// Custom node component for snapshots
const SnapshotNode = ({ data }: { data: any }) => {
  const { snapshot } = data;
  const formatDate = (dateStr: string | null) => {
    if (!dateStr) return 'Unknown';
    return new Date(dateStr).toLocaleDateString();
  };
  
  const formatSize = (bytes: number) => {
    if (bytes >= 1024 * 1024 * 1024) {
      return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)}GB`;
    } else if (bytes >= 1024 * 1024) {
      return `${(bytes / (1024 * 1024)).toFixed(1)}MB`;
    }
    return `${bytes} bytes`;
  };

  const getNodeColor = () => {
    switch (snapshot.snapshot_type) {
      case 'Bdev': return 'bg-green-50 border-green-200 text-green-900';
      case 'LvolClone': return 'bg-purple-50 border-purple-200 text-purple-900';
      case 'External': return 'bg-orange-50 border-orange-200 text-orange-900';
      default: return 'bg-gray-50 border-gray-200 text-gray-900';
    }
  };

  const getTypeIcon = () => {
    switch (snapshot.snapshot_type) {
      case 'Bdev': return <Camera className="w-4 h-4" />;
      case 'LvolClone': return <GitBranch className="w-4 h-4" />;
      case 'External': return <FileImage className="w-4 h-4" />;
      default: return <Layers className="w-4 h-4" />;
    }
  };

  return (
    <div className={`px-3 py-2 border-2 rounded-lg shadow-sm min-w-[180px] ${getNodeColor()}`}>
      <div className="flex items-center gap-2 mb-1">
        {getTypeIcon()}
        <div className="font-medium text-sm">{snapshot.snapshot_id}</div>
        {!snapshot.ready_to_use && (
          <AlertTriangle className="w-3 h-3 text-yellow-600" />
        )}
      </div>
      <div className="text-xs space-y-0.5">
        <div>Type: {snapshot.snapshot_type}</div>
        <div>Size: {formatSize(snapshot.size_bytes)}</div>
        <div>Created: {formatDate(snapshot.creation_time)}</div>
        {snapshot.spdk_bdev_details && (
          <div>Node: {snapshot.spdk_bdev_details.node}</div>
        )}
      </div>
    </div>
  );
};

const nodeTypes: NodeTypes = {
  volume: VolumeNode,
  snapshot: SnapshotNode,
};

export const SnapshotsTab: React.FC<SnapshotsTabProps> = ({ volumes }) => {
  const [snapshots, setSnapshots] = useState<Snapshot[]>([]);
  const [loading, setLoading] = useState(true);
  const [searchTerm, setSearchTerm] = useState('');
  const [selectedVolumeId, setSelectedVolumeId] = useState<string>('');
  const [showAdvancedFilters, setShowAdvancedFilters] = useState(false);
  const [snapshotTypeFilter, setSnapshotTypeFilter] = useState<'all' | 'Bdev' | 'LvolClone' | 'External'>('all');
  const [readyStatusFilter, setReadyStatusFilter] = useState<'all' | 'ready' | 'not_ready'>('all');
  const [layoutDirection, setLayoutDirection] = useState<'TB' | 'LR'>('TB');

  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);

  // Fetch all snapshots
  useEffect(() => {
    fetchAllSnapshots();
  }, []);

  const fetchAllSnapshots = async () => {
    setLoading(true);
    try {
      if (selectedVolumeId) {
        const response = await fetch(`/api/volumes/${selectedVolumeId}/snapshots`);
        if (response.ok) {
          const data = await response.json();
          setSnapshots(data);
        } else {
          console.error('Failed to fetch volume snapshots');
          setSnapshots([]);
        }
      } else {
        // Fetch snapshots for all volumes
        const allSnapshots: Snapshot[] = [];
        for (const volume of volumes) {
          try {
            const response = await fetch(`/api/volumes/${volume.id}/snapshots`);
            if (response.ok) {
              const data = await response.json();
              allSnapshots.push(...data);
            }
          } catch (error) {
            console.warn(`Failed to fetch snapshots for volume ${volume.id}:`, error);
          }
        }
        setSnapshots(allSnapshots);
      }
    } catch (error) {
      console.error('Error fetching snapshots:', error);
      setSnapshots([]);
    } finally {
      setLoading(false);
    }
  };

  // Refetch when selected volume changes
  useEffect(() => {
    fetchAllSnapshots();
  }, [selectedVolumeId, volumes]);

  // Filter snapshots based on current filters
  const filteredSnapshots = useMemo(() => {
    return snapshots.filter(snapshot => {
      // Apply search filter
      if (searchTerm) {
        const searchLower = searchTerm.toLowerCase();
        const matchesSearch = 
          snapshot.snapshot_id.toLowerCase().includes(searchLower) ||
          snapshot.source_volume_id.toLowerCase().includes(searchLower) ||
          (snapshot.spdk_bdev_details?.name || '').toLowerCase().includes(searchLower);
        if (!matchesSearch) return false;
      }

      // Apply snapshot type filter
      if (snapshotTypeFilter !== 'all' && snapshot.snapshot_type !== snapshotTypeFilter) {
        return false;
      }

      // Apply ready status filter
      if (readyStatusFilter === 'ready' && !snapshot.ready_to_use) return false;
      if (readyStatusFilter === 'not_ready' && snapshot.ready_to_use) return false;

      return true;
    });
  }, [snapshots, searchTerm, snapshotTypeFilter, readyStatusFilter]);

  // Build the flow graph
  const { flowNodes, flowEdges } = useMemo(() => {
    const nodes: Node[] = [];
    const edges: Edge[] = [];
    const volumeGroups = new Map<string, Snapshot[]>();
    
    // Group snapshots by volume
    filteredSnapshots.forEach(snapshot => {
      if (!volumeGroups.has(snapshot.source_volume_id)) {
        volumeGroups.set(snapshot.source_volume_id, []);
      }
      volumeGroups.get(snapshot.source_volume_id)!.push(snapshot);
    });

    let yOffset = 0;
    const xSpacing = 300;
    const ySpacing = 150;

    volumeGroups.forEach((volumeSnapshots, volumeId) => {
      const volume = volumes.find(v => v.id === volumeId);
      if (!volume) return;

      // Create volume node
      const volumeNodeId = `volume-${volumeId}`;
      nodes.push({
        id: volumeNodeId,
        type: 'volume',
        position: { x: 0, y: yOffset },
        data: { volume, snapshots: volumeSnapshots },
        sourcePosition: layoutDirection === 'TB' ? Position.Bottom : Position.Right,
        targetPosition: layoutDirection === 'TB' ? Position.Top : Position.Left,
      });

      // Sort snapshots by creation time to show chronological order
      const sortedSnapshots = [...volumeSnapshots].sort((a, b) => {
        const timeA = new Date(a.creation_time || 0).getTime();
        const timeB = new Date(b.creation_time || 0).getTime();
        return timeA - timeB;
      });

      // Create snapshot nodes
      sortedSnapshots.forEach((snapshot, index) => {
        const snapshotNodeId = `snapshot-${snapshot.snapshot_id}`;
        
        nodes.push({
          id: snapshotNodeId,
          type: 'snapshot',
          position: layoutDirection === 'TB' 
            ? { x: (index + 1) * xSpacing, y: yOffset + ySpacing }
            : { x: xSpacing, y: yOffset + (index + 1) * ySpacing },
          data: { snapshot },
          sourcePosition: layoutDirection === 'TB' ? Position.Bottom : Position.Right,
          targetPosition: layoutDirection === 'TB' ? Position.Top : Position.Left,
        });

        // Create edge from volume to snapshot
        if (index === 0 || !snapshot.clone_source_snapshot_id) {
          edges.push({
            id: `${volumeNodeId}-${snapshotNodeId}`,
            source: volumeNodeId,
            target: snapshotNodeId,
            type: 'smoothstep',
            animated: !snapshot.ready_to_use,
            style: { 
              stroke: snapshot.ready_to_use ? '#10b981' : '#f59e0b',
              strokeWidth: 2 
            },
            markerEnd: {
              type: MarkerType.ArrowClosed,
              color: snapshot.ready_to_use ? '#10b981' : '#f59e0b',
            },
            label: snapshot.snapshot_type,
            labelStyle: { fontSize: 10 },
          });
        }

        // Create edge from parent snapshot if this is a clone
        if (snapshot.clone_source_snapshot_id) {
          const parentNodeId = `snapshot-${snapshot.clone_source_snapshot_id}`;
          edges.push({
            id: `${parentNodeId}-${snapshotNodeId}`,
            source: parentNodeId,
            target: snapshotNodeId,
            type: 'smoothstep',
            style: { 
              stroke: '#8b5cf6',
              strokeWidth: 2,
              strokeDasharray: '5,5'
            },
            markerEnd: {
              type: MarkerType.ArrowClosed,
              color: '#8b5cf6',
            },
            label: 'Clone',
            labelStyle: { fontSize: 10, color: '#8b5cf6' },
          });
        }
      });

      yOffset += Math.max(ySpacing * (sortedSnapshots.length + 1), ySpacing * 2);
    });

    return { flowNodes: nodes, flowEdges: edges };
  }, [filteredSnapshots, volumes, layoutDirection]);

  // Update React Flow nodes and edges
  useEffect(() => {
    setNodes(flowNodes);
    setEdges(flowEdges);
  }, [flowNodes, flowEdges, setNodes, setEdges]);

  const onConnect = useCallback((params: Connection) => {
    setEdges((eds) => addEdge(params, eds));
  }, [setEdges]);

  const clearAllFilters = () => {
    setSearchTerm('');
    setSelectedVolumeId('');
    setSnapshotTypeFilter('all');
    setReadyStatusFilter('all');
  };

  const getActiveFilterCount = () => {
    let count = 0;
    if (searchTerm) count++;
    if (selectedVolumeId) count++;
    if (snapshotTypeFilter !== 'all') count++;
    if (readyStatusFilter !== 'all') count++;
    return count;
  };

  const activeFilterCount = getActiveFilterCount();

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
              <p className="text-gray-600">Visual representation of SPDK logical volume snapshots and clones</p>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <button
              onClick={fetchAllSnapshots}
              disabled={loading}
              className="p-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded-md disabled:opacity-50"
            >
              <RefreshCw className={`w-5 h-5 ${loading ? 'animate-spin' : ''}`} />
            </button>
          </div>
        </div>

        {/* Statistics */}
        <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
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
            <p className="text-sm text-gray-600">Ready</p>
          </div>
          <div className="bg-purple-50 rounded-lg p-4 text-center">
            <GitBranch className="w-6 h-6 text-purple-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-purple-600">
              {snapshots.filter(s => s.snapshot_type === 'LvolClone').length}
            </p>
            <p className="text-sm text-gray-600">Clones</p>
          </div>
          <div className="bg-orange-50 rounded-lg p-4 text-center">
            <Database className="w-6 h-6 text-orange-600 mx-auto mb-2" />
            <p className="text-xl font-bold text-orange-600">
              {new Set(snapshots.map(s => s.source_volume_id)).size}
            </p>
            <p className="text-sm text-gray-600">Volumes</p>
          </div>
        </div>
      </div>

      {/* Filters */}
      <div className="bg-white rounded-lg shadow">
        <div className="px-6 py-4 border-b border-gray-200 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <Filter className="w-5 h-5 text-gray-600" />
            <span className="font-medium">Filters</span>
            {activeFilterCount > 0 && (
              <span className="px-2 py-1 text-xs bg-blue-100 text-blue-800 rounded-full">
                {activeFilterCount} active
              </span>
            )}
          </div>
          <div className="flex items-center gap-2">
            {activeFilterCount > 0 && (
              <button
                onClick={clearAllFilters}
                className="text-sm text-gray-600 hover:text-gray-800 flex items-center gap-1"
              >
                <X className="w-4 h-4" />
                Clear All
              </button>
            )}
            <button
              onClick={() => setShowAdvancedFilters(!showAdvancedFilters)}
              className="flex items-center gap-1 text-sm text-gray-600 hover:text-gray-800"
            >
              <ChevronDown className={`w-4 h-4 transition-transform ${showAdvancedFilters ? 'rotate-180' : ''}`} />
              {showAdvancedFilters ? 'Hide' : 'Show'} Filters
            </button>
          </div>
        </div>

        {/* Search Bar */}
        <div className="px-6 py-4 bg-gray-50">
          <div className="relative">
            <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
            <input
              type="text"
              placeholder="Search by snapshot ID, volume ID, or node..."
              value={searchTerm}
              onChange={(e) => setSearchTerm(e.target.value)}
              className="w-full pl-10 pr-4 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
            />
          </div>
        </div>

        {/* Advanced Filters */}
        {showAdvancedFilters && (
          <div className="px-6 py-4 border-t border-gray-200 space-y-4">
            <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
              {/* Volume Filter */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">Volume</label>
                <select
                  value={selectedVolumeId}
                  onChange={(e) => setSelectedVolumeId(e.target.value)}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="">All Volumes</option>
                  {volumes.map(volume => (
                    <option key={volume.id} value={volume.id}>
                      {volume.name} ({volume.size})
                    </option>
                  ))}
                </select>
              </div>

              {/* Snapshot Type Filter */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">Snapshot Type</label>
                <select
                  value={snapshotTypeFilter}
                  onChange={(e) => setSnapshotTypeFilter(e.target.value as any)}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="all">All Types</option>
                  <option value="Bdev">Standard Snapshots</option>
                  <option value="LvolClone">Clones</option>
                  <option value="External">External</option>
                </select>
              </div>

              {/* Ready Status Filter */}
              <div>
                <label className="block text-sm font-medium text-gray-700 mb-2">Status</label>
                <select
                  value={readyStatusFilter}
                  onChange={(e) => setReadyStatusFilter(e.target.value as any)}
                  className="w-full border border-gray-300 rounded-md px-3 py-2 focus:outline-none focus:ring-2 focus:ring-blue-500"
                >
                  <option value="all">All Status</option>
                  <option value="ready">Ready</option>
                  <option value="not_ready">Not Ready</option>
                </select>
              </div>
            </div>
          </div>
        )}
      </div>

      {/* Layout Controls */}
      <div className="bg-white rounded-lg shadow p-4">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-4">
            <span className="text-sm font-medium text-gray-700">Layout:</span>
            <div className="flex border border-gray-300 rounded-md overflow-hidden">
              <button
                onClick={() => setLayoutDirection('TB')}
                className={`px-3 py-1 text-sm ${layoutDirection === 'TB' ? 'bg-blue-600 text-white' : 'bg-white text-gray-700 hover:bg-gray-50'}`}
              >
                Top to Bottom
              </button>
              <button
                onClick={() => setLayoutDirection('LR')}
                className={`px-3 py-1 text-sm border-l border-gray-300 ${layoutDirection === 'LR' ? 'bg-blue-600 text-white' : 'bg-white text-gray-700 hover:bg-gray-50'}`}
              >
                Left to Right
              </button>
            </div>
          </div>
          
          <div className="text-sm text-gray-500">
            {filteredSnapshots.length} snapshot{filteredSnapshots.length !== 1 ? 's' : ''} displayed
          </div>
        </div>
      </div>

      {/* React Flow Diagram */}
      <div className="bg-white rounded-lg shadow" style={{ height: '600px' }}>
        {flowNodes.length > 0 ? (
          <ReactFlow
            nodes={flowNodes}
            edges={flowEdges}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onConnect={onConnect}
            nodeTypes={nodeTypes}
            fitView
            attributionPosition="bottom-left"
          >
            <Controls />
            <Background color="#f1f5f9" gap={16} />
          </ReactFlow>
        ) : (
          <div className="flex flex-col items-center justify-center h-full text-gray-500">
            <Camera className="w-12 h-12 mb-4" />
            <p className="text-lg font-medium">No snapshots found</p>
            <p className="text-sm">
              {activeFilterCount > 0 
                ? 'Try adjusting your filters to see more results.'
                : 'No snapshots have been created for the selected volumes.'
              }
            </p>
          </div>
        )}
      </div>

      {/* Legend */}
      <div className="bg-white rounded-lg shadow p-6">
        <h3 className="text-lg font-semibold mb-4 flex items-center gap-2">
          <Info className="w-5 h-5 text-blue-600" />
          Legend
        </h3>
        <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
          <div>
            <h4 className="font-medium text-gray-700 mb-3">Node Types</h4>
            <div className="space-y-2">
              <div className="flex items-center gap-3">
                <div className="w-4 h-4 bg-blue-200 border border-blue-300 rounded"></div>
                <span className="text-sm">Volume (Source)</span>
              </div>
              <div className="flex items-center gap-3">
                <div className="w-4 h-4 bg-green-200 border border-green-300 rounded"></div>
                <span className="text-sm">Standard Snapshot</span>
              </div>
              <div className="flex items-center gap-3">
                <div className="w-4 h-4 bg-purple-200 border border-purple-300 rounded"></div>
                <span className="text-sm">Clone (Writable)</span>
              </div>
              <div className="flex items-center gap-3">
                <div className="w-4 h-4 bg-orange-200 border border-orange-300 rounded"></div>
                <span className="text-sm">External Snapshot</span>
              </div>
            </div>
          </div>
          <div>
            <h4 className="font-medium text-gray-700 mb-3">Connections</h4>
            <div className="space-y-2">
              <div className="flex items-center gap-3">
                <div className="w-8 h-0.5 bg-green-500"></div>
                <span className="text-sm">Snapshot Created (Ready)</span>
              </div>
              <div className="flex items-center gap-3">
                <div className="w-8 h-0.5 bg-yellow-500"></div>
                <span className="text-sm">Snapshot Creating (Not Ready)</span>
              </div>
              <div className="flex items-center gap-3">
                <div className="w-8 h-0.5 bg-purple-500 border-dashed border-t-2 border-purple-500"></div>
                <span className="text-sm">Clone Relationship</span>
              </div>
            </div>
          </div>
        </div>
      </div>

      {/* Information Panel */}
      <div className="bg-blue-50 border border-blue-200 rounded-lg p-6">
        <div className="flex items-start gap-3">
          <Info className="w-6 h-6 text-blue-600 mt-1 flex-shrink-0" />
          <div>
            <h4 className="font-medium text-blue-900 mb-2">SPDK Logical Volume Snapshots</h4>
            <div className="text-sm text-blue-800 space-y-2">
              <p>
                <strong>Snapshots:</strong> Read-only point-in-time copies of volumes that share storage 
                efficiently using copy-on-write.
              </p>
              <p>
                <strong>Clones:</strong> Writable volumes created from snapshots that can be modified 
                independently while sharing unchanged data.
              </p>
              <p>
                <strong>Tree Structure:</strong> Shows the hierarchical relationship between volumes, 
                snapshots, and clones as described in the SPDK documentation.
              </p>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
