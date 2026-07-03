# Flint Dashboard Improvement Plan

Status: DRAFT accepted 2026-07-02. Owner: dashboard workstream.
Assessment basis: `spdk-dashboard/` at commit 042b805 (~13k LOC TS/TSX,
React 19 + Vite + Tailwind 3, nginx → warp backend
`spdk-csi-driver/src/spdk_dashboard_backend_minimal.rs`, ~2.5k LOC).

## Why

The dashboard works and has had real fixes land recently, but it grew
organically and predates the Tier-1/Tier-2 self-healing engine. It has
one genuine vulnerability (no real auth), one dangerous habit
(mock-data fallback), several structural liabilities (god-hook data
layer, four 800–1600-line components, zero tests, no router), and it is
blind to the storage engine's most operationally important state
(replica sync, epochs, hot-rejoin, DataPath events).

## Current state — findings

| Finding | Evidence |
|---|---|
| No real security | Login checks `admin/spdk-admin-2025` hardcoded client-side (`App.tsx`, `useAuth`); "session" = `localStorage spdk_auth=true`; backend enforces nothing — all endpoints incl. destructive disk reset/delete and orphaned-volume deletion are unauthenticated; nginx adds `Access-Control-Allow-Origin: *` |
| God-hook data layer | `useDashboardData.ts` (1486 lines) mixes auth, polling, stats derivation, 7 mutation flows; post-op refresh via `setTimeout(…, 2000)` guesses |
| Monolithic components | `DiskSetupTab` 1610, `DisksTable` 1117, `EnhancedSnapshotsTab` 985, `EnhancedRaidTopologyChart` 818 lines |
| No tests, no router | Zero test infra; tabs are `useState` — no deep links, refresh loses context |
| Hand-drifted types | Frontend types duplicated from Rust by hand; no schema. Mock fallback has repeatedly masked real API failures (see `c20711e`, `23006e2`) |
| Build drift | Two near-duplicate Dockerfiles (node 18 vs 20, port 80 vs 3000) |
| Blind to the engine | No replica sync states, epoch lag, hot-rejoin windows/markers, VolumeDataPath events, catchup progress. Operators fall back to kubectl + ad-hoc scripts |
| Scale posture | Backend: per-request parallel fan-out (`join_all`) to all node agents, deliberately uncached — work scales as viewers × nodes; monolithic JSON payload. Frontend: tables paginated (25/page) but one fat state object re-renders wholesale every 30 s; cluster-wide reactflow topology unreadable at 100s of volumes |

## Decisions (settled)

1. **Mock data is removed from the app bundle.** Fixtures move to MSW
   (dev server + tests only). Production shows last-known-good REAL
   data with an unmissable "disconnected — data as of HH:MM:SS" banner.
   Stale truth clearly labeled beats fresh fiction; an ops dashboard
   that renders healthy fiction during an outage is worse than no
   dashboard.
2. **State-aware landing, not a static tab order.** Fresh cluster
   (zero initialized lvstores) → land on Disk Setup with an onboarding
   callout. Provisioned cluster → land on Overview. Persistent nav
   badge while any node has uninitialized disks. An operator opening
   the dashboard during an incident must hit Overview, not a wizard.
3. **Scale target: 50 nodes / 500 volumes / 100s of disks** without
   architectural change; the levers are the Phase 1 data-layer split,
   a backend short-TTL cache, and per-tab endpoints (below).
4. **Bulk disk operations are a first-class feature** (Phase 2) —
   group selection + batch orchestration, not one-checkbox-at-a-time.
5. Auth is enforced **by the backend**, not the SPA.

## Phase 0 — Security (small, urgent, independent)

- Bearer-token auth: token in a K8s Secret, warp middleware rejects
  unauthenticated `/api/*`; `/api/login` exchanges credentials for the
  token; frontend stores it in memory (not localStorage) and sends
  `Authorization` headers.
- Remove the hardcoded credentials and the "default credentials" hint
  from the login page.
- Drop the nginx CORS wildcard; same-origin only.
- Destructive endpoints (disk reset/delete, orphan deletion) behind a
  role flag (`viewer` vs `admin` token), so a read-only token can be
  handed out safely.
- Until this lands, deployment guidance: dashboard Service stays
  ClusterIP, access via port-forward only.

Acceptance: unauthenticated `curl /api/dashboard` → 401; viewer token
cannot invoke destructive endpoints; no secret material in the bundle.

## Phase 1 — Data layer

- Adopt TanStack Query. Split `useDashboardData.ts` into per-domain
  hooks: `useDashboard`, `useDiskOps`, `useMemoryDisks`,
  `useSnapshots`, `useAuth` (own file).
- Replace every `setTimeout`-based post-op refresh with query
  invalidation on mutation success.
