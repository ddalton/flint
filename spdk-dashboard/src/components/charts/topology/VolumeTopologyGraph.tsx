import { useEffect, useMemo, useState } from 'react';
import ReactFlow, { Background, BackgroundVariant, Controls, Panel } from 'reactflow';
import 'reactflow/dist/style.css';
import { Info } from 'lucide-react';
import type { Disk, Volume } from '../../../hooks/useDashboardData';
import { buildTopology } from './buildTopology';
import {
  AccessTopologyNode,
  AppTopologyNode,
  DiskTopologyNode,
  MemberTopologyNode,
  RaidTopologyNode,
} from './TopologyNodes';
import { TopologyDrawer } from './TopologyDrawer';

// The topology canvas: a deterministic left→right data-path graph with a
// details drawer. Selection is held as a node id (not a snapshot of data),
// so each auto-refresh rebuild feeds the drawer fresh state; if the selected
// entity disappears, the drawer simply closes.

const ABOUT = '__about__';

// Module scope — React Flow requires a stable nodeTypes identity.
const topologyNodeTypes = {
  app: AppTopologyNode,
  access: AccessTopologyNode,
  raid: RaidTopologyNode,
  member: MemberTopologyNode,
  disk: DiskTopologyNode,
};

export function VolumeTopologyGraph({ volume, disks }: { volume: Volume; disks: Disk[] }) {
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const { nodes, edges } = useMemo(() => buildTopology(volume, disks), [volume, disks]);

  // A different volume is a different diagram — drop the selection.
  useEffect(() => {
    setSelectedId(null);
  }, [volume.id]);

  const detail = useMemo(
    () =>
      selectedId === ABOUT
        ? ({ kind: 'about' } as const)
        : nodes.find(n => n.id === selectedId)?.data.detail ?? null,
    [nodes, selectedId]
  );

  useEffect(() => {
    if (!detail) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setSelectedId(null);
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [detail]);

  const displayNodes = useMemo(
    () => nodes.map(n => ({ ...n, selected: n.id === selectedId })),
    [nodes, selectedId]
  );

  return (
    <div className="relative h-[560px] overflow-hidden rounded-lg border border-gray-200 bg-gray-50/50">
      <ReactFlow
        key={volume.id}
        nodes={displayNodes}
        edges={edges}
        nodeTypes={topologyNodeTypes}
        fitView
        fitViewOptions={{ padding: 0.15, maxZoom: 1 }}
        minZoom={0.3}
        nodesDraggable={false}
        nodesConnectable={false}
        zoomOnScroll={false}
        preventScrolling={false}
        onNodeClick={(_, node) => setSelectedId(node.id)}
        onEdgeClick={(_, edge) => {
          if (edge.data?.detailRef) setSelectedId(edge.data.detailRef);
        }}
        onPaneClick={() => setSelectedId(null)}
      >
        <Background variant={BackgroundVariant.Dots} gap={16} color="#e5e7eb" />
        <Controls showInteractive={false} position="bottom-right" />
        <Panel position="top-right" className="!m-2">
          <button
            onClick={() => setSelectedId(ABOUT)}
            className="flex items-center gap-1.5 rounded-md border border-gray-200 bg-white/90 px-2.5 py-1.5 text-xs font-medium text-gray-600 shadow-sm hover:bg-gray-50 hover:text-gray-800"
          >
            <Info className="h-3.5 w-3.5" aria-hidden="true" />
            About this topology
          </button>
        </Panel>
        <Panel position="bottom-left" className="!m-2">
          <div className="flex items-center gap-3 rounded-md border border-gray-200 bg-white/90 px-2.5 py-1.5 text-[11px] text-gray-600 shadow-sm">
            <span className="flex items-center gap-1.5">
              <svg width="18" height="6" aria-hidden="true">
                <line x1="0" y1="3" x2="18" y2="3" stroke="#6b7280" strokeWidth="2" />
              </svg>
              local
            </span>
            <span className="flex items-center gap-1.5">
              <svg width="18" height="6" aria-hidden="true">
                <line x1="0" y1="3" x2="18" y2="3" stroke="#6b7280" strokeWidth="2" strokeDasharray="4 3" />
              </svg>
              NVMe-oF
            </span>
            <span>marching dashes = recovery</span>
          </div>
        </Panel>
      </ReactFlow>
      {detail && (
        <TopologyDrawer volume={volume} detail={detail} onClose={() => setSelectedId(null)} />
      )}
    </div>
  );
}
