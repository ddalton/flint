import { useEvents } from '../../hooks/useEvents';
import { EventTimelinePanel, HotRejoinWindowsPanel } from './EventPanels';

// 2c: the cluster-wide after-the-fact surfaces — completed hot-rejoin windows
// and the engine event timeline. The panels themselves live in EventPanels
// and are reused by the per-volume embedding in the volume detail view (2b).

export function EventsTab() {
  const { data, isLoading, isError, error } = useEvents();

  if (isLoading) {
    return <div className="p-8 text-center text-gray-500">Loading engine events…</div>;
  }
  if (isError) {
    return (
      <div className="p-4 bg-red-50 border border-red-200 rounded-lg text-red-800 text-sm">
        Failed to load events: {(error as Error).message}
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <HotRejoinWindowsPanel windows={data?.windows ?? []} />
      <EventTimelinePanel events={data?.events ?? []} />
    </div>
  );
}
