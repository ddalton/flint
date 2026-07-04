import type { Edge, Node } from 'reactflow';
import type { Disk, RaidMember, ReplicaStatus, Volume } from '../../../hooks/useDashboardData';
import { isReplicaRecovering } from '../../../hooks/useDashboardData';
import { memberStateStyle } from '../../ui/status';

// Pure volume → graph projection for the topology view. Everything visual
// (node positions, edge colors/dashes/animation) is decided here so it can
// be unit-tested without rendering React Flow; the components only draw
// what this returns.
//
// Layout is a fixed left→right layered DAG — the data path — NOT
// force-directed: consumer → access device → RAID bdev → members → disks.
// Deterministic positions mean nothing jumps between refreshes.

// One RAID slot joined with its replica status and backing disk. slot is
// null for a replica the RAID hasn't assembled (standby / brand-new).
// raidPresent distinguishes "not assembled into the RAID" from "this volume
// reports no RAID at all" — only the former deserves that label.
export interface MemberJoin {
  slot: number | null;
  member: RaidMember | null;
  replica: ReplicaStatus | null;
  disk: Disk | null;
  isRebuildTarget: boolean;
  isRebuildSource: boolean;
  raidPresent: boolean;
}

export type TopologyDetail =
  | { kind: 'app' }
  | { kind: 'access' }
  | { kind: 'raid' }
  | { kind: 'member'; join: MemberJoin }
  | { kind: 'disk'; disk: Disk; join: MemberJoin }
  | { kind: 'about' };

export interface AppNodeData {
  volume: Volume;
  detail: TopologyDetail;
}
export interface AccessNodeData {
  volume: Volume;
  detail: TopologyDetail;
}
export interface RaidNodeData {
  volume: Volume;
  // One ring segment per member, colored by its state.
  ringHexes: string[];
  detail: TopologyDetail;
}
export interface MemberNodeData {
  join: MemberJoin;
  // Rebuild progress for the target slot; the node shows an inline bar.
  rebuildPct: number | null;
  detail: TopologyDetail;
}
export interface DiskNodeData {
  disk: Disk;
  detail: TopologyDetail;
}
export type TopologyNodeData =
  | AppNodeData
  | AccessNodeData
  | RaidNodeData
  | MemberNodeData
  | DiskNodeData;

// Edge clicks open the drawer for the entity the edge leads to.
export interface TopologyEdgeData {
  detailRef: string;
}

export type TopologyNode = Node<TopologyNodeData>;
export type TopologyEdge = Edge<TopologyEdgeData>;

export interface MemberVisual {
  state: string;
  hex: string;
  dashed: boolean;
  recovering: boolean;
}

// Edge/ring visual state for one member: member state wins (it is what the
// RAID actually assembled), replica status fills in for unassembled
// replicas. Recovery animates: legacy rebuild or Tier-2 non-in_sync.
export function memberVisual(join: MemberJoin): MemberVisual {
  const state = (join.member?.state ?? join.replica?.status ?? 'unknown').toLowerCase();
  const recovering =
    state === 'rebuilding' ||
    join.isRebuildTarget ||
    (join.replica ? isReplicaRecovering(join.replica) : false);
  return {
    state,
    hex: memberStateStyle(state).hex,
    dashed: join.replica ? !join.replica.is_local : false,
    recovering,
  };
}

export function raidLevelDisplayName(raidLevel: number): string {
  switch (raidLevel) {
    case 0:
      return 'RAID-0 (Striping)';
    case 1:
      return 'RAID-1 (Mirroring)';
    case 5:
      return 'RAID-5 (Distributed Parity)';
    case 6:
      return 'RAID-6 (Dual Parity)';
    case 10:
      return 'RAID-10 (Striped Mirrors)';
    default:
      return `RAID-${raidLevel}`;
  }
}

