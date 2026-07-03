import { useQuery } from '@tanstack/react-query';
import { apiFetch } from '../api/client';

export type EventCategory =
  | 'hot_rejoin'
  | 'data_path'
  | 'catchup'
  | 'cutover'
  | 'epoch'
  | 'health'
  | 'other';

export interface EngineEvent {
  timestamp: string | null;
  event_type: string; // 'Normal' | 'Warning'
  reason: string;
  volume: string;
  message: string;
  category: EventCategory;
  reporting_instance: string;
}

export interface WindowStep {
  name: string;
  ms: number;
}

export interface HotRejoinWindow {
  timestamp: string | null;
  volume: string;
  node: string;
  raid: string;
  epoch: string;
  window_ms: number;
  steps: WindowStep[];
  path: 'inline' | 'esnap';
  estimator_bytes: number | null;
}

export interface EventsData {
  events: EngineEvent[];
  windows: HotRejoinWindow[];
}

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
