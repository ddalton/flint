import React from 'react';
import { Handle, Position, type NodeProps } from 'reactflow';
import { Database, HardDrive, Monitor, Network, Zap, type LucideIcon } from 'lucide-react';
import { MemberStateChip } from '../../ui/Chip';
import { SyncStateIndicator } from '../../ui/SyncStateIndicator';
import { memberVisual } from './buildTopology';
import type {
  ConsumerNodeData,
  DiskNodeData,
  MemberNodeData,
  RaidNodeData,
} from './buildTopology';

// Custom React Flow nodes for the topology view. Status renders as
// icon+label chips from the shared kit (never color alone); text stays in
// ink tokens — the colored ring/edges carry state, the words carry identity.
// Every figure shown is a backend datum: the consumer raid's node and
// configured-base count, each base's configured flag, the replica's sync
// state. Nothing here is estimated or animated without a value behind it.

// The graph is read-only: handles exist so edges have anchors, but they are
// not visible or interactive.
const HIDDEN_HANDLE = '!opacity-0 !pointer-events-none !w-1 !h-1 !min-w-0 !min-h-0 !border-0';

function NodeCard({
  selected,
  width,
  children,
}: {
  selected?: boolean;
  width: string;
  children: React.ReactNode;
}) {
  return (
    <div
      className={`${width} rounded-lg border-2 bg-white px-3 py-2.5 text-left shadow-sm transition-shadow ${
        selected
          ? 'border-brand-500 shadow-md ring-2 ring-brand-200'
          : 'border-gray-200 hover:border-gray-300'
      }`}
    >
      {children}
    </div>
  );
}

// Grafana-node-graph-style ring: one arc segment per constituent (RAID
// member, disk, …), colored by its state, with a 2px surface gap between
// segments (mark-spec spacer).
export function StatusRing({
  hexes,
  size = 44,
  icon: Icon = Database,
}: {
  hexes: string[];
  size?: number;
  icon?: LucideIcon;
}) {
  const stroke = 5;
  const r = (size - stroke) / 2;
  const c = 2 * Math.PI * r;
  const seg = c / hexes.length;
  const gap = hexes.length > 1 ? 2 : 0;
  return (
    <div className="relative flex-shrink-0" style={{ width: size, height: size }}>
      <svg width={size} height={size} className="-rotate-90" aria-hidden="true">
        <circle cx={size / 2} cy={size / 2} r={r} fill="none" stroke="#e5e7eb" strokeWidth={stroke} />
        {hexes.map((hex, i) => (
          <circle
            key={i}
            cx={size / 2}
            cy={size / 2}
            r={r}
            fill="none"
            stroke={hex}
            strokeWidth={stroke}
            strokeLinecap="butt"
            strokeDasharray={`${Math.max(seg - gap, 1)} ${c - Math.max(seg - gap, 1)}`}
            strokeDashoffset={-(i * seg)}
          />
        ))}
      </svg>
      <Icon className="absolute inset-0 m-auto h-4 w-4 text-brand-600" aria-hidden="true" />
    </div>
  );
}

export function ConsumerTopologyNode({ data, selected }: NodeProps<ConsumerNodeData>) {
  const raid = data.raid;
  return (
    <NodeCard selected={selected} width="w-48">
      <div className="flex items-center gap-2">
        {/* Raw purple on purpose: consumer identity accent, not the
            rejoining sync state the purple alias carries. */}
        <div className="flex h-8 w-8 flex-shrink-0 items-center justify-center rounded-full border border-purple-300 bg-purple-100">
          <Monitor className="h-4 w-4 text-purple-600" aria-hidden="true" />
        </div>
        <div className="min-w-0">
          <p className="text-sm font-medium text-gray-800">Consumer</p>
          <p className="truncate text-xs text-gray-500" title={raid?.node}>
            {raid ? raid.node : 'not staged on any node'}
          </p>
        </div>
      </div>
      <Handle type="source" position={Position.Right} className={HIDDEN_HANDLE} />
    </NodeCard>
  );
}

