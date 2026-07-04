# Flint Dashboard Improvement Plan

Status: accepted 2026-07-02. Phase 0 DONE (f97d9fe) and LIVE-VALIDATED
on cluster `runj` 2026-07-02 (images `phase0-auth.0`): before/after
captured — unauth `/api/dashboard` 200→401 (direct and via nginx),
bad login 401, admin/viewer roles enforced (viewer 403 on destructive
POST), `/healthz` open. Phase 1 DONE (9a6623c) and LIVE-VALIDATED
2026-07-02 (driver image `phase1.0`): per-tab endpoints serving and
auth-gated, projections consistent with the aggregate, cache
single-flight proven from backend logs — 12 concurrent requests
across 4 endpoints → exactly 1 node fan-out (125 ms build), repeat
burst within TTL → 0. OpenAPI types DONE 2026-07-03 (f753a8c).
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

Status (2026-07-02): frontend + backend cache DONE (9a6623c) and
live-validated on `runj` (driver image `phase1.0`, built on the
dedicated c5d.4xlarge builder node); OpenAPI codegen deferred to its
own task.

- [DONE] Adopt TanStack Query; `useDashboardData` reimplemented as a
  `useQuery` (30 s `refetchInterval`, keeps last good data, no blanking).
  `useDiskSetup` mutations now await a direct refresh + invalidate the
  `['dashboard']` query instead of guessing with `setTimeout(…, 2000)`.
  (Full per-domain hook *file* split deferred to Phase 3 component
  breakup to avoid churning the many type imports.)
- [DONE] Deleted `mockData` and the two mock-**success** fallbacks in
  disk init/delete (they reported success while doing nothing) and the
  snapshots-tab mock fixtures; all views now show honest errors and keep
  last-known-good data. (MSW dev fixtures: optional follow-up; the vite
  dev proxy already points at a real backend.)
- [DONE] Backend: short-TTL aggregate cache (`DASHBOARD_CACHE_TTL_MS`,
  default 3000 ms) with write-lock single-flight, so a burst of viewers
  collapses to one node fan-out; `/api/refresh` invalidates it. Per-tab
  projections `/api/overview` (counts only), `/api/volumes`, `/api/disks`
  serve slices of the same cached aggregate (ready for the Phase 2
  per-domain queries to adopt; the SPA still uses `/api/dashboard`, now
  cache-cheap).
- [DONE 2026-07-03 (f753a8c)] Generate TS types from the backend via
  utoipa → OpenAPI → `openapi-typescript` with a CI freshness check.
  Backend payload structs derive ToSchema and the ad-hoc `json!`
  responses became typed structs (byte-shape identical), so the spec
  is enforced by construction; `dashboard-openapi` bin emits
  `api/openapi.json` (3.1, 14 paths / 36 schemas); `npm run gen:api`
  produces `src/api/schema.d.ts`; frontend wire types are aliases with
  the deliberate literal-union narrowings as visible Omit-overrides;
  `scripts/check-api-types.sh` is the freshness gate (CI in Phase 3).
  The compiler then surfaced and we removed real drift: phantom
  `disk_ref`/`lvol_uuid`/`health_status`/`bdev_name` fields, tuple-shaped
  `failed_disks`, a topology warning that fired unconditionally, and
  the never-read Disk Setup advanced options (huge pages / driver
  override). Live-validated on runj: all 9 exercised endpoint
  responses conform to the generated schemas and contain no
  undocumented keys — the spec matches the deployed wire format.

Acceptance: no `setTimeout` refreshes (met); N browser tabs produce ~1×
node-agent fan-out per TTL, not N× (met via the cache — validate live on
the phase1 roll).

## Phase 2 — Surface the self-healing engine + operator workflows

The payoff phase: make the dashboard the tool the Tier-2 runbook
points at.

### 2a. Live replica sync-state indicator (lead deliverable)

