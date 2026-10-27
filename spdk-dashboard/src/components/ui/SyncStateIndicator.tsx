import type { ReplicaStatus, ReplicaSyncInfo, Volume } from '../../hooks/useDashboardData';
import { isReplicaRecovering } from '../../hooks/useDashboardData';
import { SYNC_STATE_STYLES } from './status';

// The Tier-2 engine's live per-replica state. Epoch lag is the observable
// progress measure for stale/standby — lag → 0 is the catch-up. When the
// lag is unknowable the record still says WHAT is running: rebuild_base
// "empty" is the thin-aware full build (a whole-lineage copy; the engine
// publishes no byte count), an epoch name is a delta catch-up from that
// epoch. The hot-rejoin window itself is sub-2s (too fast to poll), so
// "rejoining" renders as an indeterminate pulse and the completed window
// surfaces in the event timeline after the fact. A live maintenance-drain
// lease renders as "draining": the leg is fenced out on purpose, not
// failing.

interface SyncStateIndicatorProps {
  sync: ReplicaSyncInfo | null | undefined;
  node?: string;
  compact?: boolean;
}

// "epoch-pvc-…-1264" → "#1264"; anything else verbatim.
const shortEpoch = (epoch: string) => {
  const tail = epoch.split('-').at(-1) ?? '';
  return /^\d+$/.test(tail) ? `#${tail}` : epoch;
};

// The catch-up readout for a replica that is not in sync.
function catchUpText(sync: ReplicaSyncInfo): string {
  const lag = sync.epoch_lag;
  if (lag !== null && lag !== undefined) return `${lag} epoch${lag === 1 ? '' : 's'} behind`;
  if (sync.rebuild_base === 'empty') return 'full rebuild from empty';
  return 'catching up';
}

export function SyncStateIndicator({ sync, node, compact = false }: SyncStateIndicatorProps) {
  if (!sync) return null;

  const rejoining = sync.hot_rejoin != null;
  const draining = !rejoining && sync.maint_drain != null && sync.sync_state !== 'in_sync';
  const display = rejoining ? 'rejoining' : draining ? 'draining' : sync.sync_state;
  const style = SYNC_STATE_STYLES[display] ?? SYNC_STATE_STYLES.stale;
  const lag = sync.epoch_lag;
  const showLag = !rejoining && sync.sync_state !== 'in_sync';
  const lagText = catchUpText(sync);
  const base =
    sync.rebuild_base && sync.rebuild_base !== 'empty'
      ? `delta from ${shortEpoch(sync.rebuild_base)}`
      : null;
  const valueText = base ? `${lagText} (${base})` : lagText;

  return (
    <span className="inline-flex items-center gap-1.5">
      {node && <span className="text-xs text-gray-600">{node}:</span>}
      <span
        className={`inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-xs font-medium border ${style.chip}`}
        title={sync.reason ?? style.description}
      >
        <span
          aria-hidden="true"
          className={`w-1.5 h-1.5 rounded-full ${style.dot} ${
            rejoining ? 'animate-pulse motion-reduce:animate-none' : ''
          }`}
        />
        {style.label}
      </span>
      {rejoining && !compact && (
        <span
          role="progressbar"
          aria-label={node ? `${node} hot rejoin` : 'hot rejoin'}
          aria-valuetext={`rejoin window in flight at ${sync.hot_rejoin}`}
          className="text-xs text-purple-700"
        >
          window in flight
        </span>
      )}
      {showLag && (
        <span
          role="progressbar"
          aria-label={node ? `${node} epoch catch-up` : 'epoch catch-up'}
          aria-valuemin={0}
          {...(lag !== null && lag !== undefined ? { 'aria-valuenow': lag } : {})}
          aria-valuetext={valueText}
          title={base ?? undefined}
          className="text-xs text-gray-600 tabular-nums"
        >
          {lagText}
        </span>
      )}
    </span>
  );
}

// Volume-row summary: all in sync → one green chip; otherwise one indicator
// per degraded replica, labeled by node. Volumes without sync data (single
// replica) render a neutral dash.
export function VolumeSyncSummary({ volume }: { volume: Volume }) {
  const withSync = volume.replica_statuses.filter(
    (r): r is ReplicaStatus & { sync: ReplicaSyncInfo } => r.sync != null
  );
  const first = withSync[0];
  if (!first) {
    return <span className="text-xs text-gray-400">—</span>;
  }

  const degraded = withSync.filter(isReplicaRecovering);
  if (degraded.length === 0) {
    return <SyncStateIndicator sync={first.sync} compact />;
  }

  return (
    <div className="flex flex-col gap-1">
      {degraded.map((r) => (
        <SyncStateIndicator key={r.node} sync={r.sync} node={r.node} />
      ))}
    </div>
  );
}