export function RaidTopologyNode({ data, selected }: NodeProps<RaidNodeData>) {
  const { volume, ringHexes, raid } = data;
  return (
    <NodeCard selected={selected} width="w-64">
      <div className="flex items-start gap-3">
        <StatusRing hexes={ringHexes} />
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-semibold text-gray-800" title={volume.name}>
            {volume.name}
          </p>
          <p className="truncate text-xs text-gray-500" title={raid?.raid_name}>
            {raid ? `SPDK raid bdev on ${raid.node}` : 'no raid assembled'} • {volume.size}
          </p>
          {raid && (
            <>
              <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
                <MemberStateChip state={raid.state} />
              </div>
              <p className="mt-1 text-xs tabular-nums text-gray-600">
                {raid.num_base_bdevs_operational}/{raid.num_base_bdevs} bases configured
              </p>
            </>
          )}
        </div>
      </div>
      <Handle type="target" position={Position.Left} className={HIDDEN_HANDLE} />
      <Handle type="source" position={Position.Right} className={HIDDEN_HANDLE} />
    </NodeCard>
  );
}

export function MemberTopologyNode({ data, selected }: NodeProps<MemberNodeData>) {
  const { join } = data;
  const replica = join.replica;
  const { state, dashed } = memberVisual(join);
  // Path is only knowable relative to a consumer.
  const path = replica && join.consumerNode ? (dashed ? 'remote' : 'local') : null;
  return (
    <NodeCard selected={selected} width="w-56">
      <div className="flex items-center gap-2">
        <div className="flex h-7 w-7 flex-shrink-0 items-center justify-center rounded-full border border-gray-300 bg-gray-100">
          <span className="text-xs font-bold text-gray-700">{join.slot ?? '·'}</span>
        </div>
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium text-gray-800">
            {replica?.node ?? join.member?.name ?? 'failed slot'}
          </p>
          <p className="truncate text-xs text-gray-500" title={join.member?.name ?? undefined}>
            {join.slot !== null
              ? `slot ${join.slot}${join.member && !join.member.is_configured ? ' • removed' : ''}`
              : join.raidPresent
                ? 'not assembled'
                : 'replica'}
            {path ? ` • ${path}` : ''}
          </p>
        </div>
        {path === 'local' && (
          <Zap className="h-3.5 w-3.5 flex-shrink-0 text-brand-600" aria-label="local access" />
        )}
        {path === 'remote' && (
          // Raw purple on purpose: network-path accent, not the rejoining
          // sync state the purple alias carries.
          <Network
            className="h-3.5 w-3.5 flex-shrink-0 text-purple-600"
            aria-label="network access"
          />
        )}
      </div>
      <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
        <MemberStateChip state={state} />
        {replica?.sync && <SyncStateIndicator sync={replica.sync} compact />}
      </div>
      <Handle type="target" position={Position.Left} className={HIDDEN_HANDLE} />
      <Handle type="source" position={Position.Right} className={HIDDEN_HANDLE} />
    </NodeCard>
  );
}

export function DiskTopologyNode({ data, selected }: NodeProps<DiskNodeData>) {
  const d = data.disk;
  return (
    <NodeCard selected={selected} width="w-52">
      <div className="flex items-center gap-2">
        <HardDrive
          className={`h-4 w-4 flex-shrink-0 ${d.healthy ? 'text-gray-600' : 'text-failed-600'}`}
          aria-hidden="true"
        />
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium text-gray-800" title={d.id}>
            {d.model || d.id}
          </p>
          <p className="truncate text-xs text-gray-500">
            {d.node} • {d.free_space_display} free
          </p>
        </div>
      </div>
      <div className="mt-1 grid grid-cols-2 gap-x-2 text-[11px] tabular-nums text-gray-600">
        <span>R {d.read_iops.toLocaleString()} IOPS</span>
        <span>W {d.write_iops.toLocaleString()} IOPS</span>
      </div>
      <Handle type="target" position={Position.Left} className={HIDDEN_HANDLE} />
    </NodeCard>
  );
}