// RAID slots (sorted) joined with replicas by raid_member_slot; replicas the
// RAID doesn't know about yet are appended slot-less. Disks resolve through
// what the API actually sends — the replica's node plus the disk's
// provisioned_volumes — not the never-sent disk_ref the old cards keyed on.
export function joinMembers(volume: Volume, disks: Disk[]): MemberJoin[] {
  const raid = volume.raid_status ?? null;
  const rebuild = raid?.rebuild_info ?? null;
  const joins: MemberJoin[] = [];
  const claimed = new Set<ReplicaStatus>();

  const backingDisk = (replica: ReplicaStatus | null): Disk | null => {
    if (!replica) return null;
    return (
      disks.find(
        d =>
          d.node === replica.node &&
          d.provisioned_volumes.some(pv => pv.volume_id === volume.id)
      ) ?? null
    );
  };

  if (raid) {
    const members = [...raid.members].sort((a, b) => a.slot - b.slot);
    for (const member of members) {
      const replica =
        volume.replica_statuses.find(r => r.raid_member_slot === member.slot) ?? null;
      if (replica) claimed.add(replica);
      joins.push({
        slot: member.slot,
        member,
        replica,
        disk: backingDisk(replica),
        isRebuildTarget: rebuild?.target_slot === member.slot,
        isRebuildSource: rebuild?.source_slot === member.slot,
        raidPresent: true,
      });
    }
  }

  for (const replica of volume.replica_statuses) {
    if (claimed.has(replica)) continue;
    joins.push({
      slot: replica.raid_member_slot ?? null,
      member: null,
      replica,
      disk: backingDisk(replica),
      isRebuildTarget: false,
      isRebuildSource: false,
      raidPresent: raid !== null,
    });
  }

  return joins;
}

// Layer x-positions and estimated node heights for vertical centering.
// fitView absorbs the remaining imprecision.
// Column gaps are sized for the bezier edges: too little horizontal room
// between the RAID node's right edge and the member column makes the curves
// kink back on themselves (seen live at 830).
const COL_X = { app: 0, access: 250, raid: 505, member: 930, disk: 1275 } as const;
const ROW_H = 150;
const APP_H = 64;
const ACCESS_H = 64;
const RAID_H = 130;
const MEMBER_H = 104;
const DISK_H = 84;

const STRUCTURAL_STROKE = '#9ca3af';
const EDGE_LABEL_STYLE = { fill: '#4b5563', fontSize: 11 } as const;
const EDGE_LABEL_BG = { fill: '#ffffff', fillOpacity: 0.85 } as const;

