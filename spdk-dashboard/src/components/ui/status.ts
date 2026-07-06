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
    chip: 'bg-green-100 text-green-800 border-green-200',
    dot: 'bg-green-500',
    description: 'Holds every acknowledged write; eligible raid member',
  },
  stale: {
    label: 'stale',
    chip: 'bg-amber-100 text-amber-800 border-amber-200',
    dot: 'bg-amber-500',
    description: 'Missed acknowledged writes; excluded from assembly until caught up',
  },
  standby: {
    label: 'standby',
    chip: 'bg-blue-100 text-blue-800 border-blue-200',
    dot: 'bg-blue-500',
    description: 'Caught up and chasing epochs; rejoins at the next assembly',
  },
  rejoining: {
    label: 'rejoining',
    chip: 'bg-purple-100 text-purple-800 border-purple-200',
    dot: 'bg-purple-500',
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
    badge: 'bg-green-100 text-green-800',
    chip: 'bg-green-100 text-green-800 border-green-200',
    hex: '#10b981',
    icon: CheckCircle,
    priority: 0,
  },
  Degraded: {
    badge: 'bg-yellow-100 text-yellow-800',
    chip: 'bg-yellow-100 text-yellow-800 border-yellow-200',
    hex: '#f59e0b',
    icon: AlertTriangle,
    priority: 2,
    tooltip: 'Volume has reduced redundancy but is still functional',
  },
  Failed: {
    badge: 'bg-red-100 text-red-800',
    chip: 'bg-red-100 text-red-800 border-red-200',
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
    chip: 'bg-red-100 text-red-800 border-red-200',
    cell: 'bg-red-500',
    priority: 0,
    description: 'Unhealthy disk or failed volume/replica on this node',
  },
  warning: {
    label: 'Warning',
    chip: 'bg-amber-100 text-amber-800 border-amber-200',
    cell: 'bg-amber-400',
    priority: 1,
    description: 'Degraded volume or out-of-sync replica on this node',
  },
  ok: {
    label: 'Ready',
    chip: 'bg-green-100 text-green-800 border-green-200',
    cell: 'bg-green-500',
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
    chip: 'bg-green-100 text-green-800 border-green-200',
    icon: CheckCircle,
    iconClass: 'text-green-600',
    hex: '#059669',
  },
  healthy: {
    chip: 'bg-green-100 text-green-800 border-green-200',
    icon: CheckCircle,
    iconClass: 'text-green-600',
    hex: '#059669',
  },
  degraded: {
    chip: 'bg-yellow-100 text-yellow-800 border-yellow-200',
    icon: AlertTriangle,
    iconClass: 'text-yellow-600',
    hex: '#d97706',
  },
  failed: {
    chip: 'bg-red-100 text-red-800 border-red-200',
    icon: X,
    iconClass: 'text-red-600',
    hex: '#dc2626',
  },
  rebuilding: {
    chip: 'bg-orange-100 text-orange-800 border-orange-200',
    icon: Settings,
    // Motion with meaning: the gear itself is static — real progress renders
    // through the data-bound sync indicator / ProgressBar.
    iconClass: 'text-rebuilding-600',
    hex: '#ea580c',
  },
  spare: {
    chip: 'bg-blue-100 text-blue-800 border-blue-200',
    icon: Shield,
    iconClass: 'text-blue-600',
    hex: '#2563eb',
  },
  removing: {
    chip: 'bg-purple-100 text-purple-800 border-purple-200',
    icon: Clock,
    iconClass: 'text-purple-600',
    hex: '#9333ea',
  },
  stale: {
    chip: SYNC_STATE_STYLES.stale.chip,
    icon: AlertTriangle,
    iconClass: 'text-amber-600',
    hex: '#d97706',
  },
  standby: {
    chip: SYNC_STATE_STYLES.standby.chip,
    icon: Clock,
    iconClass: 'text-blue-600',
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
    bgColor: 'bg-blue-50',
    borderColor: 'border-blue-200',
  },
  healthy: {
    name: 'Healthy Volumes',
    short: 'healthy',
    severity: 'good',
    icon: '✅',
    description: 'All replicas operational',
    bgColor: 'bg-green-50',
    borderColor: 'border-green-200',
  },
  degraded: {
    name: 'Degraded Volumes',
    short: 'degraded',
    severity: 'warning',
    icon: '🟡',
    description: 'Volumes with reduced redundancy',
    bgColor: 'bg-yellow-50',
    borderColor: 'border-yellow-200',
  },
  failed: {
    name: 'Failed Volumes',
    short: 'failed',
    severity: 'critical',
    icon: '🔴',
    description: 'Volumes that have completely failed',
    bgColor: 'bg-red-50',
    borderColor: 'border-red-200',
  },
  faulted: {
    name: 'Faulted Volumes (Degraded + Failed)',
    short: 'faulted',
    severity: 'mixed',
    icon: '⚠️',
    description: 'Both degraded and failed volumes',
    bgColor: 'bg-orange-50',
    borderColor: 'border-orange-200',
  },
  rebuilding: {
    name: 'Volumes with Recovering Replicas',
    short: 'recovering',
    severity: 'recovery',
    icon: '🔄',
    description: 'Volumes with replica recovery activity (catch-up, standby, or legacy rebuild)',
    bgColor: 'bg-orange-50',
    borderColor: 'border-orange-200',
  },
  'local-nvme': {
    name: 'Local NVMe Volumes',
    short: 'local NVMe',
    severity: 'performance',
    icon: '⚡',
    description: 'High-performance local storage',
    bgColor: 'bg-blue-50',
    borderColor: 'border-blue-200',
  },
  orphaned: {
    name: 'Orphaned Volumes (Raw SPDK)',
    short: 'orphaned',
    severity: 'cleanup',
    icon: '🛡️',
    description: 'Raw SPDK volumes without Kubernetes backing - cleanup candidates',
    bgColor: 'bg-amber-50',
    borderColor: 'border-amber-200',
  },
};

const ALL_VOLUMES_DISPLAY = VOLUME_FILTER_DISPLAY.all as VolumeFilterDisplay;

export function volumeFilterDisplay(filter: string | null | undefined): VolumeFilterDisplay {
  return VOLUME_FILTER_DISPLAY[filter ?? 'all'] ?? ALL_VOLUMES_DISPLAY;
}
