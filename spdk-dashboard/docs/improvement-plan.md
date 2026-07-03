# Flint Dashboard Improvement Plan

Status: accepted 2026-07-02. Phase 0 DONE (f97d9fe, 2026-07-02) — live
validation on a cluster still owed (needs image build + roll).
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
6. **Consistent, modern, pleasurable UI is a directive, not a nice-to-
   have** — pursued as a cross-cutting design system whose primitives
   land alongside Phase 1–3 work (see "Design system & UX quality"),
   with a deliberate visual pass in Phase 4.

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

### 2a. Live replica sync-state indicator (lead deliverable)

Motivation (finding, 2026-07-02): every rebuild control in the current
UI is bound to the field `rebuild_progress` — the *old blind
full-rebuild* model. The Tier-2 engine no longer does that rebuild. A
hot-rejoin is a sub-2-second window (last drill: 1730 ms), and the
dashboard polls at 30 s, so a real repair blinks from `stale` to
`in_sync` *between two polls* and the UI shows nothing. The existing
"rebuild" affordances (a hand-rolled `<div>` width-% bar + a Settings
gear with `animate-spin`) are cosmetic: the gear spins regardless of
progress and the bar only tweens between two poll samples. There is no
`role="progressbar"`, no indeterminate state, and — most importantly —
no representation of `sync_state` (in_sync / stale / standby), epoch
lag, or hot-rejoin windows at all. The dashboard is watching for a kind
of rebuild the engine stopped doing.

Deliverable — a proper, live per-replica state control:

- **Real signal, not `rebuild_progress`**: drive off the PV
  `replica-sync-state` annotation (`sync_state`, `last_epoch`,
  `current_epoch`, `hot_rejoin` marker) the controller already
  maintains. Per replica show a semantic status chip
  (in_sync / stale / standby / rejoining) and, for stale/standby,
  epoch lag (`current − last`) as the progress measure — lag → 0 is the
  catch-up, which IS observable, unlike the sub-2s window.
- **Live, adaptively**: while any replica of a volume is non-`in_sync`,
  refetch that volume fast (2–3 s) instead of the 30 s baseline; drop
  back to 30 s once all replicas are `in_sync`. This is the react-query
  seam from Phase 1 (`refetchInterval` as a function of the data). A
  hot-rejoin window itself stays too fast to poll — surface it *after
  the fact* from the `HotRejoinSucceeded` event (see 2c) rather than
  pretending to animate it live.
- **Proper component**: an accessible progress/status control
  (`role="progressbar"` with aria-valuenow for lag; a labeled state
  chip otherwise; an indeterminate/pulsing state for "rejoining" where
  no numeric progress exists), replacing the decorative spinner. One
  shared component reused by the Volumes table, RAID topology, and node
  detail so the three ad-hoc rebuild renderings collapse to one.
- **Consider SSE later**: if 2–3 s polling proves too coarse or too
  chatty at scale, a backend `/api/events/stream` (Server-Sent Events
  over the K8s event/annotation watch) pushes sync-state transitions;
  the indicator subscribes while a volume is degraded. Poll first,
  push only if measurement shows it's needed.

Acceptance: kill a replica leg on a live volume and watch the volume's
row go stale → standby (epoch lag counting down) → in_sync in the UI
with no manual refresh, and the completed rejoin window appears in the
event/timeline view — the same narrative the runbook describes, seen
entirely from the dashboard.

### 2b. Volume detail + topology

- **Volume detail**: per-replica table — node, `sync_state`
  (in_sync/stale/standby), epoch lag vs `current_epoch`, hot-rejoin
  marker — read from the PV `replica-sync-state` annotation the
  controller already maintains. RAID state per consumer (online /
  degraded n/m). Uses the 2a indicator component per replica.
### 2c. Events + windows

- **Event timeline**: `HotRejoin*`, `VolumeDataPath*`, catchup
  transitions, per volume and cluster-wide (K8s events already carry
  all of it, including window step timings). This is where a completed
  sub-2s rejoin window becomes visible — the 2a indicator points here.
- **Windows panel**: hot-rejoin window durations vs the 2 s target,
  inline-vs-esnap routing with estimator bytes — straight from
  `HotRejoinSucceeded` event payloads.

### 2d. Operator workflows

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

Phase 2 acceptance (whole): an operator can initialize 100+
uninitialized disks across 10+ nodes in one confirmed action and see
per-disk outcomes; and a leg-kill drill is fully narratable from the UI
— the live 2a indicator shows stale → standby (lag counting down) →
in_sync with no manual refresh, and 2c shows the completed window with
its step timings — without kubectl.

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

