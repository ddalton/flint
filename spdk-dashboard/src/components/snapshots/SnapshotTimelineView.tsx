import React, { useEffect, useMemo, useState, useLayoutEffect, useCallback } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import {
  GitBranch, Database, Search, Camera, Layers, Trash2, Loader2, X, ZoomIn,
} from 'lucide-react';
import { getRole } from '../../api/client';
import { resolveVolumeInput, volumeInputMatches } from './volumeSearch';
import { Button } from '../ui/Button';
import {
  useSnapshotTimeline, deleteVolumeSnapshot,
  type TimelineEvent,
} from '../../hooks/useSnapshotTimeline';
import {
  computeDomain, timeTicks, bucketEpochs, clusterMarkers, pxToTime, relTime,
  type TimeDomain,
} from './timelineLayout';
import { ConfirmModal } from '../ui/ConfirmModal';
import { TimelineBrush } from './TimelineBrush';

// Design (adapted from production observability idioms):
// - Two lanes, not one: sparse human events (user VolumeSnapshots, diamond
//   flag markers — Elastic APM's icon-capped annotation line) ride above a
//   bucketed density ribbon of machine events (engine epochs — the GitHub
//   contribution strip rotated to 1-D). Dense periodic data and sparse
//   important data get different encodings, never the same marker.
// - "Now" anchors the right edge with a pulse (Datadog live-tail idiom);
//   axis labels are absolute wall-clock, relative phrasing only in popovers.
// - Hover shows a read-only crosshair+tooltip; CLICK pins a popover that
//   holds the actions (Honeycomb marker windows, Grafana's delete-in-
//   annotation-tooltip). Buttons never live in hover-only surfaces.
// - Colliding user markers merge into a "+N" chip (map cluster-marker
//   pattern) instead of overdrawing.
// - Focus+context brush zoom: a miniature full-history strip under the axis
//   (TimelineBrush) on which a dragged window re-domains the lanes above.
//   The zoom window is absolute wall-clock, so live refetches extend the
//   context strip without silently moving a pinned window.

const LANE_USER_Y = 40;
const LANE_EPOCH_TOP = 78;
const LANE_EPOCH_H = 22;
const AXIS_Y = 122;
const SVG_H = 148;

const USER_COLOR = '#7c3aed'; // violet-600: user snapshots never read as "error"
const USER_COLOR_SOFT = '#a78bfa';
const EPOCH_COLOR = '#3b82f6'; // blue-500 ramp for the density ribbon
const ORPHAN_COLOR = '#9ca3af';
const GHOST_COLOR = '#dc2626'; // red-600: a ghost IS an error state

interface Selection {
  events: TimelineEvent[];
  x: number;
}

// Ghost: the VolumeSnapshot CR still exists (and typically still claims
// ready), but no node's SPDK holds a copy — the data is gone and restore
// will fail. The inverse of an orphan (SPDK copy without a CR).
const isGhost = (e: TimelineEvent): boolean =>
  e.kind === 'user' && !e.orphan && e.nodes.length === 0;

const eventTimeMs = (e: TimelineEvent): number =>
  e.created_at ? new Date(e.created_at).getTime() : NaN;

const fmtSize = (bytes?: number | null) =>
  bytes == null ? '—' : `${(bytes / 1024 ** 3).toFixed(1)}GiB`;

const fmtAbs = (t: number) => new Date(t).toLocaleTimeString();

