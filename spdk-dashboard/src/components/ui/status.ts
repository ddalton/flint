import {
  AlertTriangle,
  CheckCircle,
  Clock,
  HardDrive,
  Settings,
  Shield,
  X,
  XCircle,
  type LucideIcon,
} from 'lucide-react';
import type { SyncState } from '../../hooks/useDashboardData';

// Semantic style tokens — ONE source for every status label/color/icon so
// the same state renders identically in every view (the pre-Phase-3
// baseline had three hand-rolled copies that had already drifted apart).
// Sections: replica sync states (Tier-2), volume states, raid-member /
// legacy replica states, and volume-filter display.
export interface SyncStateStyle {
  label: string;
  chip: string;
  dot: string;
  description: string;
}

export type SyncDisplayState = SyncState | 'rejoining';

export const SYNC_STATE_STYLES: Record<SyncDisplayState, SyncStateStyle> = {
  in_sync: {
    label: 'in sync',
    chip: 'bg-insync-100 text-insync-800 border-insync-200',
    dot: 'bg-insync-500',
    description: 'Holds every acknowledged write; eligible raid member',
  },
  stale: {
    label: 'stale',
    chip: 'bg-stale-100 text-stale-800 border-stale-200',
    dot: 'bg-stale-500',
    description: 'Missed acknowledged writes; excluded from assembly until caught up',
  },
  standby: {
    label: 'standby',
    chip: 'bg-standby-100 text-standby-800 border-standby-200',
    dot: 'bg-standby-500',
    description: 'Caught up and chasing epochs; rejoins at the next assembly',
  },
  rejoining: {
    label: 'rejoining',
    chip: 'bg-rejoining-100 text-rejoining-800 border-rejoining-200',
    dot: 'bg-rejoining-500',
    description: 'Hot rejoin in flight',
  },
};

// --- Volume states (backend DashboardVolume.state) ---------------------

export interface VolumeStateStyle {
  badge: string; // unbordered pill (tables)
  chip: string; // bordered pill
  hex: string; // chart series color
  icon: LucideIcon;
  // Sort weight: views listing mixed states put the most broken first.
  priority: number;
  tooltip?: string;
}

export const VOLUME_STATE_STYLES: Record<'Healthy' | 'Degraded' | 'Failed', VolumeStateStyle> = {
  Healthy: {
    badge: 'bg-healthy-100 text-healthy-800',
    chip: 'bg-healthy-100 text-healthy-800 border-healthy-200',
    hex: '#10b981',
    icon: CheckCircle,
    priority: 0,
  },
  Degraded: {
    badge: 'bg-degraded-100 text-degraded-800',
    chip: 'bg-degraded-100 text-degraded-800 border-degraded-200',
    hex: '#f59e0b',
    icon: AlertTriangle,
    priority: 2,
    tooltip: 'Volume has reduced redundancy but is still functional',
  },
  Failed: {
    badge: 'bg-failed-100 text-failed-800',
    chip: 'bg-failed-100 text-failed-800 border-failed-200',
    hex: '#ef4444',
    icon: XCircle,
    priority: 3,
    tooltip: 'Volume has completely failed and requires immediate attention',
  },
};

export const UNKNOWN_VOLUME_STATE: VolumeStateStyle = {
  badge: 'bg-gray-100 text-gray-800',
  chip: 'bg-gray-100 text-gray-800 border-gray-200',
  hex: '#6b7280',
  icon: X,
  priority: 4,
};

export function volumeStateStyle(state: string): VolumeStateStyle {
  return VOLUME_STATE_STYLES[state as keyof typeof VOLUME_STATE_STYLES] ?? UNKNOWN_VOLUME_STATE;
}

// --- Node fleet health (backend NodeSummary.health) ---------------------

export type NodeHealth = 'critical' | 'warning' | 'ok';

export interface NodeHealthStyle {
  label: string;
  chip: string;
  // Heatmap status-cell fill.
  cell: string;
  // Sort weight: fleet views put the most broken nodes first.
  priority: number;
  description: string;
}

