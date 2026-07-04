import { useEvents } from '../../hooks/useEvents';
import { EventTimelinePanel, HotRejoinWindowsPanel } from './EventPanels';
import { AsyncView } from '../ui/AsyncView';

// 2c: the cluster-wide after-the-fact surfaces — completed hot-rejoin windows
// and the engine event timeline. The panels themselves live in EventPanels
// and are reused by the per-volume embedding in the volume detail view (2b).
// State handling follows the kit's AsyncView contract; the panels render
// their own explanatory empty states, so "no events" is data, not empty.

export function EventsTab() {
  const { data, isLoading, isError, error, refetch } = useEvents();

  return (
    <AsyncView
      loading={isLoading}
      error={isError ? (error as Error).message : null}
      data={data}
      onRetry={() => refetch()}
    >
      {(events) => (
        <div className="space-y-6">
          <HotRejoinWindowsPanel windows={events.windows ?? []} />
          <EventTimelinePanel events={events.events ?? []} />
        </div>
      )}
    </AsyncView>
  );
}
