import type { Edge, Node } from 'reactflow';
import type { Disk, NodeInfo, Volume } from '../../../hooks/useDashboardData';
import { isReplicaRecovering } from '../../../hooks/useDashboardData';
import { memberStateStyle } from '../../ui/status';

// Pure cluster → graph projection: the second altitude of the topology
// view. One card per k8s node, laid out on a grid; an edge between two
// nodes means they share at least one volume's replicas (the spatial
// answer to "where does this data live?"). Deterministic like the
// volume-level layout: nodes sort by name, never jump between refreshes.
//
// Scale target is 50 nodes / 500 volumes (improvement plan): volumes are
// never individual graph nodes here — they aggregate onto node cards and
// pair edges, and the drawer lists them on demand.

export interface ClusterNodeInfo {
  name: string;
  info: NodeInfo | null;
  disks: Disk[];
  // Volumes with at least one replica on this node.
  volumes: Volume[];
  replicaCount: number;
}

export interface NodePairLink {
  a: string;
  b: string;
  volumes: Volume[];
  // Worst state across the shared volumes (drives edge color).
  worstState: 'Healthy' | 'Degraded' | 'Failed';
  recovering: boolean;
}

export type ClusterDetail =
  | { kind: 'cluster-node'; node: ClusterNodeInfo }
  | { kind: 'cluster-link'; link: NodePairLink };

export interface ClusterNodeData {
  node: ClusterNodeInfo;
  // One ring segment per disk (healthy/failed/uninitialized); single gray
  // segment for diskless nodes.
  ringHexes: string[];
  detail: ClusterDetail;
}

export interface ClusterEdgeData {
  detail: ClusterDetail;
}

export type ClusterGraphNode = Node<ClusterNodeData>;
export type ClusterGraphEdge = Edge<ClusterEdgeData>;

// Above this many pair edges the densest links win and the rest drop; the
// caller must surface truncatedLinks so the cap is never silent.
export const MAX_LINK_EDGES = 150;

const NODE_W = 300;
const NODE_H = 190;
const GUTTER_X = 120;
const GUTTER_Y = 90;

const DISK_HEALTHY_HEX = '#059669';
const DISK_FAILED_HEX = '#dc2626';
const DISK_UNINIT_HEX = '#9ca3af';
const DISKLESS_HEX = '#e5e7eb';

function worstVolumeState(volumes: Volume[]): NodePairLink['worstState'] {
  let worst: NodePairLink['worstState'] = 'Healthy';
  for (const v of volumes) {
    if (v.state === 'Failed') return 'Failed';
    if (v.state === 'Degraded') worst = 'Degraded';
  }
  return worst;
}

// Union of every node the data mentions: the backend node list, disk
// owners, and replica placements — a node must not vanish from the map
// just because one source omits it.
export function collectClusterNodes(
  volumes: Volume[],
  disks: Disk[],
  nodeNames: string[],
  nodeInfo?: Record<string, NodeInfo>
): ClusterNodeInfo[] {
  const names = new Set<string>(nodeNames);
  for (const d of disks) names.add(d.node);
  for (const v of volumes) for (const r of v.replica_statuses) names.add(r.node);

  return [...names].sort().map(name => {
    const nodeVolumes = volumes.filter(v => v.replica_statuses.some(r => r.node === name));
    return {
      name,
      info: nodeInfo?.[name] ?? null,
      disks: disks.filter(d => d.node === name),
      volumes: nodeVolumes,
      replicaCount: volumes.reduce(
        (sum, v) => sum + v.replica_statuses.filter(r => r.node === name).length,
        0
      ),
    };
  });
}

// Unordered node pairs sharing ≥1 volume's replicas, densest first.
export function collectNodeLinks(volumes: Volume[]): NodePairLink[] {
  const byPair = new Map<string, { a: string; b: string; volumes: Volume[] }>();
  for (const v of volumes) {
    const nodes = [...new Set(v.replica_statuses.map(r => r.node))].sort();
    for (let i = 0; i < nodes.length; i++) {
      for (let j = i + 1; j < nodes.length; j++) {
        const key = `${nodes[i]}|${nodes[j]}`;
        const entry = byPair.get(key) ?? { a: nodes[i]!, b: nodes[j]!, volumes: [] };
        entry.volumes.push(v);
        byPair.set(key, entry);
      }
    }
  }
  return [...byPair.values()]
    .map(({ a, b, volumes: shared }) => ({
      a,
      b,
      volumes: shared,
      worstState: worstVolumeState(shared),
      recovering: shared.some(v => v.replica_statuses.some(isReplicaRecovering)),
    }))
    .sort((x, y) => y.volumes.length - x.volumes.length || x.a.localeCompare(y.a));
}

export function buildClusterTopology(
  volumes: Volume[],
  disks: Disk[],
  nodeNames: string[],
  nodeInfo?: Record<string, NodeInfo>
): {
  nodes: ClusterGraphNode[];
  edges: ClusterGraphEdge[];
  truncatedLinks: number;
} {
  const clusterNodes = collectClusterNodes(volumes, disks, nodeNames, nodeInfo);
  const links = collectNodeLinks(volumes);

  const cols = Math.max(1, Math.ceil(Math.sqrt(clusterNodes.length)));

  const nodes: ClusterGraphNode[] = clusterNodes.map((node, i) => {
    const ringHexes =
      node.disks.length > 0
        ? node.disks.map(d =>
            !d.healthy
              ? DISK_FAILED_HEX
              : d.blobstore_initialized
                ? DISK_HEALTHY_HEX
                : DISK_UNINIT_HEX
          )
        : [DISKLESS_HEX];
    return {
      id: `node-${node.name}`,
      type: 'clusterNode',
      position: {
        x: (i % cols) * (NODE_W + GUTTER_X),
        y: Math.floor(i / cols) * (NODE_H + GUTTER_Y),
      },
      data: { node, ringHexes, detail: { kind: 'cluster-node', node } },
    };
  });

  const kept = links.slice(0, MAX_LINK_EDGES);
  const edges: ClusterGraphEdge[] = kept.map(link => {
    const hex =
      link.worstState === 'Healthy'
        ? memberStateStyle('online').hex
        : link.worstState === 'Degraded'
          ? memberStateStyle('degraded').hex
          : memberStateStyle('failed').hex;
    return {
      id: `link-${link.a}-${link.b}`,
      source: `node-${link.a}`,
      target: `node-${link.b}`,
      animated: link.recovering,
      label: `${link.volumes.length} vol${link.volumes.length === 1 ? '' : 's'}`,
      labelStyle: { fill: '#4b5563', fontSize: 11 },
      labelBgStyle: { fill: '#ffffff', fillOpacity: 0.85 },
      style: {
        stroke: hex,
        strokeWidth: Math.min(2 + Math.log2(link.volumes.length), 5),
      },
      data: { detail: { kind: 'cluster-link', link } },
    };
  });

  return { nodes, edges, truncatedLinks: links.length - kept.length };
}

// Kept for the drawer: capacity totals across a node's disks (GB, as the
// API reports them).
export function nodeCapacity(node: ClusterNodeInfo): { totalGb: number; freeGb: number } {
  return {
    totalGb: node.disks.reduce((s, d) => s + d.capacity_gb, 0),
    freeGb: node.disks.reduce((s, d) => s + d.free_space, 0),
  };
}