export const NODE_HEALTH_STYLES: Record<NodeHealth, NodeHealthStyle> = {
  critical: {
    label: 'Critical',
    chip: 'bg-failed-100 text-failed-800 border-failed-200',
    cell: 'bg-failed-500',
    priority: 0,
    description: 'Unhealthy disk or failed volume/replica on this node',
  },
  warning: {
    label: 'Warning',
    chip: 'bg-warning-100 text-warning-800 border-warning-200',
    cell: 'bg-warning-400',
    priority: 1,
    description: 'Degraded volume or out-of-sync replica on this node',
  },
  ok: {
    label: 'Ready',
    chip: 'bg-healthy-100 text-healthy-800 border-healthy-200',
    cell: 'bg-healthy-500',
    priority: 3,
    description: 'All disks healthy, all replicas in sync',
  },
};

const UNKNOWN_NODE_HEALTH: NodeHealthStyle = {
  label: 'Unknown',
  chip: 'bg-gray-100 text-gray-800 border-gray-200',
  cell: 'bg-gray-400',
  priority: 2,
  description: 'Health state not recognized by this frontend',
};

export function nodeHealthStyle(health: string): NodeHealthStyle {
  return NODE_HEALTH_STYLES[health as NodeHealth] ?? UNKNOWN_NODE_HEALTH;
}

// --- Raid-member / legacy replica states --------------------------------
// Lowercase keys: SPDK member states (online/degraded/failed/rebuilding/
// spare/removing), the legacy replica statuses, and the Tier-2 sync states
// (whose chips reuse SYNC_STATE_STYLES so both renderings can never drift).

export interface MemberStateStyle {
  chip: string;
  icon: LucideIcon;
  iconClass: string;
  // Stroke token for graph edges/rings (topology view). The -600 steps, not
  // the -500 chart fills: thin strokes need ≥3:1 on the white surface.
  hex: string;
}

const MEMBER_STATE_STYLES: Record<string, MemberStateStyle> = {
  online: {
    chip: 'bg-healthy-100 text-healthy-800 border-healthy-200',
    icon: CheckCircle,
    iconClass: 'text-healthy-600',
    hex: '#059669',
  },
  healthy: {
    chip: 'bg-healthy-100 text-healthy-800 border-healthy-200',
    icon: CheckCircle,
    iconClass: 'text-healthy-600',
    hex: '#059669',
  },
  degraded: {
    chip: 'bg-degraded-100 text-degraded-800 border-degraded-200',
    icon: AlertTriangle,
    iconClass: 'text-degraded-600',
    hex: '#d97706',
  },
  failed: {
    chip: 'bg-failed-100 text-failed-800 border-failed-200',
    icon: X,
    iconClass: 'text-failed-600',
    hex: '#dc2626',
  },
  rebuilding: {
    chip: 'bg-rebuilding-100 text-rebuilding-800 border-rebuilding-200',
    icon: Settings,
    // Motion with meaning: the gear itself is static — real progress renders
    // through the data-bound sync indicator / ProgressBar.
    iconClass: 'text-rebuilding-600',
    hex: '#ea580c',
  },
  spare: {
    chip: 'bg-standby-100 text-standby-800 border-standby-200',
    icon: Shield,
    iconClass: 'text-standby-600',
    hex: '#2563eb',
  },
  removing: {
    // Raw purple on purpose: no semantic alias fits (rejoining also aliases
    // purple but means the opposite of leaving). Rare legacy SPDK state.
    chip: 'bg-purple-100 text-purple-800 border-purple-200',
    icon: Clock,
    iconClass: 'text-purple-600',
    hex: '#9333ea',
  },
  stale: {
    chip: SYNC_STATE_STYLES.stale.chip,
    icon: AlertTriangle,
    iconClass: 'text-stale-600',
    hex: '#d97706',
  },
  standby: {
    chip: SYNC_STATE_STYLES.standby.chip,
    icon: Clock,
    iconClass: 'text-standby-600',
    hex: '#2563eb',
  },
};