- Generate TS types from the backend: annotate warp handlers with
  utoipa → OpenAPI spec → `openapi-typescript`. Frontend/backend drift
  becomes a compile error. CI check that the generated types are
  current.
- Delete `mockData` (Decision 1); add MSW with the same fixtures for
  dev/test.
- Backend: split `/api/dashboard` into per-tab endpoints
  (`/api/overview`, `/api/volumes`, `/api/disks`, `/api/snapshots`) so
  a 30 s Overview tick stops re-shipping the snapshot world; add a
  short-TTL (2–5 s) aggregate cache so concurrent viewers share one
  node fan-out instead of each triggering their own.

Acceptance: no `setTimeout` refreshes; only the active tab's query
polls; N browser tabs produce ~1× node-agent fan-out per TTL, not N×.

## Phase 2 — Surface the self-healing engine + operator workflows

The payoff phase: make the dashboard the tool the Tier-2 runbook
points at.

- **Volume detail**: per-replica table — node, `sync_state`
  (in_sync/stale/standby), epoch lag vs `current_epoch`, hot-rejoin
  marker — read from the PV `replica-sync-state` annotation the
  controller already maintains. RAID state per consumer (online /
  degraded n/m).
- **Event timeline**: `HotRejoin*`, `VolumeDataPath*`, catchup
  transitions, per volume and cluster-wide (K8s events already carry
  all of it, including window step timings).
- **Windows panel**: hot-rejoin window durations vs the 2 s target,
  inline-vs-esnap routing with estimator bytes — straight from
  `HotRejoinSucceeded` event payloads.
- **State-aware landing + onboarding** (Decision 2).
- **Bulk disk initialization** (Decision 4). Current UI already has
  per-disk checkbox selection and per-node `disks/setup` calls; add:
  - Grouping: by node and by disk class (model + capacity); group
    header shows `k uninitialized / n total` with a group-level
    "select all uninitialized" action; global "select all
    uninitialized (cluster)" plus filter-driven selection (by node
    pattern, by size class). Shift-click range select.
  - Batch orchestration: fan out per-node setup calls with a
    concurrency cap; per-disk status stream (pending → running → ok /
    failed); partial-failure summary with "retry failed only". The
    batch runs client-side over existing per-node endpoints first; a
    backend `/api/disks/batch_setup` becomes worthwhile when node
    counts make client fan-out chatty.
  - Safety rails: confirmation modal listing exactly what will be
    wiped (node, device path, serial, capacity), type-to-confirm above
    a threshold (e.g. >10 disks), and the batch never includes disks
    the backend doesn't report as uninitialized unless explicitly
    forced per-disk.

Acceptance: an operator can initialize 100+ uninitialized disks across
10+ nodes in one confirmed action and see per-disk outcomes; a
leg-kill drill is fully narratable from the UI (stale → standby →
window → in_sync with timings) without kubectl.

## Phase 3 — Structure and safety net (interleave with 1–2)

- Break up the four 800+ line components as they're touched; extract
  the three duplicated status-color/case switches into one
  `status.ts` module.
- react-router: URL state for tab, selected volume/node/snapshot —
  deep-linkable, refresh-safe.
- Vitest + React Testing Library + MSW; first tests ride the Phase 1
  seams (query hooks, mutation invalidation) and Phase 2 features.
- Eliminate the 33 `: any`s; enable `noUncheckedIndexedAccess`.
- Single Dockerfile (delete the node-18 variant); one nginx.conf
  source of truth.
- CI: typecheck + lint + test + build + generated-types freshness.

## Phase 4 — UX polish (after the above)

- Unified status color/badge system from the Phase 3 `status.ts`.
- Consistent loading/empty/error states (skeletons, not spinners-only;
  every empty state says what would populate it).
- Topology views scoped per-node / per-volume with level-of-detail
  instead of cluster-wide graphs (Decision 3 scale target).
- Then, and only then, visual redesign if still wanted.

## Sequencing and effort (rough)

| Phase | Size | Notes |
|---|---|---|
| 0 Security | ~1–2 days | Independent; do first |
| 1 Data layer | ~3–5 days | Enables everything else |
| 2 Engine surface + bulk ops | ~1–2 weeks | The payoff; needs 1 |
| 3 Structure/tests | ongoing | Interleave; gate new code on tests |
| 4 UX polish | ~1 week | Last |

## Related storage-side item (tracked in docs/UnansweredOn7b.md)

The 7b-4 inline-admission byte ceiling (`FLINT_HOT_REJOIN_INLINE_
DELTA_MAX_MIB`) is hardware-relative; the observed 1656 ms fenced-delta
copy was poll-quantization-bound, not media-bound. Planned evolution:
tighten in-window poll cadence, then time-based admission (estimator
converts bytes → predicted ms via measured copy rate). The dashboard
Windows panel (Phase 2) is where that measured rate becomes visible.
