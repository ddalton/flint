import type { SyncState } from '../../hooks/useDashboardData';

// Semantic style tokens for replica sync states — the design-system seed.
// One source for label + color so the Volumes table, RAID topology, and node
// detail render the same state identically (Phase 3 migrates the volume/disk
// status switches here too).
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