Status (2026-07-02): DONE (564b2fe) and LIVE-VALIDATED on `runj`
(images `phase2a.0`, backend + frontend) with a live leg-kill drill
against the standing `hr-e2e` fixture. Backend projects the PV
`replica-sync-state` annotation into per-replica rows (`sync_state`,
`last_epoch`, computed `epoch_lag`, `since`, `reason`, `hot_rejoin`)
plus volume `current_epoch`; replica-set volumes derive state from the
record (previously read Unknown/0-replicas — the legacy path needs the
single `node-name` attribute they don't have). Frontend ships
`SyncStateIndicator`/`VolumeSyncSummary` on `SYNC_STATE_STYLES` tokens
(ui/status.ts), replacing all three ad-hoc rebuild renderings; polling
is adaptive (2.5 s while any replica non-in_sync, 30 s baseline).
Drill evidence (kill 22:36:53 local, spdk-tgt leg on runj-aws-2,
2 s API poll = the UI cadence): Healthy 2/2 → +23 s Degraded 1/2 with
`stale` → `stale lag=1` as epoch 1261 cut → `standby` chasing with lag
visibly oscillating 2→1→2→1 against the 30 s cut cadence → +4 m 12 s
Healthy 2/2 both `in_sync`. `HotRejoinSucceeded`: 1729 ms window,
26 MiB estimator → inline fenced-delta path. Zero counter skips across
1,941 drill-window writer appends. As designed, the sub-2 s window/
`hot_rejoin` marker was never sampled by a poll (marker span ≈ seconds
vs 2 s poll + 3 s backend cache TTL) — the "rejoining" chip state is
data-driven and unit-covered, and completed windows surface via events
(2c, next).

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

Status (2026-07-02): DONE (29f4a89) and LIVE-VALIDATED on `runj`
(images `phase2b.0`, surgical `kubectl set image` roll) with a third
leg-kill drill narrated entirely from the new surfaces. Backend:
volumes carry `consumer_raids` — one `bdev_raid_get_bdevs` per node
agent, projected per consumer (state, operational n/m, members);
presence of `raid_<pv>` on a node IS the consumer set. Configured
members are labeled with the replica node they back (same matching
rules as `replicas_missing_from_raid`: identity/live uuid, local
lvol-uuid name, deterministic remote bdev name); SPDK nulls name+uuid
on a failed leg, so a null member renders as the failed slot. Frontend:
the detail modal is LIVE (selection by id, volume re-derived from the
polled query each render; SPDK details stay a one-shot fetch keyed by
id); Replicas tab is the per-replica sync table on the 2a indicator;
RAID tab shows per-consumer assembly; a new Events tab embeds the 2c
panels via `/api/events?volume=` (the item deferred out of 2c).
Drill evidence (kill 23:37:55 local, spdk-tgt leg on runj-aws-2,
consumer on runj-aws-3, 2 s API poll): **+5 s the consumer raid showed
`online 1/2 + failed slot` — before the replica sync flipped at +22 s.
The consumer-raid projection reads SPDK ground truth on the consumer
and is the dashboard's fastest degradation signal.** Then the familiar
narrative: stale → stale lag=1 → standby with lag oscillating 1↔2
against the 30 s cuts → `stale+hot_rejoin` marker sampled live at
+3 m 07 s → Healthy 2/2 with the raid back to `online 2/2` in the same
poll (+3 m 12 s). The raid stayed **online** (serving at 1/2) through
the entire episode — the no-blind-rebuild story made visible. Window
1764 ms inline (30 MiB estimator, steps summing exactly) appeared in
the volume-filtered feed within one 10 s poll; member labels intact
after the rejoin (remote-name match is uuid-change-proof); zero
acked-write loss (writer counter monotonic through the drill).
Also fixed en route: the frontend `PvcInfo` type was hand-drifted
fiction (four fields the API never sent) — corrected to the backend
struct; the dead `raid_bdevs`/`nvmeof_subsystems` empty-states in the
old modal (fields no backend path emits) were removed.

- **Volume detail**: per-replica table — node, `sync_state`
  (in_sync/stale/standby), epoch lag vs `current_epoch`, hot-rejoin
  marker — read from the PV `replica-sync-state` annotation the
  controller already maintains. RAID state per consumer (online /
  degraded n/m). Uses the 2a indicator component per replica.
### 2c. Events + windows

Status (2026-07-02): DONE (c903f5c) and LIVE-VALIDATED on `runj`
(images `phase2c.0`) with a second leg-kill drill watched through the
new endpoint at the tab's own 10 s cadence. `/api/events`
(viewer-gated) lists the engine's PV events from the `default`
namespace (single K8s list, no node fan-out, uncached), categorized by
reason family, newest first, capped 200; `HotRejoinSucceeded` messages
parse into structured windows (node, raid, E_f, window_ms, per-step
timings, inline-vs-esnap, estimator bytes) — parser unit-tested
against the verbatim drill payload. Frontend Events tab: windows panel
(bar vs the 2 s target, path chip, estimator, step breakdown) +
category-filterable timeline, 10 s polling. Drill evidence (kill
23:02:51 local): EpochCutFailed +11 s → VolumeDegraded/ReplicaStale
+23 s → ReplicaCatchupStarted +71 s → ReplicaStandby +76 s →
HotRejoinSucceeded +2 m 13 s, each appearing in the endpoint within
one 10 s poll; windows[] grew 1→2 with the new 1720 ms inline window
(30 MiB estimator, steps summing exactly); volume back to Healthy 2/2
in_sync; zero writer counter skips (1,642 appends). Bonus: this
drill's 2 s sync poll sampled the `hot_rejoin` marker live
(`stale + marker` at 23:05:03 — the "rejoining" chip state), so every
2a indicator state has now been observed on a live cluster.

- **Event timeline**: `HotRejoin*`, `VolumeDataPath*`, catchup
  transitions, per volume and cluster-wide (K8s events already carry
  all of it, including window step timings). This is where a completed
  sub-2s rejoin window becomes visible — the 2a indicator points here.
- **Windows panel**: hot-rejoin window durations vs the 2 s target,
  inline-vs-esnap routing with estimator bytes — straight from
  `HotRejoinSucceeded` event payloads.
- ~~Deferred within 2c~~ LANDED with 2b: per-volume timeline embedding
  in the volume detail view (`/api/events?volume=`, panels shared via
  `EventPanels.tsx`).

### 2d. Operator workflows

Status (2026-07-03): DONE (bd33b8a) and LIVE-VALIDATED on `runj`
(images `phase2d.0`, surgical `kubectl set image` roll — driver image
rebuilt only because the aggregate `DashboardDisk` gained
`is_system_disk`, without which the nav badge would count root disks
forever). The batch engine is pure logic in `batchSetup.ts`
(`runInitBatch`): one setup call per disk — the agent's
`/api/disks/setup` loops per-PCI server-side anyway, so per-disk calls
cost nothing extra and buy a live per-disk status — serial within a
node (the agent mutates shared host state), capped cross-node
concurrency (6), cancel drains the remainder as `skipped`, per-node
disk-list refresh on queue drain. Verified by a 28-check simulation
(120 disks / 12 nodes: cap respected, ≤1 in-flight per node, monotonic
progress, retry-failed subset, throw containment, cancel). Selection
is the interaction: group by node or disk class with
`k uninitialized / n total` headers and group-level select,
cluster-wide + filter-driven "select all uninitialized", shift-click
ranges; anything ineligible (system / already-initialized / mounted
without Force Unmount) is excluded from the batch and listed with the
reason in the confirm modal, which shows node/device/PCI/serial/
capacity per disk and demands a typed phrase above 10 disks.
Live-validated: both real per-disk endpoint paths exercised against
runj agents (bogus PCI → contained per-disk failure with the agent's
`Disk not found` message; already-initialized PCI → idempotent
success, no reformat — `initialize_blobstore` early-returns on an
existing LVS; hr-e2e volume stayed `in_sync/in_sync` throughout);
landing inputs from live `/api/disks`: 3 initialized lvstores →
`isFreshCluster=false` → Overview, and the badge counts 1 (the
builder node's spare NVMe) instead of 5 once system disks are
excluded. No real bulk init was run: the only uninitialized
non-system disk on runj backs the dind-builder's scratch — precisely
the skip-a-disk case the selection model exists for.

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

Status (2026-07-03): DONE in six commits (789cf4c test infra, ae1f9f6
router, ed57bfa status tokens, 0257d36 strictness, bf197f8 nginx/
Dockerfile, 3c77e02 CI); code-validated by the new gate (60 tests,
0 lint errors, tsc clean under noUncheckedIndexedAccess, build OK,
both freshness halves green). Not yet live-rolled — needs a
`phase3.0` frontend image (frontend-only; no backend changes) and a
deep-link smoke test on runj.

- [DONE] Vitest + RTL + MSW: fixtures typed against the GENERATED
  OpenAPI schemas (a drifted fixture is a compile error), MSW at the
  fetch layer with unhandled-request=error. Suites: the committed form
  of the 2d batch-engine simulation, api/client session boundary,
  useDashboardData (last-known-good on failure, transform hardening,
  adaptive-poll input), 2a indicator ARIA contract, 2c panels, routes,
  Dashboard shell integration. The dead pre-Phase-0 playwright scratch
  harness (verify-disk-setup.mjs) is deleted.
- [DONE] react-router URL state: tab = path segment; cross-tab filters
  (?filter/?disk/?replicas) and modal selections (?volume/?snapshot) =
  search params, with detail params scoped to their home tab
  (routes.ts). Landing (Decision 2) now fires ONLY on the bare "/"
  entry — deep links are never hijacked. Tabs are real <Link>s.
- [DONE] status.ts is THE status vocabulary: volume states, member/
  replica states (Tier-2 chips aliased from SYNC_STATE_STYLES), and
  the filter display copy that existed in four diverging versions.
  Bonus correctness fix: the filter PREDICATE is unified too —
  filterVolumesByType is Tier-2-aware and shared, so the 'rebuilding'
  card count and the filtered views can no longer disagree about a
  stale/standby replica.
- [DONE] Zero `: any`; noUncheckedIndexedAccess ON. En route,
  NodeMetricsAPI (700 lines) was DELETED: its endpoints
  (/api/nodes/{n}/metrics, /raid) never existed in the backend — every
  production render of the "SPDK Metrics" modal was its embedded mock
  payload, the exact Decision-1 failure mode. NodeDetailView (real
  aggregate data) remains the node surface.
- [DONE] One nginx.conf: the chart's stale ConfigMap copy (pre-Phase-0,
  no gzip/security headers — and it was what production actually
  served) is removed; the image's nginx.conf is the single source from
  the next chart release. Dockerfile.frontend: node 18 → 22 (the
  second Dockerfile variant was already gone).
- [DONE] CI (repo's first workflows): dashboard-ci (lint/test/build +
  schema.d.ts⇔spec freshness, node 22) and dashboard-api-spec
  (spec⇔backend via the dashboard-openapi bin, cargo + protoc),
  path-scoped.

**Honesty debts found during Phase 3, deliberately left for a product
decision (they need features or removal, not types):**
- The snapshots tab FABRICATES storage analytics when the backend
  omits them: "mock 30% consumption", "70% actual data usage", and the
  derived efficiency/breakdown numbers in the storage view are
  invented client-side (EnhancedSnapshotsTab enhancers — now typed and
  labeled, still synthesizing).
- The entire Remote Storage tab's actions (connect/disconnect/save/
  discover) are setTimeout mocks that report success without doing
  anything. Either back it with real endpoints or remove the tab.

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