/** One diamond flag marker (or a +N cluster chip) on the user lane. */
const UserMarker: React.FC<{
  x: number;
  events: TimelineEvent[];
  selected: boolean;
  onSelect: () => void;
}> = ({ x, events, selected, onSelect }) => {
  const single = events.length === 1 ? events[0] ?? null : null;
  const ghost = single ? isGhost(single) : false;
  // A cluster can hide a ghost among healthy snapshots — surface it on the
  // collapsed marker so a scan of the lane never reads a ghost as healthy.
  const clusterGhosts = single ? 0 : events.filter(isGhost).length;
  const color = ghost ? GHOST_COLOR : single?.orphan ? ORPHAN_COLOR : USER_COLOR;
  const label = single
    ? `User snapshot ${single.name}${ghost ? ' (no copies)' : ''}`
    : `${events.length} user snapshots${clusterGhosts ? ` (${clusterGhosts} without copies)` : ''}`;
  return (
    <g
      role="button"
      tabIndex={0}
      aria-label={label}
      className="cursor-pointer focus:outline-none"
      onClick={(ev) => {
        ev.stopPropagation();
        onSelect();
      }}
      onKeyDown={(ev) => {
        if (ev.key === 'Enter' || ev.key === ' ') {
          ev.preventDefault();
          onSelect();
        }
      }}
    >
      {/* Oversized invisible hit target — nobody should have to hit a 2px stem. */}
      <rect x={x - 12} y={LANE_USER_Y - 14} width={24} height={AXIS_Y - LANE_USER_Y + 14} fill="transparent" />
      <line x1={x} y1={LANE_USER_Y} x2={x} y2={AXIS_Y} stroke={color} strokeWidth={selected ? 2 : 1.5} strokeOpacity={0.55} />
      {single ? (
        <>
          <rect
            x={-6.5}
            y={-6.5}
            width={13}
            height={13}
            rx={2}
            transform={`translate(${x}, ${LANE_USER_Y}) rotate(45)`}
            // A ghost diamond is hollow with a red outline: the CR shell
            // exists, the data doesn't.
            fill={single.ready && !single.orphan && !ghost ? color : '#ffffff'}
            stroke={color}
            strokeWidth={2}
            strokeDasharray={single.orphan ? '3,2' : undefined}
          />
          {ghost && (
            <text
              x={x}
              y={LANE_USER_Y + 3.5}
              textAnchor="middle"
              fontSize={10}
              fontWeight={700}
              fill={GHOST_COLOR}
              pointerEvents="none"
            >
              !
            </text>
          )}
        </>
      ) : (
        <>
          <circle cx={x} cy={LANE_USER_Y} r={11} fill={USER_COLOR} />
          <text x={x} y={LANE_USER_Y + 4} textAnchor="middle" fill="#fff" fontSize={11} fontWeight={700}>
            +{events.length}
          </text>
          {clusterGhosts > 0 && (
            // Red ring + corner dot: this cluster contains ≥1 ghost.
            <>
              <circle cx={x} cy={LANE_USER_Y} r={13} fill="none" stroke={GHOST_COLOR} strokeWidth={2} />
              <circle cx={x + 10} cy={LANE_USER_Y - 10} r={4} fill={GHOST_COLOR} stroke="#ffffff" strokeWidth={1} />
            </>
          )}
        </>
      )}
      {selected && (
        <circle cx={x} cy={LANE_USER_Y} r={events.length > 1 ? 14 : 12} fill="none" stroke={USER_COLOR_SOFT} strokeWidth={2} />
      )}
    </g>
  );
};

