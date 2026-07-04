import { useQuery } from '@tanstack/react-query';
import { apiFetch } from '../api/client';
import type { components } from '../api/schema';

type Schemas = components['schemas'];

// Wire types straight from the generated OpenAPI schema — the backend's
// SnapshotTimelineResponse struct IS the contract.
export type TimelineEvent = Schemas['SnapshotTimelineEvent'];
export type TimelineReplica = Schemas['TimelineReplica'];
export type SnapshotTimeline = Schemas['SnapshotTimelineResponse'];

const fetchTimeline = async (volume: string): Promise<SnapshotTimeline> => {
  const response = await apiFetch(`/api/snapshots/timeline?volume=${encodeURIComponent(volume)}`);
  const contentType = response.headers.get('content-type') || '';
  if (!response.ok || contentType.indexOf('application/json') === -1) {
    throw new Error(
      response.ok
        ? 'Received non-JSON response from backend'
        : `Backend error (HTTP ${response.status})`
    );
  }
  return response.json();
};

// 10s polling mirrors useEvents: the epoch lane advances every scheduler
// tick, so the timeline is inherently live data.
export const useSnapshotTimeline = (volume: string | null) =>
  useQuery({
    queryKey: ['snapshot-timeline', volume],
    queryFn: () => fetchTimeline(volume as string),
    enabled: volume !== null && volume !== '' && volume !== 'all',
    refetchInterval: 10_000,
  });

/** Delete a user snapshot through its VolumeSnapshot CR (admin only). */
export const deleteVolumeSnapshot = async (
  namespace: string,
  name: string
): Promise<Schemas['DeleteVolumeSnapshotResponse']> => {
  const response = await apiFetch(
    `/api/volumesnapshots/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`,
    { method: 'DELETE' }
  );
  const body = await response.json().catch(() => null);
  if (!response.ok) {
    throw new Error(body?.error ?? `Delete failed (HTTP ${response.status})`);
  }
  return body;
};
