import React from 'react';
import { Handle, Position, type NodeProps } from 'reactflow';
import { Database, HardDrive, Monitor, Network, Zap } from 'lucide-react';
import { MemberStateChip } from '../../ui/Chip';
import { SyncStateIndicator } from '../../ui/SyncStateIndicator';
import { ProgressBar } from '../../ui/ProgressBar';
import { memberVisual } from './buildTopology';
import type {
  AccessNodeData,
  AppNodeData,
  DiskNodeData,
  MemberNodeData,
  RaidNodeData,
} from './buildTopology';

// Custom React Flow nodes for the topology view. Status renders as
// icon+label chips from the shared kit (never color alone); text stays in
// ink tokens — the colored ring/edges carry state, the words carry identity.

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

// Grafana-node-graph-style ring: one arc segment per RAID member, colored by
// its state, with a 2px surface gap between segments (mark-spec spacer).
export function StatusRing({ hexes, size = 44 }: { hexes: string[]; size?: number }) {
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
      <Database className="absolute inset-0 m-auto h-4 w-4 text-blue-600" aria-hidden="true" />
    </div>
  );
}

export function AppTopologyNode({ selected }: NodeProps<AppNodeData>) {
  return (
    <NodeCard selected={selected} width="w-44">
      <div className="flex items-center gap-2">
        <div className="flex h-8 w-8 flex-shrink-0 items-center justify-center rounded-full border border-purple-300 bg-purple-100">
          <Monitor className="h-4 w-4 text-purple-600" aria-hidden="true" />
        </div>
        <div className="min-w-0">
          <p className="text-sm font-medium text-gray-800">Consumer</p>
          <p className="truncate text-xs text-gray-500">Pod / application</p>
        </div>
      </div>
      <Handle type="source" position={Position.Right} className={HIDDEN_HANDLE} />
    </NodeCard>
  );
}

export function AccessTopologyNode({ data, selected }: NodeProps<AccessNodeData>) {
  const ublk = data.volume.ublk_device;
  return (
    <NodeCard selected={selected} width="w-48">
      <div className="flex items-center gap-2">
        <div className="flex h-8 w-8 flex-shrink-0 items-center justify-center rounded-full border border-indigo-300 bg-indigo-100">
          {ublk ? (
            <HardDrive className="h-4 w-4 text-indigo-600" aria-hidden="true" />
          ) : (
            <Network className="h-4 w-4 text-indigo-600" aria-hidden="true" />
          )}
        </div>
        <div className="min-w-0">
          <p className="text-sm font-medium text-gray-800">
            {ublk ? 'ublk block device' : 'NVMe-oF access'}
          </p>
          <p className="truncate font-mono text-xs text-gray-500">
            {ublk
              ? ublk.device_path
              : `${data.volume.nvmeof_targets.length} target${
                  data.volume.nvmeof_targets.length === 1 ? '' : 's'
                }`}
          </p>
        </div>
      </div>
      <Handle type="target" position={Position.Left} className={HIDDEN_HANDLE} />
      <Handle type="source" position={Position.Right} className={HIDDEN_HANDLE} />
    </NodeCard>
  );
}

export function RaidTopologyNode({ data, selected }: NodeProps<RaidNodeData>) {
  const { volume, ringHexes } = data;
  const raid = volume.raid_status;
  return (
    <NodeCard selected={selected} width="w-64">
      <div className="flex items-start gap-3">
        <StatusRing hexes={ringHexes} />
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-semibold text-gray-800" title={volume.name}>
            {volume.name}
          </p>
          <p className="text-xs text-gray-500">
            {raid ? 'SPDK RAID bdev' : 'SPDK bdev'} • {volume.size}
          </p>
          <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
            {raid && <MemberStateChip state={raid.state} />}
            {raid && <span className="text-xs font-medium text-gray-600">RAID-{raid.raid_level}</span>}
          </div>
          {raid && (
            <p className="mt-1 text-xs tabular-nums text-gray-600">
              {raid.operational_members}/{raid.num_members} members operational
            </p>
          )}
        </div>
      </div>
      <Handle type="target" position={Position.Left} className={HIDDEN_HANDLE} />
      <Handle type="source" position={Position.Right} className={HIDDEN_HANDLE} />
    </NodeCard>
  );
}

export function MemberTopologyNode({ data, selected }: NodeProps<MemberNodeData>) {
  const { join, rebuildPct } = data;
  const replica = join.replica;
  const { state } = memberVisual(join);
  return (
    <NodeCard selected={selected} width="w-56">
      <div className="flex items-center gap-2">
        <div className="flex h-7 w-7 flex-shrink-0 items-center justify-center rounded-full border border-gray-300 bg-gray-100">
          <span className="text-xs font-bold text-gray-700">{join.slot ?? '·'}</span>
        </div>
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium text-gray-800">
            {replica?.node ?? join.member?.name ?? 'unknown'}
          </p>
          <p className="truncate text-xs text-gray-500" title={join.member?.name}>
            {join.slot !== null
              ? `slot ${join.slot}`
              : join.raidPresent
                ? 'not assembled'
                : 'replica'}
            {replica ? (replica.is_local ? ' • local' : ' • remote') : ''}
          </p>
        </div>
        {replica &&
          (replica.is_local ? (
            <Zap className="h-3.5 w-3.5 flex-shrink-0 text-blue-600" aria-label="local access" />
          ) : (
            <Network
              className="h-3.5 w-3.5 flex-shrink-0 text-purple-600"
              aria-label="network access"
            />
          ))}
      </div>
      <div className="mt-1.5 flex flex-wrap items-center gap-1.5">
        <MemberStateChip state={state} />
        {replica?.sync && <SyncStateIndicator sync={replica.sync} compact />}
        {replica?.is_new_replica && (
          <span className="rounded-full bg-brand-500 px-1.5 py-0.5 text-[10px] font-medium text-white">
            NEW
          </span>
        )}
      </div>
      {rebuildPct !== null && (
        <div className="mt-1.5">
          <ProgressBar
            value={rebuildPct}
            label={`slot ${join.slot ?? '?'} rebuild progress`}
            valueText={`${rebuildPct.toFixed(1)}%`}
            tone="warn"
            className="w-full"
          />
        </div>
      )}
      <Handle type="target" position={Position.Left} className={HIDDEN_HANDLE} />
      <Handle type="source" position={Position.Right} className={HIDDEN_HANDLE} />
      {/* Rebuild data-flow anchors (top/bottom) so a member→member edge runs
          down the column instead of looping around the layer. */}
      <Handle type="source" position={Position.Top} id="rb-out-top" className={HIDDEN_HANDLE} />
      <Handle type="target" position={Position.Top} id="rb-in-top" className={HIDDEN_HANDLE} />
      <Handle type="source" position={Position.Bottom} id="rb-out-bottom" className={HIDDEN_HANDLE} />
      <Handle type="target" position={Position.Bottom} id="rb-in-bottom" className={HIDDEN_HANDLE} />
    </NodeCard>
  );
}

export function DiskTopologyNode({ data, selected }: NodeProps<DiskNodeData>) {
  const d = data.disk;
  return (
    <NodeCard selected={selected} width="w-52">
      <div className="flex items-center gap-2">
        <HardDrive
          className={`h-4 w-4 flex-shrink-0 ${d.healthy ? 'text-gray-600' : 'text-red-600'}`}
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
