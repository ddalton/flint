import { Handle, Position, type NodeProps } from 'reactflow';
import { Server } from 'lucide-react';
import { StatusRing } from './TopologyNodes';
import { nodeCapacity } from './buildClusterTopology';
import type { ClusterNodeData } from './buildClusterTopology';

// The cluster-altitude node card: one per k8s node. The ring is the
// node's disks (green initialized / gray uninitialized / red unhealthy);
// counts summarize what the drawer itemizes. Same read-only handle rules
// as the volume-level nodes.

const HIDDEN_HANDLE = '!opacity-0 !pointer-events-none !w-1 !h-1 !min-w-0 !min-h-0 !border-0';

export function ClusterTopologyNode({ data, selected }: NodeProps<ClusterNodeData>) {
  const { node, ringHexes } = data;
  const { totalGb, freeGb } = nodeCapacity(node);
  const unhealthyDisks = node.disks.filter(d => !d.healthy).length;
  const brokenVolumes = node.volumes.filter(v => v.state !== 'Healthy').length;
  return (
    <div
      className={`w-72 rounded-lg border-2 bg-white px-3 py-2.5 text-left shadow-sm transition-shadow ${
        selected
          ? 'border-brand-500 shadow-md ring-2 ring-brand-200'
          : 'border-gray-200 hover:border-gray-300'
      }`}
    >
      <div className="flex items-start gap-3">
        <StatusRing hexes={ringHexes} icon={Server} />
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-semibold text-gray-800" title={node.name}>
            {node.name}
          </p>
          <p className="text-xs text-gray-500">
            {node.disks.length} disk{node.disks.length === 1 ? '' : 's'}
            {unhealthyDisks > 0 && (
              <span className="font-medium text-failed-600"> ({unhealthyDisks} unhealthy)</span>
            )}
            {totalGb > 0 && ` • ${Math.round(freeGb)}/${Math.round(totalGb)} GB free`}
          </p>
          <p className="mt-1 text-xs tabular-nums text-gray-600">
            {node.replicaCount} replica{node.replicaCount === 1 ? '' : 's'} •{' '}
            {node.volumes.length} volume{node.volumes.length === 1 ? '' : 's'}
            {brokenVolumes > 0 && (
              <span className="font-medium text-degraded-700"> ({brokenVolumes} not healthy)</span>
            )}
          </p>
        </div>
      </div>
      <Handle type="target" position={Position.Left} className={HIDDEN_HANDLE} />
      <Handle type="source" position={Position.Right} className={HIDDEN_HANDLE} />
    </div>
  );
}
