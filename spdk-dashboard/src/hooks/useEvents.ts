import { useQuery } from '@tanstack/react-query';
import { apiFetch } from '../api/client';
import type { components } from '../api/schema';

type Schemas = components['schemas'];

export type EventCategory =
  | 'hot_rejoin'
  | 'data_path'
  | 'catchup'
  | 'cutover'
  | 'epoch'
  | 'health'
  | 'other';

// Wire types aliased from the generated OpenAPI schema (api/openapi.json);
// category/path stay narrowed to the literal unions the backend emits.
export type EngineEvent = Omit<Schemas['DashboardEvent'], 'category'> & {
  category: EventCategory;
};

export type WindowStep = Schemas['WindowStep'];

export type HotRejoinWindow = Omit<Schemas['HotRejoinWindow'], 'path'> & {
  path: 'inline' | 'esnap';
};

export type EventsData = Omit<Schemas['EventsResponse'], 'events' | 'windows'> & {
  events: EngineEvent[];
  windows: HotRejoinWindow[];
};

// The engine's hot-rejoin window target (FLINT_HOT_REJOIN default 2s) —
// windows at or over this get flagged, mirroring HotRejoinWindowSlow.
export const WINDOW_TARGET_MS = 2000;

const fetchEvents = async (volume?: string): Promise<EventsData> => {
  const qs = volume ? `?volume=${encodeURIComponent(volume)}` : '';
  const response = await apiFetch(`/api/events${qs}`);
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

// K8s garbage-collects events after ~1h, so the timeline is inherently
// recent history; 10s polling keeps it live without hammering the API.
export const useEvents = (volume?: string) =>
  useQuery({
    queryKey: ['events', volume ?? null],
    queryFn: () => fetchEvents(volume),
    refetchInterval: 10_000,
  });
