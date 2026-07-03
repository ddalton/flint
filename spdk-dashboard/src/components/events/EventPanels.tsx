import { useState } from 'react';
import { Activity, AlertTriangle, Timer, Zap } from 'lucide-react';
import { WINDOW_TARGET_MS } from '../../hooks/useEvents';
import type { EngineEvent, EventCategory, HotRejoinWindow } from '../../hooks/useEvents';

// The after-the-fact surfaces for what the live sync indicator cannot show —
// completed hot-rejoin windows (step timings vs the 2s target) and the engine
// event timeline. Shared by the cluster-wide Events tab (2c) and the
// per-volume embedding in the volume detail view (2b).

const CATEGORY_LABELS: Record<EventCategory, string> = {
  hot_rejoin: 'Hot rejoin',
  catchup: 'Catch-up',
  data_path: 'Data path',
  epoch: 'Epochs',
  health: 'Health',
  cutover: 'Cutover',
  other: 'Other',
};

const formatTime = (ts: string | null | undefined) => {
  if (!ts) return '—';
  const d = new Date(ts);
  return isNaN(d.getTime()) ? ts : d.toLocaleString();
};

const shortVolume = (v: string) => (v.length > 20 ? `${v.slice(0, 20)}…` : v);

const formatMiB = (bytes: number | null | undefined) =>
  bytes == null ? null : `${(bytes / (1024 * 1024)).toFixed(bytes % (1024 * 1024) === 0 ? 0 : 1)} MiB`;

function WindowRow({ w, showVolume }: { w: HotRejoinWindow; showVolume: boolean }) {
  const overTarget = w.window_ms > WINDOW_TARGET_MS;
  const pct = Math.min(100, (w.window_ms / WINDOW_TARGET_MS) * 100);
  const estimator = formatMiB(w.estimator_bytes);
  return (
    <tr className="hover:bg-gray-50">
      <td className="px-4 py-3 whitespace-nowrap text-sm text-gray-500">{formatTime(w.timestamp)}</td>
      {showVolume && (
        <td className="px-4 py-3 whitespace-nowrap text-sm font-medium" title={w.volume}>
          {shortVolume(w.volume)}
        </td>
      )}
      <td className="px-4 py-3 whitespace-nowrap text-sm">{w.node}</td>
      <td className="px-4 py-3 whitespace-nowrap">
        <div className="flex items-center gap-2">
          <div
            className="w-24 bg-gray-200 rounded-full h-2"
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={WINDOW_TARGET_MS}
            aria-valuenow={Math.min(w.window_ms, WINDOW_TARGET_MS)}
            aria-valuetext={`${w.window_ms}ms of ${WINDOW_TARGET_MS}ms target`}
          >
            <div
              className={`h-2 rounded-full ${overTarget ? 'bg-amber-500' : 'bg-green-500'}`}
              style={{ width: `${pct}%` }}
            />
          </div>
          <span className={`text-sm font-medium tabular-nums ${overTarget ? 'text-amber-700' : 'text-green-700'}`}>
            {w.window_ms}ms
          </span>
          {overTarget && (
            <span title={`Over the ${WINDOW_TARGET_MS}ms target`}>
              <AlertTriangle className="w-4 h-4 text-amber-500" />
            </span>
          )}
        </div>
      </td>
      <td className="px-4 py-3 whitespace-nowrap">
        <span
          className={`inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-xs font-medium border ${
            w.path === 'inline'
              ? 'bg-green-100 text-green-800 border-green-200'
              : 'bg-blue-100 text-blue-800 border-blue-200'
          }`}
          title={
            w.path === 'inline'
              ? 'Final delta copied inside the window — fully redundant immediately'
              : 'Esnap clone in the window; chain localized afterwards'
          }
        >
          {w.path === 'inline' ? <Zap className="w-3 h-3" /> : <Timer className="w-3 h-3" />}
          {w.path}
        </span>
        {estimator && <span className="ml-2 text-xs text-gray-500">{estimator} est.</span>}
      </td>
      <td className="px-4 py-3 text-xs text-gray-500">
        {w.steps.map((s) => `${s.name} ${s.ms}ms`).join(' · ')}
      </td>
    </tr>
  );
}

