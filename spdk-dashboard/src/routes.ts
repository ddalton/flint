// URL state model (Phase 3): the tab is the path segment, everything else
// rides in search params — deep-linkable and refresh-safe.
//
//   /volumes?filter=degraded&disk=<diskId>&volume=<volumeId>
//   /disks?replicas=<volumeId>
//   /snapshots?snapshot=<snapshotId>
//
// Cross-tab FILTER params (filter / disk / replicas) persist across tab
// switches — an operator filtering to degraded volumes keeps that context
// everywhere. DETAIL params (volume / snapshot) belong to one tab's modal
// and are dropped when navigating away from it.

export const TAB_IDS = [
  'overview',
  'volumes',
  'disks',
  'events',
  'snapshots',
  'disk-setup',
  'remote-storage',
  'nodes',
] as const;

export type TabId = (typeof TAB_IDS)[number];

export const DEFAULT_TAB: TabId = 'overview';

// The bare path "/" also means Overview, but with one difference: it is the
// only entry point where the state-aware landing (plan Decision 2) may
// redirect a fresh cluster to Disk Setup. An explicit /overview deep link is
// a user choice and is never hijacked.
export function parseTab(segment: string | undefined): TabId | null {
  if (segment === undefined || segment === '') return DEFAULT_TAB;
  return (TAB_IDS as readonly string[]).includes(segment) ? (segment as TabId) : null;
}

const DETAIL_PARAM_HOME: Record<string, TabId> = {
  volume: 'volumes',
  snapshot: 'snapshots',
};

// Search string for a tab link: keeps filter context, drops detail params
// that don't belong on the target tab.
export function searchForTab(current: URLSearchParams, target: TabId): string {
  const next = new URLSearchParams(current);
  for (const [param, home] of Object.entries(DETAIL_PARAM_HOME)) {
    if (home !== target) next.delete(param);
  }
  const qs = next.toString();
  return qs ? `?${qs}` : '';
}

// Volume filter values the URL accepts; anything else degrades to 'all'
// instead of crashing on a hand-typed link.
const VOLUME_FILTERS = [
  'all',
  'orphaned',
  'healthy',
  'degraded',
  'failed',
  'faulted',
  'rebuilding',
  'local-nvme',
] as const;

export type UrlVolumeFilter = (typeof VOLUME_FILTERS)[number];

export function parseVolumeFilter(value: string | null): UrlVolumeFilter {
  return value && (VOLUME_FILTERS as readonly string[]).includes(value)
    ? (value as UrlVolumeFilter)
    : 'all';
}
