import React, { useMemo, useState } from 'react';
import { useSearchParams } from 'react-router';
import { Filter, X, Search, Server } from 'lucide-react';
import { NodeDetailView } from './NodeDetailView';
import type { DashboardData, Disk, Volume, VolumeFilter } from '../../hooks/useDashboardData';
import { filterVolumesByType } from '../../hooks/useDashboardData';
import {
  useNodesFleet,
  sortFleetNodes,
  matchesFacet,
  facetCounts,
  type FleetFacet,
  type FleetNode,
  type FleetSort,
} from '../../hooks/useNodesFleet';
import { volumeFilterDisplay, nodeHealthStyle, NODE_HEALTH_STYLES } from '../ui/status';
import { AsyncView } from '../ui/AsyncView';
import { TabSkeleton } from '../ui/Skeleton';

// Fleet-scale nodes view: the landing surface reads the /api/nodes rollup
// (grows with node count only) — health facets, a status-cell heatmap, and
// a problems-first list of one-line node rows. Per-disk/per-volume detail
// comes from the aggregate only when a node is drilled into (?node=).

interface NodesFleetViewProps {
  data: DashboardData;
  activeFilter?: VolumeFilter;
  onClearFilter?: () => void;
  onDiskVolumeFilter?: (diskId: string) => void;
  showNodesWithDisksOnly?: boolean;
  onShowNodesWithDisksOnlyChange?: (enabled: boolean) => void;
}

const FACETS: { key: FleetFacet; label: string }[] = [
  { key: 'all', label: 'All' },
  { key: 'critical', label: NODE_HEALTH_STYLES.critical.label },
  { key: 'warning', label: NODE_HEALTH_STYLES.warning.label },
  { key: 'ok', label: NODE_HEALTH_STYLES.ok.label },
  { key: 'uninitialized', label: 'Uninit. disks' },
];

