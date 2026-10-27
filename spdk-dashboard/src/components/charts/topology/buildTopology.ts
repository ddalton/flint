import type { Edge, Node } from 'reactflow';
import type {
  ConsumerRaid,
  ConsumerRaidMember,
  Disk,
  ReplicaStatus,
  Volume,
} from '../../../hooks/useDashboardData';
import { isReplicaRecovering, primaryConsumerRaid } from '../../../hooks/useDashboardData';
import { memberStateStyle } from '../../ui/status';

// Pure volume → graph projection for the topology view. Everything visual
// (node positions, edge colors/dashes/animation) is decided here so it can
// be unit-tested without rendering React Flow; the components only draw
// what this returns.
//
// Layout is a fixed left→right layered DAG — the data path — NOT
// force-directed: consumer node → the raid it assembled → base legs →
// backing disks. Deterministic positions mean nothing jumps between
// refreshes.
//
// Sources, exactly as the backend sends them: `consumer_raids` (one row per
// node that has `raid_<pv>` assembled, each base with its configured flag
// and the replica node it backs) and `replica_statuses` (the Tier-2 sync
// record). There is no raid-level rebuild_info: flint never runs the stock
// blind rebuild, so "recovery" is a replica leaving in_sync and coming back
// (stale → standby → hot rejoin), carried by the sync record.

// One raid base joined with its replica status and backing disk.
export interface MemberJoin {
  // Position in the consumer raid's base list; null for a replica the raid
  // has not assembled (a stale/standby leg, or no consumer at all).
  slot: number | null;
  member: ConsumerRaidMember | null;
  replica: ReplicaStatus | null;
  disk: Disk | null;
  // Node the consumer raid lives on; null when the volume has no consumer.
  consumerNode: string | null;
  // True when a consumer raid exists — only then is a slot-less replica
  // "not assembled" rather than simply a replica.
  raidPresent: boolean;
}

export type TopologyDetail =
  | { kind: 'consumer' }
  | { kind: 'raid' }
  | { kind: 'member'; join: MemberJoin }
  | { kind: 'disk'; disk: Disk; join: MemberJoin }
  | { kind: 'about' };

export interface ConsumerNodeData {
  volume: Volume;
  raid: ConsumerRaid | null;
  detail: TopologyDetail;
}
export interface RaidNodeData {
  volume: Volume;
  raid: ConsumerRaid | null;
  // One ring segment per member, colored by its state.
  ringHexes: string[];
  detail: TopologyDetail;
}
export interface MemberNodeData {
  join: MemberJoin;
  detail: TopologyDetail;
}
export interface DiskNodeData {
  disk: Disk;
  detail: TopologyDetail;
}
export type TopologyNodeData = ConsumerNodeData | RaidNodeData | MemberNodeData | DiskNodeData;

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

// Visual state for one member. A base the raid holds is what SPDK says it
// is: configured → online; a nulled slot → the leg's own status when the
// backend named the leg it belonged to (stale while it catches up), failed
// when nothing is known about it. A replica outside the
// raid shows its sync-record status (stale / standby). Dashed = reached
// over NVMe-oF, i.e. on a node other than the consumer's. Recovery
// (animated) = the replica is not in_sync.
export function memberVisual(join: MemberJoin): MemberVisual {
  const state = join.member
    ? join.member.is_configured
      ? 'online'
      : (join.replica?.status ?? 'failed').toLowerCase()
    : (join.replica?.status ?? 'unknown').toLowerCase();
  return {
    state,
    hex: memberStateStyle(state).hex,
    dashed: !!join.replica && !!join.consumerNode && join.replica.node !== join.consumerNode,
    recovering: join.replica ? isReplicaRecovering(join.replica) : false,
  };
}

// Health of an assembled consumer raid, from SPDK's own state and the
// configured-base count ("online" with operational < total is degraded).
export function raidHealth(raid: ConsumerRaid): 'healthy' | 'degraded' | 'failed' {
  if (raid.state === 'online') {
    return raid.num_base_bdevs_operational < raid.num_base_bdevs ? 'degraded' : 'healthy';
  }
  return raid.state === 'configuring' ? 'degraded' : 'failed';
}

// Consumer-raid bases (in SPDK slot order) joined with replicas by the
// replica_node the backend resolved; replicas the raid does not hold are
// appended slot-less. Disks resolve through what the API actually sends —
// the replica's node plus the disk's provisioned_volumes.
export function joinMembers(volume: Volume, disks: Disk[]): MemberJoin[] {
  const raid = primaryConsumerRaid(volume);
  const consumerNode = raid?.node ?? null;
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
    raid.base_bdevs.forEach((member, slot) => {
      const replica = member.replica_node
        ? volume.replica_statuses.find(
            r => r.node === member.replica_node && !claimed.has(r)
          ) ?? null
        : null;
      if (replica) claimed.add(replica);
      joins.push({
        slot,
        member,
        replica,
        disk: backingDisk(replica),
        consumerNode,
        raidPresent: true,
      });
    });
  }

  for (const replica of volume.replica_statuses) {
    if (claimed.has(replica)) continue;
    joins.push({
      slot: null,
      member: null,
      replica,
      disk: backingDisk(replica),
      consumerNode,
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
const COL_X = { consumer: 0, raid: 300, member: 725, disk: 1070 } as const;
const ROW_H = 150;
const CONSUMER_H = 64;
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
  const raid = primaryConsumerRaid(volume);

  const memberMid =
    joins.length > 0 ? ((joins.length - 1) * ROW_H + MEMBER_H) / 2 : RAID_H / 2;

  const nodes: TopologyNode[] = [];
  const edges: TopologyEdge[] = [];

  // --- Consumer ------------------------------------------------------------
  nodes.push({
    id: 'consumer',
    type: 'consumer',
    position: { x: COL_X.consumer, y: memberMid - CONSUMER_H / 2 },
    data: { volume, raid, detail: { kind: 'consumer' } },
  });

  // --- RAID bdev ---------------------------------------------------------
  const ringHexes =
    joins.length > 0
      ? joins.map(j => memberVisual(j).hex)
      : [memberStateStyle('unknown').hex];
  nodes.push({
    id: 'raid',
    type: 'raid',
    position: { x: COL_X.raid, y: memberMid - RAID_H / 2 },
    data: { volume, raid, ringHexes, detail: { kind: 'raid' } },
  });
  edges.push({
    id: 'e-consumer-raid',
    source: 'consumer',
    target: 'raid',
    style: { stroke: STRUCTURAL_STROKE, strokeWidth: 2 },
    data: { detailRef: 'raid' },
  });

  // --- Members + backing disks -------------------------------------------
  const memberNodeId = (join: MemberJoin, index: number) =>
    join.slot !== null ? `member-${join.slot}` : `replica-${join.replica?.node ?? index}`;

  joins.forEach((join, i) => {
    const id = memberNodeId(join, i);
    const visual = memberVisual(join);
    nodes.push({
      id,
      type: 'member',
      position: { x: COL_X.member, y: i * ROW_H },
      data: { join, detail: { kind: 'member', join } },
    });

    // Path label only when the consumer is known: local leg, or an NVMe-oF
    // remote. The wire carries no transport detail beyond that.
    const pathLabel =
      join.replica && join.consumerNode ? (visual.dashed ? 'nvme-of' : 'local') : undefined;

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
        nodes.push({
          id: diskId,
          type: 'disk',
          position: { x: COL_X.disk, y: i * ROW_H + (MEMBER_H - DISK_H) / 2 },
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

  return { nodes, edges };
}
