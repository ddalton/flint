import React, { useState, useMemo } from 'react';
import { Filter, X, Search, Server, ChevronDown, ChevronUp, ChevronLeft, ChevronRight } from 'lucide-react';
import { NodeDetailView } from './NodeDetailView';
import type { DashboardData, VolumeFilter } from '../../hooks/useDashboardData';

interface FilteredNodesViewProps {
  data: DashboardData;
  activeFilter?: VolumeFilter;
  onClearFilter?: () => void;
  onDiskVolumeFilter?: (diskId: string) => void;
}

export const FilteredNodesView: React.FC<FilteredNodesViewProps> = ({ 
  data, 
  activeFilter, 
  onClearFilter,
  onDiskVolumeFilter
}) => {
  // Search and filter state
  const [searchTerm, setSearchTerm] = useState('');
  const [showNodeSummary, setShowNodeSummary] = useState(true);
  const [sortBy, setSortBy] = useState<'name' | 'volumes' | 'disks' | 'capacity'>('name');
  const [sortOrder, setSortOrder] = useState<'asc' | 'desc'>('asc');
  
  // Pagination state
  const [currentPage, setCurrentPage] = useState(1);
  const [pageSize, setPageSize] = useState(10);

  // Filter volumes based on the active filter
  const getFilteredVolumes = () => {
    if (!activeFilter || activeFilter === 'all') {
      return data.volumes;
    }

    switch (activeFilter) {
      case 'healthy':
        return data.volumes.filter(v => v.state === 'Healthy');
      case 'degraded':
        return data.volumes.filter(v => v.state === 'Degraded');
      case 'failed':
        return data.volumes.filter(v => v.state === 'Failed');
      case 'faulted':
        return data.volumes.filter(v => v.state === 'Degraded' || v.state === 'Failed');
      case 'rebuilding':
        return data.volumes.filter(v => 
          v.replica_statuses.some(replica => 
            replica.status === 'rebuilding' || 
            replica.rebuild_progress !== null ||
            replica.is_new_replica
          )
        );
      case 'local-nvme':
        return data.volumes.filter(v => v.local_nvme);
      default:
        return data.volumes;
    }
  };

  // Get nodes that match search criteria and have the filtered volumes
  const getFilteredAndSearchedNodes = () => {
    const filteredVolumes = getFilteredVolumes();
    
    // First, get nodes based on volume filter
    let relevantNodes = new Set<string>();
    
    if (filteredVolumes.length === 0 && activeFilter && activeFilter !== 'all') {
      // No volumes match the filter, return empty
      return [];
    }

    if (activeFilter && activeFilter !== 'all') {
      filteredVolumes.forEach(volume => {
        switch (activeFilter) {
          case 'healthy':
          case 'degraded':
          case 'failed':
          case 'faulted':
            // Include all nodes that have replicas of volumes in these states
            volume.replica_statuses.forEach(replica => {
              relevantNodes.add(replica.node);
            });
            break;
            
          case 'rebuilding':
            // Only include nodes that actually have rebuilding replicas
            volume.replica_statuses.forEach(replica => {
              if (replica.status === 'rebuilding' || 
                  replica.rebuild_progress !== null ||
                  replica.is_new_replica) {
                relevantNodes.add(replica.node);
              }
            });
            break;
            
          case 'local-nvme':
            // Only include nodes that have local NVMe replicas for this volume
            volume.replica_statuses.forEach(replica => {
              if (replica.is_local) {
                relevantNodes.add(replica.node);
              }
            });
            break;
            
          default:
            // For 'all' or unknown filters, include all nodes with any replica
            volume.nodes.forEach(node => {
              relevantNodes.add(node);
            });
            break;
        }
      });
    } else {
      // No volume filter, include all nodes
      relevantNodes = new Set(data.nodes);
    }

    // Apply search filter to nodes
    let searchedNodes = Array.from(relevantNodes);
    
    if (searchTerm.trim()) {
      const searchLower = searchTerm.toLowerCase();
      searchedNodes = searchedNodes.filter(nodeName => {
        // Search by node name
        if (nodeName.toLowerCase().includes(searchLower)) {
          return true;
        }
        
        // Search by disk model or ID
        const nodeDisks = data.disks.filter(d => d.node === nodeName);
        if (nodeDisks.some(disk => 
          disk.id.toLowerCase().includes(searchLower) ||
          disk.model.toLowerCase().includes(searchLower) ||
          disk.pci_addr.toLowerCase().includes(searchLower)
        )) {
          return true;
        }
        
        // Search by volume name
        const nodeVolumes = data.volumes.filter(v => v.nodes.includes(nodeName));
        if (nodeVolumes.some(volume => 
          volume.name.toLowerCase().includes(searchLower) ||
          volume.id.toLowerCase().includes(searchLower)
        )) {
          return true;
        }
        
        return false;
      });
    }

    return searchedNodes;
  };

  // Get enhanced node data with stats for sorting
  const getEnhancedNodeData = useMemo(() => {
    const searchedNodes = getFilteredAndSearchedNodes();
    const filteredVolumes = getFilteredVolumes();
    
    const enhancedNodes = searchedNodes.map(node => {
      const nodeDisks = data.disks.filter(d => d.node === node);
      const nodeVolumes = data.volumes.filter(v => v.nodes.includes(node));
      const nodeFilteredVolumes = filteredVolumes.filter(v => v.nodes.includes(node));
      const totalCapacity = nodeDisks.reduce((sum, disk) => sum + disk.capacity_gb, 0);
      const totalAllocated = nodeDisks.reduce((sum, disk) => sum + disk.allocated_space, 0);
      const healthyDisks = nodeDisks.filter(d => d.healthy).length;
      
      return {
        name: node,
        nodeDisks,
        nodeVolumes,
        nodeFilteredVolumes,
        healthyDisks,
        totalCapacity,
        totalAllocated,
        totalFree: totalCapacity - totalAllocated,
        volumeCount: activeFilter && activeFilter !== 'all' ? nodeFilteredVolumes.length : nodeVolumes.length,
        diskCount: nodeDisks.length
      };
    });

    // Sort enhanced nodes
    enhancedNodes.sort((a, b) => {
      let aValue: any, bValue: any;
      
      switch (sortBy) {
        case 'name':
          aValue = a.name;
          bValue = b.name;
          break;
        case 'volumes':
          aValue = a.volumeCount;
          bValue = b.volumeCount;
          break;
        case 'disks':
          aValue = a.diskCount;
          bValue = b.diskCount;
          break;
        case 'capacity':
          aValue = a.totalCapacity;
          bValue = b.totalCapacity;
          break;
        default:
          aValue = a.name;
          bValue = b.name;
      }

      if (typeof aValue === 'string') {
        return sortOrder === 'asc' 
          ? aValue.localeCompare(bValue)
          : bValue.localeCompare(aValue);
      } else {
        return sortOrder === 'asc' ? aValue - bValue : bValue - aValue;
      }
    });

    return enhancedNodes;
  }, [data, activeFilter, searchTerm, sortBy, sortOrder]);

  // Pagination calculations
  const allEnhancedNodes = getEnhancedNodeData;
  const totalPages = Math.ceil(allEnhancedNodes.length / pageSize);
  const paginatedNodes = allEnhancedNodes.slice((currentPage - 1) * pageSize, currentPage * pageSize);

  // Reset pagination when filters change
  React.useEffect(() => {
    setCurrentPage(1);
  }, [activeFilter, searchTerm, sortBy, sortOrder]);

  const handleSort = (field: typeof sortBy) => {
    if (sortBy === field) {
      setSortOrder(sortOrder === 'asc' ? 'desc' : 'asc');
    } else {
      setSortBy(field);
      setSortOrder('asc');
    }
  };

  const clearSearch = () => {
    setSearchTerm('');
  };

  const getFilterDisplayName = (filter: VolumeFilter) => {
    switch (filter) {
      case 'healthy': return 'Healthy Volumes';
      case 'degraded': return 'Degraded Volumes';
      case 'failed': return 'Failed Volumes';
      case 'faulted': return 'Faulted Volumes (Degraded + Failed)';
      case 'rebuilding': return 'Volumes with Rebuilding Replicas';
      case 'local-nvme': return 'Local NVMe Volumes';
      default: return 'All Volumes';
    }
  };

  const getNodeFilterDescription = (filter: VolumeFilter) => {
    switch (filter) {
      case 'healthy': return 'nodes with healthy volumes';
      case 'degraded': return 'nodes with degraded volumes';
      case 'failed': return 'nodes with failed volumes';
      case 'faulted': return 'nodes with faulted volumes';
      case 'rebuilding': return 'nodes with rebuilding replica activity';
      case 'local-nvme': return 'nodes with local NVMe volumes';
      default: return 'nodes';
    }
  };

  const getFilterSeverityInfo = (filter: VolumeFilter) => {
    switch (filter) {
      case 'failed':
        return {
          bgColor: 'bg-red-50',
          borderColor: 'border-red-200',
          textColor: 'text-red-900'
        };
      case 'degraded':
        return {
          bgColor: 'bg-yellow-50',
          borderColor: 'border-yellow-200',
          textColor: 'text-yellow-900'
        };
      case 'faulted':
        return {
          bgColor: 'bg-orange-50',
          borderColor: 'border-orange-200',
          textColor: 'text-orange-900'
        };
      case 'rebuilding':
        return {
          bgColor: 'bg-orange-50',
          borderColor: 'border-orange-200',
          textColor: 'text-orange-900'
        };
      case 'healthy':
        return {
          bgColor: 'bg-green-50',
          borderColor: 'border-green-200',
          textColor: 'text-green-900'
        };
      default:
        return {
          bgColor: 'bg-blue-50',
          borderColor: 'border-blue-200',
          textColor: 'text-blue-900'
        };
    }
  };

  const filteredVolumes = getFilteredVolumes();
  const totalNodes = data.nodes.length;
  const severityInfo = getFilterSeverityInfo(activeFilter);
  const hasSearch = searchTerm.trim().length > 0;
  const hasVolumeFilter = activeFilter && activeFilter !== 'all';

  return (
    <div>
      {/* Search and Filter Controls */}
      <div className="mb-6 bg-white rounded-lg shadow p-4">
        <div className="flex items-center justify-between mb-4">
          <div className="flex items-center gap-2">
            <Server className="w-5 h-5 text-gray-600" />
            <h3 className="text-lg font-medium text-gray-900">Node Management</h3>
            {(hasVolumeFilter || hasSearch) && (
              <span className="px-2 py-1 text-xs bg-blue-100 text-blue-800 rounded-full">
                {hasVolumeFilter && hasSearch ? '2 filters' : '1 filter'} active
              </span>
            )}
          </div>
          <div className="flex items-center gap-2">
            <button
              onClick={() => setShowNodeSummary(!showNodeSummary)}
              className="text-sm text-gray-600 hover:text-gray-800 flex items-center gap-1"
            >
              {showNodeSummary ? <ChevronUp className="w-4 h-4" /> : <ChevronDown className="w-4 h-4" />}
              {showNodeSummary ? 'Hide' : 'Show'} Summary
            </button>
          </div>
        </div>

        {/* Search Bar */}
        <div className="relative mb-4">
          <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
          <input
            type="text"
            placeholder="Search nodes by name, disk model/ID, volume name, or PCI address..."
            value={searchTerm}
            onChange={(e) => setSearchTerm(e.target.value)}
            className="w-full pl-10 pr-10 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-blue-500"
          />
          {hasSearch && (
            <button
              onClick={clearSearch}
              className="absolute right-3 top-1/2 transform -translate-y-1/2 text-gray-400 hover:text-gray-600"
            >
              <X className="w-4 h-4" />
            </button>
          )}
        </div>

        {/* Sort and Page Size Controls */}
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-4">
            <span className="text-sm text-gray-700">Sort by:</span>
            {[
              { key: 'name', label: 'Name' },
              { key: 'volumes', label: 'Volumes' },
              { key: 'disks', label: 'Disks' },
              { key: 'capacity', label: 'Capacity' }
            ].map(({ key, label }) => (
              <button
                key={key}
                onClick={() => handleSort(key as typeof sortBy)}
                className={`text-sm px-2 py-1 rounded transition-colors ${
                  sortBy === key 
                    ? 'bg-blue-100 text-blue-700' 
                    : 'text-gray-600 hover:text-gray-800 hover:bg-gray-100'
                }`}
              >
                {label}
                {sortBy === key && (
                  <span className="ml-1">
                    {sortOrder === 'asc' ? '↑' : '↓'}
                  </span>
                )}
              </button>
            ))}
          </div>
          
          {/* Page Size Selector */}
          <div className="flex items-center gap-2">
            <span className="text-sm text-gray-700">Show:</span>
            <select
              value={pageSize}
              onChange={(e) => {
                setPageSize(Number(e.target.value));
                setCurrentPage(1);
              }}
              className="border border-gray-300 rounded px-2 py-1 text-sm"
            >
              <option value={5}>5</option>
              <option value={10}>10</option>
              <option value={20}>20</option>
              <option value={50}>50</option>
            </select>
            <span className="text-sm text-gray-700">per page</span>
          </div>
        </div>
      </div>

      {/* Node Summary Cards */}
      {showNodeSummary && (
        <div className="grid grid-cols-1 md:grid-cols-4 gap-4 mb-6">
          <div className="bg-white rounded-lg p-4 shadow">
            <div className="flex items-center">
              <Server className="w-8 h-8 text-blue-600 mr-3" />
              <div>
                <p className="text-sm font-medium">
                  {hasVolumeFilter || hasSearch ? 'Filtered Nodes' : 'Total Nodes'}
                </p>
                <p className="text-2xl font-bold text-blue-600">
                  {allEnhancedNodes.length}
                  {(hasVolumeFilter || hasSearch) && (
                    <span className="text-lg text-gray-500">/{totalNodes}</span>
                  )}
                </p>
              </div>
            </div>
          </div>
          
          <div className="bg-white rounded-lg p-4 shadow">
            <div className="flex items-center">
              <Filter className="w-8 h-8 text-green-600 mr-3" />
              <div>
                <p className="text-sm font-medium">Total Capacity</p>
                <p className="text-2xl font-bold text-green-600">
                  {allEnhancedNodes.reduce((sum, node) => sum + node.totalCapacity, 0).toLocaleString()}GB
                </p>
              </div>
            </div>
          </div>
          
          <div className="bg-white rounded-lg p-4 shadow">
            <div className="flex items-center">
              <div className="w-8 h-8 bg-purple-100 rounded-full flex items-center justify-center mr-3">
                <span className="text-purple-600 font-bold text-sm">V</span>
              </div>
              <div>
                <p className="text-sm font-medium">
                  {hasVolumeFilter ? 'Filtered Volumes' : 'Total Volumes'}
                </p>
                <p className="text-2xl font-bold text-purple-600">
                  {allEnhancedNodes.reduce((sum, node) => sum + node.volumeCount, 0)}
                </p>
              </div>
            </div>
          </div>
          
          <div className="bg-white rounded-lg p-4 shadow">
            <div className="flex items-center">
              <div className="w-8 h-8 bg-indigo-100 rounded-full flex items-center justify-center mr-3">
                <span className="text-indigo-600 font-bold text-sm">D</span>
              </div>
              <div>
                <p className="text-sm font-medium">Total Disks</p>
                <p className="text-2xl font-bold text-indigo-600">
                  {allEnhancedNodes.reduce((sum, node) => sum + node.diskCount, 0)}
                </p>
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Active Filters Display */}
      {hasVolumeFilter && (
        <div className={`mb-6 p-4 rounded-lg border-2 ${severityInfo.bgColor} ${severityInfo.borderColor}`}>
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Filter className="w-4 h-4 text-blue-600" />
              <span className={`text-sm font-medium ${severityInfo.textColor}`}>
                Showing {getNodeFilterDescription(activeFilter)}
              </span>
            </div>
            {onClearFilter && (
              <button
                onClick={onClearFilter}
                className="text-blue-600 hover:text-blue-800 text-sm font-medium flex items-center gap-1"
              >
                <X className="w-3 h-3" />
                Clear Filter
              </button>
            )}
          </div>
          <div className="mt-2 text-sm text-blue-700">
            <div className="flex gap-4">
              <span>{allEnhancedNodes.length} of {totalNodes} {getNodeFilterDescription(activeFilter)}</span>
              <span>•</span>
              <span>{filteredVolumes.length} volume{filteredVolumes.length !== 1 ? 's' : ''} match this filter</span>
              {hasSearch && (
                <>
                  <span>•</span>
                  <span>Search: "{searchTerm}"</span>
                </>
              )}
            </div>
          </div>
        </div>
      )}

      {/* Search Results Display */}
      {hasSearch && !hasVolumeFilter && (
        <div className="mb-6 p-4 bg-gray-50 border border-gray-200 rounded-lg">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2">
              <Search className="w-4 h-4 text-gray-600" />
              <span className="text-sm font-medium text-gray-900">
                Search Results for "{searchTerm}"
              </span>
            </div>
            <button
              onClick={clearSearch}
              className="text-gray-600 hover:text-gray-800 text-sm font-medium flex items-center gap-1"
            >
              <X className="w-3 h-3" />
              Clear Search
            </button>
          </div>
          <div className="mt-2 text-sm text-gray-700">
            Found {allEnhancedNodes.length} of {totalNodes} nodes matching your search
          </div>
        </div>
      )}

      {/* Pagination Controls (Top) */}
      {totalPages > 1 && (
        <div className="mb-6 bg-white rounded-lg shadow p-4">
          <div className="flex items-center justify-between">
            <div className="text-sm text-gray-700">
              Showing {((currentPage - 1) * pageSize) + 1} to {Math.min(currentPage * pageSize, allEnhancedNodes.length)} of {allEnhancedNodes.length} nodes
            </div>
            <div className="flex items-center gap-2">
              <button
                onClick={() => setCurrentPage(prev => Math.max(1, prev - 1))}
                disabled={currentPage === 1}
                className="p-1 text-gray-500 hover:text-gray-700 disabled:opacity-50 disabled:cursor-not-allowed"
                title="Previous page"
              >
                <ChevronLeft className="w-5 h-5" />
              </button>
              <span className="px-3 py-1 text-sm bg-blue-100 text-blue-800 rounded">
                Page {currentPage} of {totalPages}
              </span>
              <button
                onClick={() => setCurrentPage(prev => Math.min(totalPages, prev + 1))}
                disabled={currentPage === totalPages}
                className="p-1 text-gray-500 hover:text-gray-700 disabled:opacity-50 disabled:cursor-not-allowed"
                title="Next page"
              >
                <ChevronRight className="w-5 h-5" />
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Results */}
      {allEnhancedNodes.length === 0 ? (
        <div className="text-center py-12">
          <div className="text-gray-500 mb-4">
            {hasSearch ? (
              <>
                <Search className="w-12 h-12 mx-auto mb-2 opacity-50" />
                <p className="text-lg font-medium">No nodes found</p>
                <p className="text-sm">No nodes match the search term "{searchTerm}"</p>
                <p className="text-xs text-gray-400 mt-2">
                  Try searching for node names, disk models, volume names, or PCI addresses
                </p>
              </>
            ) : hasVolumeFilter ? (
              <>
                <Filter className="w-12 h-12 mx-auto mb-2 opacity-50" />
                <p className="text-lg font-medium">No nodes found</p>
                <p className="text-sm">No nodes have volumes matching the "{getFilterDisplayName(activeFilter)}" filter.</p>
                {filteredVolumes.length > 0 && (
                  <p className="text-xs text-yellow-600 mt-2">
                    Found {filteredVolumes.length} matching volume{filteredVolumes.length !== 1 ? 's' : ''}, 
                    but {activeFilter === 'rebuilding' ? 'no rebuilding activity found on any nodes' : 
                         'no matching node conditions found'}.
                  </p>
                )}
              </>
            ) : (
              <>
                <Server className="w-12 h-12 mx-auto mb-2 opacity-50" />
                <p className="text-lg font-medium">No nodes available</p>
                <p className="text-sm">No nodes are currently configured in the system.</p>
              </>
            )}
          </div>
          {(onClearFilter || hasSearch) && (
            <div className="flex gap-2 justify-center">
              {hasSearch && (
                <button
                  onClick={clearSearch}
                  className="px-4 py-2 bg-gray-600 text-white rounded-md hover:bg-gray-700"
                >
                  Clear Search
                </button>
              )}
              {onClearFilter && hasVolumeFilter && (
                <button
                  onClick={onClearFilter}
                  className="px-4 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700"
                >
                  Clear Volume Filter
                </button>
              )}
            </div>
          )}
        </div>
      ) : (
        <div className="space-y-6">
          {paginatedNodes.map((nodeData) => (
            <NodeDetailView 
              key={nodeData.name} 
              node={nodeData.name}
              nodeDisks={nodeData.nodeDisks}
              nodeVolumes={nodeData.nodeVolumes}
              healthyDisks={nodeData.healthyDisks}
              totalCapacity={nodeData.totalCapacity}
              totalAllocated={nodeData.totalAllocated}
              totalFree={nodeData.totalFree}
              volumeFilter={activeFilter}
              filteredVolumes={hasVolumeFilter ? nodeData.nodeFilteredVolumes : undefined}
              onDiskVolumeFilter={onDiskVolumeFilter}
            />
          ))}
        </div>
      )}

      {/* Bottom Pagination */}
      {totalPages > 1 && (
        <div className="mt-6 bg-white rounded-lg shadow p-4">
          <div className="flex items-center justify-between">
            <div className="text-sm text-gray-700">
              Showing {((currentPage - 1) * pageSize) + 1} to {Math.min(currentPage * pageSize, allEnhancedNodes.length)} of {allEnhancedNodes.length} results
            </div>
            <div className="flex items-center gap-2">
              <button
                onClick={() => setCurrentPage(1)}
                disabled={currentPage === 1}
                className="px-3 py-1 text-sm border border-gray-300 rounded hover:bg-gray-50 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                First
              </button>
              <button
                onClick={() => setCurrentPage(prev => Math.max(1, prev - 1))}
                disabled={currentPage === 1}
                className="px-3 py-1 text-sm border border-gray-300 rounded hover:bg-gray-50 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                Previous
              </button>
              
              {/* Page numbers */}
              {Array.from({ length: Math.min(5, totalPages) }, (_, i) => {
                const pageNum = Math.max(1, Math.min(totalPages - 4, currentPage - 2)) + i;
                return (
                  <button
                    key={pageNum}
                    onClick={() => setCurrentPage(pageNum)}
                    className={`px-3 py-1 text-sm border rounded ${
                      pageNum === currentPage
                        ? 'bg-blue-600 text-white border-blue-600'
                        : 'border-gray-300 hover:bg-gray-50'
                    }`}
                  >
                    {pageNum}
                  </button>
                );
              })}
              
              <button
                onClick={() => setCurrentPage(prev => Math.min(totalPages, prev + 1))}
                disabled={currentPage === totalPages}
                className="px-3 py-1 text-sm border border-gray-300 rounded hover:bg-gray-50 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                Next
              </button>
              <button
                onClick={() => setCurrentPage(totalPages)}
                disabled={currentPage === totalPages}
                className="px-3 py-1 text-sm border border-gray-300 rounded hover:bg-gray-50 disabled:opacity-50 disabled:cursor-not-allowed"
              >
                Last
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Results Summary */}
      {allEnhancedNodes.length > 0 && (hasVolumeFilter || hasSearch) && (
        <div className="mt-6 p-4 bg-gray-50 rounded-lg">
          <h4 className="text-sm font-medium text-gray-700 mb-2">Results Summary</h4>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-4 text-xs">
            <div>
              <span className="text-gray-600">Nodes Shown:</span>
              <div className="font-medium text-gray-900">{allEnhancedNodes.length} of {totalNodes}</div>
            </div>
            <div>
              <span className="text-gray-600">Total Capacity:</span>
              <div className="font-medium text-gray-900">
                {allEnhancedNodes.reduce((sum, node) => sum + node.totalCapacity, 0).toLocaleString()}GB
              </div>
            </div>
            <div>
              <span className="text-gray-600">Volume Count:</span>
              <div className="font-medium text-gray-900">
                {allEnhancedNodes.reduce((sum, node) => sum + node.volumeCount, 0)} volumes
              </div>
            </div>
            <div>
              <span className="text-gray-600">Healthy Disks:</span>
              <div className="font-medium text-gray-900">
                {allEnhancedNodes.reduce((sum, node) => sum + node.healthyDisks, 0)} of {allEnhancedNodes.reduce((sum, node) => sum + node.diskCount, 0)}
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};
            