## Design system & UX quality (cross-cutting principle)

Goal (owner directive, 2026-07-02): the whole UI should be **consistent,
modern, and a pleasure to use** — not a bolt-on final phase. This is a
throughline: every component touched in Phases 1–3 adopts the shared
primitives below, so consistency accretes instead of being retrofitted.

Current inconsistencies (baseline to fix):
- Three ad-hoc `case 'degraded' / 'healthy' / …` color switches
  (Dashboard, VolumesTable, RaidTopology) — same states, different
  colors/labels per view.
- Hand-rolled bars and `animate-spin` gears/dots as the only loading and
  progress affordances; spinner-only loads with no skeletons; empty
  states that don't say what would fill them; error states that vary by
  component (some silent, some red boxes).
- Decorative animation not bound to data (the rebuild gear spins
  regardless of progress) — motion should mean something.
- One 922 KB JS bundle, no code splitting — first paint is heavier than
  it needs to be.
- Login page and main app use different visual languages.

Deliverables (a small, real design system — not a rewrite):
- **Design tokens** in Tailwind config: a semantic palette
  (`healthy`/`degraded`/`failed`/`rebuilding`/`stale`/`standby`/
  `in_sync`), spacing/radius/shadow scale, one type ramp. Every color
  comes from a token; no raw `text-orange-600` scattered in JSX.
- **A shared primitive kit** (`src/components/ui/`): `StatusChip`,
  `ProgressBar` (the accessible 2a control), `Card`, `Table` shell with
  built-in pagination/sort/empty/loading, `Skeleton`, `Toast`/inline
  error, `ConfirmModal` (reused by bulk disk ops), `Button`/`IconButton`
  with consistent sizes and focus rings. Built on the `status.ts` from
  Phase 3 so semantics and colors share one source.
- **Consistent state contracts**: every data view renders one of
  loading (skeleton) / empty (with a "what would populate this" hint) /
  error (inline, actionable) / data — via a small `<AsyncView>` wrapper
  over the react-query state, so no view invents its own.
- **Motion with meaning**: transitions only where they convey change
  (state transitions, value updates); remove purely decorative spinners
  once real state exists. Respect `prefers-reduced-motion`.
- **Accessibility as table stakes**: keyboard nav, focus-visible rings,
  ARIA on the status/progress controls, adequate contrast — a modern UI
  is an accessible one.

## Phase 4 — Visual system rollout + polish

By here the primitives exist and each touched component already uses
them; Phase 4 finishes the sweep and does the deliberate visual pass.

- Migrate any not-yet-touched views onto the primitive kit and tokens;
  delete the last ad-hoc color switch and hand-rolled bar/spinner.
- Code-split by route/tab (react-router lazy) + `manualChunks` for
  reactflow/recharts so first paint drops well under the current
  single 922 KB bundle.
- Topology views scoped per-node / per-volume with level-of-detail
  instead of cluster-wide graphs (Decision 3 scale target).
- Unify the login page with the app shell; consistent header, nav, and
  density across tabs.
- A deliberate visual pass on layout, hierarchy, and whitespace — the
  "pleasure to use" polish — now cheap because structure and tokens are
  already in place.
- Optional: a quick screenshot/interaction review against a couple of
  reference dashboards to sanity-check that it reads as modern.

## Sequencing and effort (rough)

| Phase | Size | Notes |
|---|---|---|
| 0 Security | ~1–2 days | Independent; do first |
| 1 Data layer | ~3–5 days | Enables everything else |
| 2a Live sync-state indicator | ~3–4 days | Lead deliverable; needs the Phase 1 react-query seam |
| 2b–2d Volume detail / events / bulk ops | ~1–2 weeks | The payoff; needs 1 |
| Design system | cross-cutting | Primitives land with Phase 1–3 work, not after |
| 3 Structure/tests | ongoing | Interleave; gate new code on tests |
| 4 Visual rollout + polish | ~1 week | Finish the sweep + deliberate visual pass |

## Related storage-side item (tracked in docs/UnansweredOn7b.md)

The 7b-4 inline-admission byte ceiling (`FLINT_HOT_REJOIN_INLINE_
DELTA_MAX_MIB`) is hardware-relative; the observed 1656 ms fenced-delta
copy was poll-quantization-bound, not media-bound. Planned evolution:
tighten in-window poll cadence, then time-based admission (estimator
converts bytes → predicted ms via measured copy rate). The dashboard
Windows panel (Phase 2) is where that measured rate becomes visible.
