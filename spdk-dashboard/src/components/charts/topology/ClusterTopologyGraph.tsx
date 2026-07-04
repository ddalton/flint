import { useEffect, useMemo, useState } from 'react';
import ReactFlow, { Background, BackgroundVariant, Controls, Panel } from 'reactflow';
import 'reactflow/dist/style.css';
import type { Disk, NodeInfo, Volume } from '../../../hooks/useDashboardData';
import { buildClusterTopology } from './buildClusterTopology';
import { ClusterTopologyNode } from './ClusterNodes';
import { ClusterDrawer } from './ClusterDrawer';

// The cluster-altitude canvas: node cards on a grid, replica-placement
// links between them. Same selection contract as the volume graph
// (selection is an id, drawer reads fresh data each rebuild); volume rows
// in the drawer hand off to the volume view via onOpenVolume.

const clusterNodeTypes = { clusterNode: ClusterTopologyNode };

export function ClusterTopologyGraph({
  volumes,
  disks,
  nodeNames,
  nodeInfo,
  onOpenVolume,
}: {
  volumes: Volume[];
  disks: Disk[];
  nodeNames: string[];
  nodeInfo?: Record<string, NodeInfo>;
  onOpenVolume: (volumeId: string) => void;
}) {
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const { nodes, edges, truncatedLinks } = useMemo(
    () => buildClusterTopology(volumes, disks, nodeNames, nodeInfo),
    [volumes, disks, nodeNames, nodeInfo]
  );

  const detail = useMemo(
    () =>
      nodes.find(n => n.id === selectedId)?.data.detail ??
      edges.find(e => e.id === selectedId)?.data?.detail ??
      null,
    [nodes, edges, selectedId]
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
        nodes={displayNodes}
        edges={edges}
        nodeTypes={clusterNodeTypes}
        fitView
        fitViewOptions={{ padding: 0.15, maxZoom: 1 }}
        minZoom={0.3}
        nodesDraggable={false}
        nodesConnectable={false}
        zoomOnScroll={false}
        preventScrolling={false}
        onNodeClick={(_, node) => setSelectedId(node.id)}
        onEdgeClick={(_, edge) => setSelectedId(edge.id)}
        onPaneClick={() => setSelectedId(null)}
      >
        <Background variant={BackgroundVariant.Dots} gap={16} color="#e5e7eb" />
        <Controls showInteractive={false} position="bottom-right" />
        <Panel position="bottom-left" className="!m-2">
          <div className="flex items-center gap-3 rounded-md border border-gray-200 bg-white/90 px-2.5 py-1.5 text-[11px] text-gray-600 shadow-sm">
            <span>link = nodes sharing volume replicas (thicker = more)</span>
            <span>marching dashes = recovery</span>
            {truncatedLinks > 0 && (
              <span className="font-medium text-amber-700">
                {truncatedLinks} least-shared links hidden
              </span>
            )}
          </div>
        </Panel>
      </ReactFlow>
      {detail && (
        <ClusterDrawer
          detail={detail}
          onClose={() => setSelectedId(null)}
          onOpenVolume={onOpenVolume}
        />
      )}
    </div>
  );
}