function EventRow({ e, showVolume }: { e: EngineEvent; showVolume: boolean }) {
  const warning = e.event_type === 'Warning';
  return (
    <div className="flex items-start gap-3 px-4 py-3 hover:bg-gray-50">
      <span
        aria-hidden="true"
        className={`mt-1.5 w-2 h-2 rounded-full flex-shrink-0 ${warning ? 'bg-amber-500' : 'bg-green-500'}`}
      />
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-baseline gap-x-2">
          <span className={`text-sm font-semibold ${warning ? 'text-amber-800' : 'text-gray-900'}`}>
            {e.reason}
          </span>
          {showVolume && (
            <span className="text-xs text-gray-500" title={e.volume}>
              {shortVolume(e.volume)}
            </span>
          )}
          <span className="text-xs text-gray-400">{formatTime(e.timestamp)}</span>
        </div>
        <p className="text-sm text-gray-600 break-words">{e.message}</p>
      </div>
    </div>
  );
}

export function HotRejoinWindowsPanel({
  windows,
  showVolume = true,
}: {
  windows: HotRejoinWindow[];
  showVolume?: boolean;
}) {
  const headers = showVolume
    ? ['Time', 'Volume', 'Node', 'Window', 'Path', 'Steps']
    : ['Time', 'Node', 'Window', 'Path', 'Steps'];
  return (
    <div className="bg-white rounded-lg shadow overflow-hidden">
      <div className="px-4 py-3 border-b bg-gray-50 flex items-center gap-2">
        <Timer className="w-5 h-5 text-gray-600" />
        <h3 className="font-semibold">Hot-rejoin windows</h3>
        <span className="text-xs text-gray-500">
          quiesce-to-unquiesce duration vs the {WINDOW_TARGET_MS / 1000}s target
        </span>
      </div>
      {windows.length === 0 ? (
        <div className="px-4 py-8 text-center text-sm text-gray-500">
          No completed hot-rejoin windows in recent history. When a failed replica
          leg hot-rejoins its raid, the window and its step timings appear here.
          (Kubernetes retains events for about an hour.)
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="min-w-full divide-y divide-gray-200">
            <thead className="bg-gray-50">
              <tr>
                {headers.map((h) => (
                  <th
                    key={h}
                    className="px-4 py-2 text-left text-xs font-medium text-gray-500 uppercase tracking-wider"
                  >
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody className="bg-white divide-y divide-gray-200">
              {windows.map((w, i) => (
                <WindowRow key={`${w.volume}-${w.timestamp}-${i}`} w={w} showVolume={showVolume} />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

export function EventTimelinePanel({
  events,
  showVolume = true,
}: {
  events: EngineEvent[];
  showVolume?: boolean;
}) {
  const [category, setCategory] = useState<EventCategory | 'all'>('all');

  const counts = events.reduce<Record<string, number>>((acc, e) => {
    acc[e.category] = (acc[e.category] ?? 0) + 1;
    return acc;
  }, {});
  const visible = category === 'all' ? events : events.filter((e) => e.category === category);
  const presentCategories = (Object.keys(CATEGORY_LABELS) as EventCategory[]).filter(
    (c) => counts[c]
  );

  return (
    <div className="bg-white rounded-lg shadow overflow-hidden">
      <div className="px-4 py-3 border-b bg-gray-50 flex flex-wrap items-center gap-2">
        <Activity className="w-5 h-5 text-gray-600" />
        <h3 className="font-semibold">Event timeline</h3>
        <div className="ml-auto flex flex-wrap gap-1">
          <button
            onClick={() => setCategory('all')}
            className={`px-2 py-1 text-xs rounded-full border ${
              category === 'all'
                ? 'bg-blue-100 text-blue-800 border-blue-300'
                : 'bg-white text-gray-600 border-gray-200 hover:bg-gray-50'
            }`}
          >
            All ({events.length})
          </button>
          {presentCategories.map((c) => (
            <button
              key={c}
              onClick={() => setCategory(c)}
              className={`px-2 py-1 text-xs rounded-full border ${
                category === c
                  ? 'bg-blue-100 text-blue-800 border-blue-300'
                  : 'bg-white text-gray-600 border-gray-200 hover:bg-gray-50'
              }`}
            >
              {CATEGORY_LABELS[c]} ({counts[c]})
            </button>
          ))}
        </div>
      </div>
      {visible.length === 0 ? (
        <div className="px-4 py-8 text-center text-sm text-gray-500">
          No engine events in recent history — replica state transitions,
          hot rejoins, and data-path changes will appear here as they happen.
        </div>
      ) : (
        <div className="divide-y divide-gray-100">
          {visible.map((e, i) => (
            <EventRow key={`${e.volume}-${e.timestamp}-${e.reason}-${i}`} e={e} showVolume={showVolume} />
          ))}
        </div>
      )}
    </div>
  );
}