export const NodesFleetView: React.FC<NodesFleetViewProps> = ({
  data,
  activeFilter,
  onClearFilter,
  onDiskVolumeFilter,
  showNodesWithDisksOnly = false,
  onShowNodesWithDisksOnlyChange,
}) => {
  const nodesQuery = useNodesFleet();
  const [searchParams, setSearchParams] = useSearchParams();
  const selectedNode = searchParams.get('node');
  const [facet, setFacet] = useState<FleetFacet>('all');
  const [sort, setSort] = useState<FleetSort>('problems');
  const [searchTerm, setSearchTerm] = useState('');

  const hasVolumeFilter = activeFilter && activeFilter !== 'all';
  const filteredVolumes = useMemo(
    () => (activeFilter ? filterVolumesByType(data.volumes, activeFilter) : data.volumes),
    [data.volumes, activeFilter]
  );

  // Aggregate lookups for the drill-in body and deep search.
  const disksByNode = useMemo(() => {
    const map = new Map<string, Disk[]>();
    for (const disk of data.disks) {
      const list = map.get(disk.node);
      if (list) list.push(disk);
      else map.set(disk.node, [disk]);
    }
    return map;
  }, [data.disks]);

  const volumesByNode = useMemo(() => {
    const map = new Map<string, Volume[]>();
    for (const volume of data.volumes) {
      for (const node of volume.nodes) {
        const list = map.get(node);
        if (list) list.push(volume);
        else map.set(node, [volume]);
      }
    }
    return map;
  }, [data.volumes]);

  // Search matches node names plus what the aggregate knows about the node
  // (disk id/model/PCI, volume name/id) when it has that node.
  const searchBlobs = useMemo(() => {
    const map = new Map<string, string>();
    for (const node of data.nodes) {
      const disks = disksByNode.get(node) ?? [];
      const volumes = volumesByNode.get(node) ?? [];
      map.set(
        node,
        [
          node,
          ...disks.flatMap(d => [d.id, d.model, d.pci_addr]),
          ...volumes.flatMap(v => [v.name, v.id]),
        ]
          .join(' ')
          .toLowerCase()
      );
    }
    return map;
  }, [data.nodes, disksByNode, volumesByNode]);

  const selectNode = (name: string, scrollTo = false) => {
    setSearchParams(prev => {
      const next = new URLSearchParams(prev);
      if (prev.get('node') === name) next.delete('node');
      else next.set('node', name);
      return next;
    });
    if (scrollTo) {
      // After the row expands; smooth-scroll it under the heatmap.
      setTimeout(() => {
        document
          .getElementById(`node-row-${name}`)
          ?.scrollIntoView({ behavior: 'smooth', block: 'start' });
      }, 0);
    }
  };

  const matchesSearch = (node: FleetNode) => {
    const term = searchTerm.trim().toLowerCase();
    if (!term) return true;
    return (searchBlobs.get(node.name) ?? node.name.toLowerCase()).includes(term);
  };

  const getFilterDisplayName = (filter: VolumeFilter) => volumeFilterDisplay(filter).name;

  return (
    <AsyncView
      loading={nodesQuery.isPending}
      error={nodesQuery.error ? String(nodesQuery.error) : null}
      data={nodesQuery.data}
      hasData={d => d.nodes.length > 0}
      emptyTitle="No nodes discovered"
      emptyHint="Nodes appear here when flint node agents register with the backend."
      onRetry={() => nodesQuery.refetch()}
      skeleton={<TabSkeleton />}
    >
      {({ nodes }) => {
        const counts = facetCounts(nodes);
        const visible = sortFleetNodes(
          nodes
            .filter(n => matchesFacet(n, facet))
            .filter(matchesSearch)
            .filter(n => !showNodesWithDisksOnly || n.disks_total > 0),
          sort
        );

        return (
          <div>
            {/* Controls: facets are the primary navigation */}
            <div className="mb-4 bg-white rounded-lg shadow p-4 space-y-3">
              <div className="flex flex-wrap items-center gap-2">
                <Server className="w-5 h-5 text-gray-600" />
                <h3 className="text-section text-gray-900 mr-2">Nodes</h3>
                {FACETS.map(({ key, label }) => {
                  const active = facet === key;
                  const count = counts[key];
                  return (
                    <button
                      key={key}
                      onClick={() => setFacet(active && key !== 'all' ? 'all' : key)}
                      aria-pressed={active}
                      className={`px-2.5 py-1 text-xs font-medium rounded-full border transition-colors ${
                        active
                          ? 'bg-blue-600 text-white border-blue-600'
                          : count === 0
                            ? 'bg-gray-50 text-gray-400 border-gray-200'
                            : 'bg-white text-gray-700 border-gray-300 hover:bg-gray-50'
                      }`}
                    >
                      {label} · {count}
                    </button>
                  );
                })}
                <div className="ml-auto flex items-center gap-3">
                  {onShowNodesWithDisksOnlyChange && (
                    <label className="flex items-center gap-2 cursor-pointer text-sm text-gray-700">
                      <input
                        type="checkbox"
                        checked={showNodesWithDisksOnly}
                        onChange={e => onShowNodesWithDisksOnlyChange(e.target.checked)}
                        className="w-4 h-4 text-blue-600 border-gray-300 rounded focus:ring-blue-500"
                      />
                      Only nodes with disks
                    </label>
                  )}
                  <select
                    value={sort}
                    onChange={e => setSort(e.target.value as FleetSort)}
                    aria-label="Sort nodes"
                    className="border border-gray-300 rounded px-2 py-1 text-sm"
                  >
                    <option value="problems">Problems first</option>
                    <option value="name">Name</option>
                    <option value="capacity">Capacity</option>
                    <option value="volumes">Volumes</option>
                  </select>
                </div>
              </div>

              <div className="relative">
                <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
                <input
                  type="text"
                  placeholder="Search nodes by name, disk model/ID, volume name, or PCI address..."
                  value={searchTerm}
                  onChange={e => setSearchTerm(e.target.value)}
                  className="w-full pl-10 pr-10 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-blue-500"
                />
                {searchTerm && (
                  <button
                    onClick={() => setSearchTerm('')}
                    aria-label="Clear search"
                    className="absolute right-3 top-1/2 transform -translate-y-1/2 text-gray-400 hover:text-gray-600"
                  >
                    <X className="w-4 h-4" />
                  </button>
                )}
              </div>
            </div>

            {/* Volume-filter context from other tabs */}
            {hasVolumeFilter && (
              <div className="mb-4 p-3 rounded-lg border bg-blue-50 border-blue-200 flex items-center justify-between">
                <div className="flex items-center gap-2 text-sm text-blue-900">
                  <Filter className="w-4 h-4 text-blue-600" />
                  <span>
                    Volume filter: <strong>{getFilterDisplayName(activeFilter)}</strong> —{' '}
                    {filteredVolumes.length} matching volume{filteredVolumes.length !== 1 ? 's' : ''};
                    per-node matches shown on each row
                  </span>
                </div>
                {onClearFilter && (
                  <button
                    onClick={onClearFilter}
                    className="text-blue-600 hover:text-blue-800 text-sm font-medium flex items-center gap-1"
                  >
                    <X className="w-3 h-3" />
                    Clear
                  </button>
                )}
              </div>
            )}

            {/* Fleet heatmap: one status cell per node, problems first */}
            <div className="mb-4 bg-white rounded-lg shadow p-4">
              <div className="flex items-center justify-between mb-3">
                <h4 className="text-sm font-medium text-gray-700">Fleet health</h4>
                <div className="flex items-center gap-3 text-xs text-gray-500">
                  {(['critical', 'warning', 'ok'] as const).map(h => (
                    <span key={h} className="flex items-center gap-1">
                      <span className={`w-2.5 h-2.5 rounded-sm ${NODE_HEALTH_STYLES[h].cell}`} />
                      {NODE_HEALTH_STYLES[h].label}
                    </span>
                  ))}
                </div>
              </div>
              <div className="flex flex-wrap gap-1">
                {visible.map(n => {
                  const style = nodeHealthStyle(n.health);
                  const selected = selectedNode === n.name;
                  return (
                    <button
                      key={n.name}
                      onClick={() => selectNode(n.name, true)}
                      aria-label={`${n.name}: ${style.label}`}
                      title={`${n.name} — ${style.label}\ndisks ${n.disks_healthy}/${n.disks_total} healthy · ${n.volumes_total} volumes · ${n.replicas_out_of_sync} out of sync`}
                      className={`w-5 h-5 rounded-sm ${style.cell} hover:ring-2 hover:ring-blue-400 focus-visible:outline focus-visible:outline-2 focus-visible:outline-blue-600 ${
                        selected ? 'ring-2 ring-offset-1 ring-blue-600' : ''
                      }`}
                    />
                  );
                })}
                {visible.length === 0 && (
                  <span className="text-sm text-gray-500">No nodes match the current filters.</span>
                )}
              </div>
            </div>

            {/* Node rows: one line each, drill in for detail. content-visibility
                keeps offscreen rows out of layout/paint at fleet scale. */}
            <div className="space-y-2">
              {visible.map(n => {
                const nodeFiltered = hasVolumeFilter
                  ? filteredVolumes.filter(v => v.nodes.includes(n.name))
                  : undefined;
                return (
                  <div
                    key={n.name}
                    id={`node-row-${n.name}`}
                    style={{ contentVisibility: 'auto', containIntrinsicSize: '0 52px' }}
                  >
                    <NodeDetailView
                      node={n.name}
                      summary={n}
                      nodeDisks={disksByNode.get(n.name) ?? []}
                      nodeVolumes={volumesByNode.get(n.name) ?? []}
                      volumeFilter={activeFilter}
                      filteredVolumes={nodeFiltered}
                      onDiskVolumeFilter={onDiskVolumeFilter}
                      nodeInfo={data.node_info?.[n.name]}
                      expanded={selectedNode === n.name}
                      onToggle={() => selectNode(n.name)}
                    />
                  </div>
                );
              })}
            </div>

            <p className="mt-4 text-sm text-gray-500">
              Showing {visible.length} of {nodes.length} node{nodes.length !== 1 ? 's' : ''}
            </p>
          </div>
        );
      }}
    </AsyncView>
  );
};