export const UNKNOWN_MEMBER_STATE: MemberStateStyle = {
  chip: 'bg-gray-100 text-gray-800 border-gray-200',
  icon: HardDrive,
  iconClass: 'text-gray-600',
  hex: '#6b7280',
};

export function memberStateStyle(state: string): MemberStateStyle {
  return MEMBER_STATE_STYLES[state.toLowerCase()] ?? UNKNOWN_MEMBER_STATE;
}

// --- Volume-filter display ----------------------------------------------
// One vocabulary for the filter cards, banners, and per-view captions
// (Dashboard, VolumesTable, and NodeDetailView each had their own copy with
// diverging names).

export interface VolumeFilterDisplay {
  name: string;
  // Inline lowercase form ("3 degraded volumes on this node"); empty for
  // 'all' so unfiltered captions read naturally.
  short: string;
  severity: string;
  icon: string;
  description: string;
  bgColor: string;
  borderColor: string;
}

export const VOLUME_FILTER_DISPLAY: Record<string, VolumeFilterDisplay> = {
  all: {
    name: 'All Volumes',
    short: '',
    severity: 'info',
    icon: '📊',
    description: 'All volumes in the system',
    bgColor: 'bg-brand-50',
    borderColor: 'border-brand-200',
  },
  healthy: {
    name: 'Healthy Volumes',
    short: 'healthy',
    severity: 'good',
    icon: '✅',
    description: 'All replicas operational',
    bgColor: 'bg-healthy-50',
    borderColor: 'border-healthy-200',
  },
  degraded: {
    name: 'Degraded Volumes',
    short: 'degraded',
    severity: 'warning',
    icon: '🟡',
    description: 'Volumes with reduced redundancy',
    bgColor: 'bg-degraded-50',
    borderColor: 'border-degraded-200',
  },
  failed: {
    name: 'Failed Volumes',
    short: 'failed',
    severity: 'critical',
    icon: '🔴',
    description: 'Volumes that have completely failed',
    bgColor: 'bg-failed-50',
    borderColor: 'border-failed-200',
  },
  faulted: {
    name: 'Faulted Volumes (Degraded + Failed)',
    short: 'faulted',
    severity: 'mixed',
    icon: '⚠️',
    description: 'Both degraded and failed volumes',
    // Raw orange on purpose: "faulted" is a mixed bucket, not the rebuilding
    // meaning the orange alias carries.
    bgColor: 'bg-orange-50',
    borderColor: 'border-orange-200',
  },
  rebuilding: {
    name: 'Volumes with Recovering Replicas',
    short: 'recovering',
    severity: 'recovery',
    icon: '🔄',
    description: 'Volumes with replica recovery activity (catch-up, standby, or legacy rebuild)',
    bgColor: 'bg-rebuilding-50',
    borderColor: 'border-rebuilding-200',
  },
  'local-nvme': {
    name: 'Local NVMe Volumes',
    short: 'local NVMe',
    severity: 'performance',
    icon: '⚡',
    description: 'High-performance local storage',
    bgColor: 'bg-brand-50',
    borderColor: 'border-brand-200',
  },
  orphaned: {
    name: 'Orphaned Volumes (Raw SPDK)',
    short: 'orphaned',
    severity: 'cleanup',
    icon: '🛡️',
    description: 'Raw SPDK volumes without Kubernetes backing - cleanup candidates',
    bgColor: 'bg-warning-50',
    borderColor: 'border-warning-200',
  },
};

const ALL_VOLUMES_DISPLAY = VOLUME_FILTER_DISPLAY.all as VolumeFilterDisplay;

export function volumeFilterDisplay(filter: string | null | undefined): VolumeFilterDisplay {
  return VOLUME_FILTER_DISPLAY[filter ?? 'all'] ?? ALL_VOLUMES_DISPLAY;
}