export function buildTopology(volume: Volume, disks: Disk[]): {
  nodes: TopologyNode[];
  edges: TopologyEdge[];
} {
  const joins = joinMembers(volume, disks);
  const raid = volume.raid_status ?? null;
  const rebuild = raid?.rebuild_info ?? null;

  const memberMid =
    joins.length > 0 ? ((joins.length - 1) * ROW_H + MEMBER_H) / 2 : RAID_H / 2;

  const nodes: TopologyNode[] = [];
  const edges: TopologyEdge[] = [];

  // --- Consumer + access layers -----------------------------------------
  nodes.push({
    id: 'app',
    type: 'app',
    position: { x: COL_X.app, y: memberMid - APP_H / 2 },
    data: { volume, detail: { kind: 'app' } },
  });

  const hasUblk = !!volume.ublk_device;
  const hasAccess = hasUblk || volume.nvmeof_targets.length > 0;
  if (hasAccess) {
    nodes.push({
      id: 'access',
      type: 'access',
      position: { x: COL_X.access, y: memberMid - ACCESS_H / 2 },
      data: { volume, detail: { kind: 'access' } },
    });
    edges.push({
      id: 'e-app-access',
      source: 'app',
      target: 'access',
      style: { stroke: STRUCTURAL_STROKE, strokeWidth: 2 },
      data: { detailRef: 'access' },
    });
  }

  // --- RAID bdev ---------------------------------------------------------
  const ringHexes =
    joins.length > 0
      ? joins.map(j => memberVisual(j).hex)
      : [memberStateStyle('unknown').hex];
  nodes.push({
    id: 'raid',
    type: 'raid',
    position: { x: COL_X.raid, y: memberMid - RAID_H / 2 },
    data: { volume, ringHexes, detail: { kind: 'raid' } },
  });
  edges.push({
    id: hasAccess ? 'e-access-raid' : 'e-app-raid',
    source: hasAccess ? 'access' : 'app',
    target: 'raid',
    style: { stroke: STRUCTURAL_STROKE, strokeWidth: 2 },
    data: { detailRef: 'raid' },
  });

  // --- Members + backing disks -------------------------------------------
  const memberNodeId = (join: MemberJoin, index: number) =>
    join.slot !== null ? `member-${join.slot}` : `replica-${join.replica?.node ?? index}`;
  const diskYByNodeId = new Map<string, number>();

  joins.forEach((join, i) => {
    const id = memberNodeId(join, i);
    const visual = memberVisual(join);
    nodes.push({
      id,
      type: 'member',
      position: { x: COL_X.member, y: i * ROW_H },
      data: {
        join,
        rebuildPct: join.isRebuildTarget ? rebuild?.progress_percentage ?? null : null,
        detail: { kind: 'member', join },
      },
    });

    const pathLabel = join.replica
      ? join.replica.is_local
        ? 'local'
        : `nvme-of${
            join.replica.nvmf_target?.transport_type
              ? `/${join.replica.nvmf_target.transport_type.toLowerCase()}`
              : ''
          }`
      : undefined;

    edges.push({
      id: `e-raid-${id}`,
      source: 'raid',
      target: id,
      animated: visual.recovering,
      label: pathLabel,
      labelStyle: EDGE_LABEL_STYLE,
      labelBgStyle: EDGE_LABEL_BG,
      style: {
        stroke: visual.hex,
        strokeWidth: 2.5,
        ...(visual.dashed ? { strokeDasharray: '7 4' } : {}),
      },
      data: { detailRef: id },
    });

    if (join.disk) {
      const diskId = `disk-${join.disk.id}`;
      if (!nodes.some(n => n.id === diskId)) {
        const diskY = i * ROW_H + (MEMBER_H - DISK_H) / 2;
        diskYByNodeId.set(diskId, diskY);
        nodes.push({
          id: diskId,
          type: 'disk',
          position: { x: COL_X.disk, y: diskY },
          data: { disk: join.disk, detail: { kind: 'disk', disk: join.disk, join } },
        });
      }
      edges.push({
        id: `e-${id}-${diskId}`,
        source: id,
        target: diskId,
        style: { stroke: STRUCTURAL_STROKE, strokeWidth: 1.5 },
        data: { detailRef: diskId },
      });
    }
  });

  // --- Rebuild data flow: source member → target member -------------------
  if (rebuild) {
    const sourceIdx = joins.findIndex(j => j.slot === rebuild.source_slot);
    const targetIdx = joins.findIndex(j => j.slot === rebuild.target_slot);
    const sourceJoin = joins[sourceIdx];
    const targetJoin = joins[targetIdx];
    if (sourceJoin && targetJoin && sourceIdx !== targetIdx) {
      const sourceId = memberNodeId(sourceJoin, sourceIdx);
      const targetId = memberNodeId(targetJoin, targetIdx);
      const downward = targetIdx > sourceIdx;
      edges.push({
        id: 'e-rebuild',
        source: sourceId,
        target: targetId,
        sourceHandle: downward ? 'rb-out-bottom' : 'rb-out-top',
        targetHandle: downward ? 'rb-in-top' : 'rb-in-bottom',
        animated: true,
        label: `rebuild ${rebuild.progress_percentage.toFixed(1)}%`,
        labelStyle: EDGE_LABEL_STYLE,
        labelBgStyle: EDGE_LABEL_BG,
        style: {
          stroke: memberStateStyle('rebuilding').hex,
          strokeWidth: 2.5,
          strokeDasharray: '4 3',
        },
        data: { detailRef: targetId },
      });
    }
  }

  return { nodes, edges };
}