export const SnapshotTimelineView: React.FC<{
  selectedVolume: string;
  onVolumeChange: (volumeId: string) => void;
  availableVolumes: string[];
  // PV id -> PVC name, for search + display. Optional: absent names just
  // degrade to id-only matching.
  volumeNames?: Record<string, string>;
}> = ({ selectedVolume, onVolumeChange, availableVolumes, volumeNames = {} }) => {
  const isValidVolume = availableVolumes.includes(selectedVolume);
  const { data, isLoading, error } = useSnapshotTimeline(isValidVolume ? selectedVolume : null);
  const queryClient = useQueryClient();

  // What the user typed, decoupled from the resolved selection so typing a
  // partial name doesn't snap the input to the full pv id mid-keystroke.
  const [typed, setTyped] = useState(selectedVolume === 'all' ? '' : selectedVolume);
  useEffect(() => {
    if (isValidVolume) setTyped(selectedVolume);
    else if (selectedVolume === 'all') setTyped('');
  }, [selectedVolume, isValidVolume]);

  // Callback-ref + effect-on-element: the container div only mounts once
  // data arrives, so a mount-time effect on a plain ref would observe null
  // and the chart would stay at the fallback width forever.
  const [containerEl, setContainerEl] = useState<HTMLDivElement | null>(null);
  const [width, setWidth] = useState(900);
  useLayoutEffect(() => {
    if (!containerEl) return;
    if (containerEl.clientWidth) setWidth(containerEl.clientWidth);
    const observer = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect.width;
      if (w) setWidth(w);
    });
    observer.observe(containerEl);
    return () => observer.disconnect();
  }, [containerEl]);

  const [selection, setSelection] = useState<Selection | null>(null);
  const [hoverX, setHoverX] = useState<number | null>(null);
  // Brush zoom window (absolute ms). null = full history. A window from one
  // volume is meaningless on another, so switching volumes resets it.
  const [zoom, setZoom] = useState<TimeDomain | null>(null);
  useEffect(() => {
    setZoom(null);
  }, [selectedVolume]);
  const [confirming, setConfirming] = useState<TimelineEvent | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const isAdmin = getRole() === 'admin';

  const nowMs = data ? new Date(data.now).getTime() : Date.now();
  const events = useMemo(() => data?.events ?? [], [data]);
  const userEvents = useMemo(() => events.filter((e) => e.kind === 'user'), [events]);
  const epochEvents = useMemo(() => events.filter((e) => e.kind === 'epoch'), [events]);
  const datelessOrphans = useMemo(() => userEvents.filter((e) => !e.created_at), [userEvents]);
  const ghostCount = useMemo(() => userEvents.filter(isGhost).length, [userEvents]);

  const domain: TimeDomain | null = useMemo(
    () => computeDomain(events.map(eventTimeMs), nowMs),
    [events, nowMs]
  );

  // The lanes render the zoom window when one is brushed, the full history
  // otherwise. The context strip below always renders `domain`.
  const view: TimeDomain | null = zoom ?? domain;

  const userClusters = useMemo(() => {
    if (!view) return [];
    return clusterMarkers(
      userEvents.filter((e) => e.created_at).map((e) => ({ timeMs: eventTimeMs(e), item: e })),
      view,
      width
    );
  }, [userEvents, view, width]);

  const epochBuckets = useMemo(() => {
    if (!view) return [];
    return bucketEpochs(
      epochEvents.filter((e) => e.created_at).map((e) => ({ timeMs: eventTimeMs(e), item: e })),
      view,
      width
    );
  }, [epochEvents, view, width]);

  const maxBucket = Math.max(1, ...epochBuckets.map((b) => b.count));
  const ticks = useMemo(() => (view ? timeTicks(view, width) : []), [view, width]);

  const userTimesMs = useMemo(
    () => userEvents.map(eventTimeMs).filter((t) => Number.isFinite(t)),
    [userEvents]
  );
  const epochTimesMs = useMemo(
    () => epochEvents.map(eventTimeMs).filter((t) => Number.isFinite(t)),
    [epochEvents]
  );

  // Marker x-positions shift when the domain changes, so a pinned popover's
  // anchor goes stale — close it with the zoom gesture that moved it.
  const handleZoomChange = useCallback((win: TimeDomain | null) => {
    setSelection(null);
    setDeleteError(null);
    setZoom(win);
  }, []);

  // The right edge is only "now" when the window reaches it; zoomed into
  // the past, drawing the live pulse there would be a lie.
  const showNow = !zoom || (domain !== null && zoom.max >= domain.max - 500);

  const onMouseMove = useCallback((ev: React.MouseEvent<SVGSVGElement>) => {
    const rect = ev.currentTarget.getBoundingClientRect();
    setHoverX(ev.clientX - rect.left);
  }, []);

  const closePopover = useCallback(() => {
    setSelection(null);
    setDeleteError(null);
  }, []);

  const runDelete = async (event: TimelineEvent) => {
    if (!event.vs_namespace || !event.vs_name) return;
    setDeleting(true);
    setDeleteError(null);
    try {
      await deleteVolumeSnapshot(event.vs_namespace, event.vs_name);
      setConfirming(null);
      closePopover();
      await queryClient.invalidateQueries({ queryKey: ['snapshot-timeline'] });
    } catch (e) {
      setDeleteError(e instanceof Error ? e.message : String(e));
      setConfirming(null);
    } finally {
      setDeleting(false);
    }
  };

  const selectedEvent = selection?.events.length === 1 ? selection.events[0] : null;
  const popoverLeft = selection ? Math.min(Math.max(selection.x, 150), Math.max(width - 150, 150)) : 0;

  return (
    <div className="space-y-6">
      <div className="bg-white border border-gray-200 rounded-lg shadow-sm p-6">
        <div className="flex items-center justify-between mb-1 flex-wrap gap-3">
          <h3 className="text-section text-gray-900 flex items-center gap-2">
            <GitBranch className="w-5 h-5 text-blue-600" />
            Snapshot Timeline
          </h3>
          <div className="relative flex items-center gap-2">
            <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 w-4 h-4 text-gray-400" />
            <input
              id="volume-search"
              type="text"
              list="volume-list"
              value={typed}
              onChange={(e) => {
                const v = e.target.value;
                setTyped(v);
                if (v === '') {
                  onVolumeChange('all');
                  return;
                }
                const resolved = resolveVolumeInput(v, availableVolumes, volumeNames);
                onVolumeChange(resolved ?? v);
              }}
              placeholder="Search by PVC name or volume id..."
              className="w-full pl-10 pr-4 py-2 border border-gray-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-blue-500"
            />
            <datalist id="volume-list">
              {availableVolumes.flatMap((volume) => {
                const name = volumeNames[volume];
                return [
                  <option key={volume} value={volume} label={name} />,
                  ...(name ? [<option key={`${volume}-name`} value={name} label={volume} />] : []),
                ];
              })}
            </datalist>
          </div>
        </div>

        {!isValidVolume ? (
          <div className="text-center py-12">
            <Database className="w-16 h-16 text-gray-400 mx-auto mb-4" />
            <h3 className="text-lg font-medium text-gray-900 mb-2">
              {selectedVolume === 'all' ? 'Please Select a Volume' : 'Volume Not Found'}
            </h3>
            <p className="text-gray-500">
              {selectedVolume === 'all'
                ? 'Search by PVC name or volume id — only volumes with snapshot history are listed.'
                : `Nothing matches "${selectedVolume}" exactly. Search by PVC name or volume id.`}
            </p>
            {selectedVolume !== 'all' && (() => {
              const candidates = volumeInputMatches(selectedVolume, availableVolumes, volumeNames);
              if (candidates.length === 0) return null;
              return (
                <div className="mt-4 flex flex-wrap justify-center gap-2">
                  {candidates.slice(0, 5).map(id => (
                    <Button key={id} variant="link" onClick={() => onVolumeChange(id)}>
                      {volumeNames[id] ? `${volumeNames[id]} (${id.slice(0, 12)}…)` : id}
                    </Button>
                  ))}
                  {candidates.length > 5 && (
                    <span className="text-sm text-gray-500">+{candidates.length - 5} more — keep typing</span>
                  )}
                </div>
              );
            })()}
          </div>
        ) : isLoading ? (
          <div className="flex items-center justify-center py-12 text-gray-500 gap-2">
            <Loader2 className="w-5 h-5 animate-spin" /> Loading timeline…
          </div>
        ) : error ? (
          <div className="text-center py-12 text-sm text-failed-600">
            Could not load the timeline: {error instanceof Error ? error.message : String(error)}
          </div>
        ) : !domain ? (
          <div className="text-center py-12">
            <Camera className="w-16 h-16 text-gray-400 mx-auto mb-4" />
            <h3 className="text-lg font-medium text-gray-900 mb-2">No Snapshot History Yet</h3>
            <p className="text-gray-500">
              No user snapshots or engine epochs are recorded for this volume.
            </p>
          </div>
        ) : (
          <>
            {/* Legend + live state chips */}
            <div className="flex items-center justify-between flex-wrap gap-2 mb-2 text-xs text-gray-600">
              <div className="flex items-center gap-4">
                <span className="flex items-center gap-1.5">
                  <span
                    className="inline-block w-2.5 h-2.5 rotate-45 rounded-[2px]"
                    style={{ backgroundColor: USER_COLOR }}
                  />
                  User snapshots · {userEvents.length}
                  {ghostCount > 0 && (
                    <span className="text-red-600 font-medium">
                      · {ghostCount} without copies
                    </span>
                  )}
                </span>
                <span className="flex items-center gap-1.5">
                  <span
                    className="inline-block w-2.5 h-3.5 rounded-[2px]"
                    style={{ backgroundColor: EPOCH_COLOR, opacity: 0.7 }}
                  />
                  Engine epochs · {epochEvents.length}
                  {(data?.untracked_epochs ?? 0) > 0 && (
                    <span className="text-gray-400">(+{data?.untracked_epochs} rotating)</span>
                  )}
                </span>
              </div>
              <div className="flex items-center gap-2">
                {zoom && (
                  <span className="flex items-center gap-1.5 px-2 py-0.5 bg-amber-50 text-amber-700 rounded-full font-medium">
                    <ZoomIn className="w-3 h-3" />
                    {fmtAbs(zoom.min)} – {fmtAbs(zoom.max)}
                    <button
                      onClick={() => handleZoomChange(null)}
                      aria-label="Reset zoom"
                      className="p-0.5 -mr-1 rounded-full hover:bg-amber-100"
                    >
                      <X className="w-3 h-3" />
                    </button>
                  </span>
                )}
                {data?.current_epoch && (
                  <span className="px-2 py-0.5 bg-blue-50 text-blue-700 rounded-full font-mono">
                    epoch #{data.current_epoch.split('-').pop()}
                  </span>
                )}
                {(data?.replicas ?? []).map((r) => (
                  <span
                    key={r.node}
                    title={`${r.node}: ${r.sync_state}${r.last_epoch ? ` (last epoch ${r.last_epoch.split('-').pop()})` : ''}`}
                    className={`px-2 py-0.5 rounded-full ${
                      r.sync_state === 'in_sync'
                        ? 'bg-green-50 text-green-700'
                        : r.sync_state === 'standby'
                          ? 'bg-yellow-50 text-yellow-700'
                          : 'bg-red-50 text-red-700'
                    }`}
                  >
                    {r.node} · {r.sync_state}
                  </span>
                ))}
              </div>
            </div>

            <div ref={setContainerEl} className="relative select-none">
              <svg
                width={width}
                height={SVG_H}
                className="block"
                onMouseMove={onMouseMove}
                onMouseLeave={() => setHoverX(null)}
                onClick={closePopover}
                data-testid="timeline-svg"
              >
                {/* lane labels */}
                <text x={0} y={LANE_USER_Y - 18} fontSize={10} fill="#6b7280" fontWeight={600}>
                  USER SNAPSHOTS
                </text>
                <text x={0} y={LANE_EPOCH_TOP - 6} fontSize={10} fill="#6b7280" fontWeight={600}>
                  ENGINE EPOCHS
                </text>

                {/* epoch lane backdrop + density cells */}
                <rect x={0} y={LANE_EPOCH_TOP} width={width} height={LANE_EPOCH_H} rx={4} fill="#f3f4f6" />
                {epochBuckets.map((b) => (
                  <g key={`eb-${b.x}`}>
                    <rect
                      x={b.x}
                      y={LANE_EPOCH_TOP + 2}
                      width={b.widthPx}
                      height={LANE_EPOCH_H - 4}
                      rx={2}
                      fill={EPOCH_COLOR}
                      fillOpacity={0.3 + 0.7 * (b.count / maxBucket)}
                    >
                      <title>
                        {b.count === 1 && b.items[0]
                          ? `${b.items[0].name} · ${b.items[0].created_at ? fmtAbs(eventTimeMs(b.items[0])) : ''}`
                          : `${b.count} epochs`}
                      </title>
                    </rect>
                  </g>
                ))}

                {/* axis */}
                <line x1={0} y1={AXIS_Y} x2={width} y2={AXIS_Y} stroke="#d1d5db" strokeWidth={1} />
                {ticks.map((t) => (
                  <g key={`tick-${t.x}`}>
                    <line x1={t.x} y1={AXIS_Y} x2={t.x} y2={AXIS_Y + 4} stroke="#9ca3af" strokeWidth={1} />
                    <text x={t.x} y={AXIS_Y + 16} textAnchor="middle" fontSize={10} fill="#6b7280">
                      {t.label}
                    </text>
                  </g>
                ))}

                {/* now anchor: hairline + pulse, only when the window
                    actually reaches the live edge */}
                {showNow && (
                  <>
                    <line x1={width - 1} y1={LANE_USER_Y - 12} x2={width - 1} y2={AXIS_Y} stroke="#10b981" strokeWidth={1} strokeDasharray="2,3" />
                    <circle cx={width - 1} cy={AXIS_Y} r={3.5} fill="#10b981">
                      <animate attributeName="r" values="3;5;3" dur="2s" repeatCount="indefinite" />
                      <animate attributeName="fill-opacity" values="1;0.4;1" dur="2s" repeatCount="indefinite" />
                    </circle>
                    <text x={width - 8} y={LANE_USER_Y - 18} textAnchor="end" fontSize={10} fill="#10b981" fontWeight={600}>
                      now
                    </text>
                  </>
                )}

                {/* crosshair (read-only hover) */}
                {hoverX !== null && !selection && (
                  <g pointerEvents="none">
                    <line x1={hoverX} y1={LANE_USER_Y - 12} x2={hoverX} y2={AXIS_Y} stroke="#9ca3af" strokeWidth={1} strokeDasharray="3,3" />
                    <rect x={Math.min(hoverX + 4, width - 64)} y={AXIS_Y - 18} width={60} height={15} rx={3} fill="#374151" />
                    <text x={Math.min(hoverX + 34, width - 34)} y={AXIS_Y - 7} textAnchor="middle" fontSize={9.5} fill="#fff">
                      {fmtAbs(pxToTime(hoverX, zoom ?? domain, width))}
                    </text>
                  </g>
                )}

                {/* user snapshot markers (drawn last: they own the pointer) */}
                {userClusters.map((c) => (
                  <UserMarker
                    key={`uc-${c.x}`}
                    x={c.x}
                    events={c.items}
                    selected={selection?.events === c.items}
                    onSelect={() => {
                      setDeleteError(null);
                      setSelection({ events: c.items, x: c.x });
                    }}
                  />
                ))}
              </svg>

              {/* Context strip: the full history in miniature, brushed to zoom */}
              <div className="mt-1.5">
                <TimelineBrush
                  domain={domain}
                  zoom={zoom}
                  onZoomChange={handleZoomChange}
                  width={width}
                  userTimesMs={userTimesMs}
                  epochTimesMs={epochTimesMs}
                />
              </div>

              {/* Pinned popover: metadata + actions (click-committed surface) */}
              {selection && (
                <div
                  className="absolute z-20 w-[300px] bg-white border border-gray-200 rounded-lg shadow-xl p-3 text-sm"
                  style={{ left: popoverLeft, top: 8, transform: 'translateX(-50%)' }}
                  role="dialog"
                  aria-label="Snapshot details"
                >
                  {selection.events.length > 1 && !selectedEvent ? (
                    <>
                      <p className="font-semibold text-gray-900 mb-2">
                        {selection.events.length} snapshots here
                      </p>
                      <ul className="space-y-1">
                        {selection.events.map((e) => (
                          <li key={e.id}>
                            <button
                              className="w-full text-left px-2 py-1 rounded hover:bg-violet-50 text-violet-700 font-mono text-xs"
                              onClick={() => setSelection({ events: [e], x: selection.x })}
                            >
                              {e.name}
                              {e.created_at && (
                                <span className="text-gray-400 ml-2">{relTime(eventTimeMs(e), nowMs)}</span>
                              )}
                            </button>
                          </li>
                        ))}
                      </ul>
                    </>
                  ) : selectedEvent ? (
                    <>
                      <div className="flex items-start justify-between gap-2 mb-2">
                        <p className="font-semibold text-gray-900 break-all">{selectedEvent.name}</p>
                        <span
                          className={`shrink-0 px-1.5 py-0.5 rounded text-[10px] font-semibold uppercase ${
                            selectedEvent.orphan
                              ? 'bg-gray-100 text-gray-600'
                              : 'bg-violet-100 text-violet-700'
                          }`}
                        >
                          {selectedEvent.orphan ? 'orphan' : 'user'}
                        </span>
                      </div>
                      <dl className="space-y-1 text-xs text-gray-600">
                        <div className="flex justify-between gap-2">
                          <dt>Created</dt>
                          <dd className="text-right">
                            {selectedEvent.created_at
                              ? `${fmtAbs(eventTimeMs(selectedEvent))} · ${relTime(eventTimeMs(selectedEvent), nowMs)}`
                              : 'unknown (no CR)'}
                          </dd>
                        </div>
                        <div className="flex justify-between gap-2">
                          <dt>Size</dt>
                          <dd>{fmtSize(selectedEvent.size_bytes)}</dd>
                        </div>
                        <div className="flex justify-between gap-2">
                          <dt>Ready</dt>
                          <dd>{selectedEvent.ready ? '✓ yes' : 'no'}</dd>
                        </div>
                        <div className="flex justify-between gap-2">
                          <dt>Replicas on</dt>
                          <dd
                            className={`text-right ${
                              isGhost(selectedEvent) ? 'text-red-600 font-medium' : ''
                            }`}
                          >
                            {selectedEvent.nodes.length
                              ? selectedEvent.nodes.join(', ')
                              : isGhost(selectedEvent)
                                ? 'none'
                                : '—'}
                          </dd>
                        </div>
                        {selectedEvent.vs_namespace && (
                          <div className="flex justify-between gap-2">
                            <dt>Namespace</dt>
                            <dd>{selectedEvent.vs_namespace}</dd>
                          </div>
                        )}
                        {selectedEvent.spdk_name && (
                          <div className="pt-1 border-t border-gray-100 font-mono text-[10px] text-gray-400 break-all">
                            {selectedEvent.spdk_name}
                          </div>
                        )}
                      </dl>
                      {isGhost(selectedEvent) && (
                        <div className="mt-2 p-2 bg-red-50 border border-red-200 rounded text-xs text-red-700">
                          <p className="font-semibold mb-0.5">No SPDK copies exist on any node</p>
                          <p>
                            The VolumeSnapshot still reports ready, but its data is gone —
                            restore will fail. Deleting the snapshot is the clean-up path.
                          </p>
                        </div>
                      )}
                      {deleteError && (
                        <p className="mt-2 text-xs text-failed-600">{deleteError}</p>
                      )}
                      <div className="mt-3 flex justify-end gap-2">
                        <button
                          onClick={closePopover}
                          className="px-2.5 py-1 text-xs rounded border border-gray-300 text-gray-600 hover:bg-gray-50"
                        >
                          Close
                        </button>
                        {!selectedEvent.orphan && selectedEvent.vs_name && (
                          <button
                            onClick={() => setConfirming(selectedEvent)}
                            disabled={!isAdmin}
                            title={isAdmin ? undefined : 'Admin login required'}
                            className="px-2.5 py-1 text-xs rounded bg-failed-600 text-white hover:bg-failed-700 disabled:opacity-40 disabled:cursor-not-allowed flex items-center gap-1"
                          >
                            <Trash2 className="w-3 h-3" /> Delete
                          </button>
                        )}
                      </div>
                    </>
                  ) : null}
                </div>
              )}
            </div>

            {datelessOrphans.length > 0 && (
              <p className="mt-2 text-xs text-gray-400">
                {datelessOrphans.length} orphaned SPDK snapshot
                {datelessOrphans.length > 1 ? 's' : ''} (no VolumeSnapshot CR, creation time
                unknown) not plotted — shown in List View.
              </p>
            )}
          </>
        )}
      </div>

      {confirming && (
        <ConfirmModal
          title="Delete user snapshot"
          subtitle={`VolumeSnapshot ${confirming.vs_namespace}/${confirming.vs_name}`}
          danger={
            <>
              Deletes the VolumeSnapshot CR — the snapshot controller then removes the
              snapshot content and its SPDK copies on{' '}
              {confirming.nodes.length ? confirming.nodes.join(', ') : 'all replicas'} per the
              class deletionPolicy. Restores from this snapshot become impossible.
            </>
          }
          confirmLabel={deleting ? 'Deleting…' : 'Delete snapshot'}
          busy={deleting}
          onConfirm={() => runDelete(confirming)}
          onCancel={() => setConfirming(null)}
        />
      )}

      <div className="bg-blue-50 border border-blue-200 rounded-lg p-6">
        <div className="flex items-start gap-3">
          <Layers className="w-6 h-6 text-blue-600 mt-1 flex-shrink-0" />
          <div>
            <h4 className="font-medium text-blue-900 mb-2">About the Snapshot Timeline</h4>
            <div className="text-sm text-blue-800 space-y-2">
              <p>
                <strong>User snapshots</strong> (violet diamonds) are your VolumeSnapshots, plotted
                at their real creation time from the Kubernetes CR. Click one to inspect it or
                delete it (admin). <strong>Engine epochs</strong> (blue ribbon) are the automatic
                consistency points the replica-rebuild engine cuts and rotates; their density shows
                scheduler cadence. Timestamps come from the CR and the PV sync record — never
                fabricated. The green pulse is “now”.
              </p>
              <p>
                <strong>Zooming:</strong> drag across the miniature full-history strip below the
                lanes to focus on a time window — drag the window’s edges to resize it, its body
                to pan, and click outside it (or the ✕ on the zoom chip) to return to the full
                history. The strip always shows everything, so you never lose orientation.
              </p>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};
