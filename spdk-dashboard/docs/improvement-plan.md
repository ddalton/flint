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
both freshness halves green). LIVE-VALIDATED on runj 2026-07-04
(frontend-only image `phase3.0`, digest 4a7f4d75…, built on a
transient spot builder and rolled surgically — backend stays 1.4.0):
11/11 browser checks through the real nginx + backend — deep link
`/volumes?filter=degraded` survives the auth gate with URL intact and
lands on the filtered Volumes tab; tabs are real links with the filter
riding the href across tab switches; unknown paths bounce to the
landing entry; opening a volume detail writes `?volume=` and the modal
deep link reopens after a full reload + re-login (in-memory token is
by design); `/events` by path serves the 2c panels; zero page errors.
The image's own nginx.conf serves these routes today only via the
chart ConfigMap copy still mounted from the 1.4.0 install — identical
`try_files` semantics; the single-source config takes over at the
next chart release.

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

**Honesty debts found during Phase 3 — RESOLVED 2026-07-04 (owner
decision: remove dead code):**
- Remote Storage tab REMOVED outright: the backend has no
  remote-storage routes at all; every action was a setTimeout mock
  reporting success. Old /remote-storage deep links bounce to the
  landing entry like any unknown path. If real NVMe-oF/iSCSI target
  management ever lands backend-side, the tab returns with it.
- Snapshot analytics investigation split the debt in two: the TREE
  endpoint's storage_analytics/storage_info are REAL (computed
  backend-side from SPDK bdev consumption, with honest
  data-unavailable recommendations) — the storage view renders those
  and stays. The client-side fabrication fallbacks ("mock 30%
  consumption" on list snapshots — for a field the backend never
  sends — and the "70% actual data" tree fallback for a field the
  backend always sends) are deleted, along with the now-unused
  storage_consumption field and the identity tree enhancer.

Removal LIVE-VALIDATED on runj 2026-07-04 (image `phase4.1`, digest
11c1f642…): Remote Storage gone from the nav (7 tabs), stale
/remote-storage deep links bounce home, snapshots tab serves the
backend's real analytics (its own "estimated volume size" honesty
note visible in recommendations), zero page errors. Same builder
session ran the FIRST live end-to-end bulk init (Step 0 of the
builder recipe, scripts/bulk-init-drill.mjs, 8/8): group-scoped
select on the pristine scratch NVMe → ConfirmModal manifest → confirm
→ agent initialize_blobstore → per-disk ok → blobstore_initialized
verified via the agent — the last untested hop of the 2d flow is
closed. New finding from the hand-back: the node agent returned 404
for /disks/delete — the UI's "Delete SPDK Disk" button called an
endpoint the agent did not implement. BOTH follow-ups FIXED 2026-07-04
(956f2f8): the agent now implements /api/disks/delete (strict inverse
of initialize; refuses with 409 while lvols exist; typed + added to
the OpenAPI spec, which had also been missing the proxy), and epoch
snapshots resolve to their real volume in the snapshot tree (the
shared name parser learned the epoch-<pv>-<seq> convention; the tree
builder re-derives ids for older agents). Delivery: the tree fix
activates at the next dashboard-backend roll, the delete endpoint at
the next node-DS roll — both ride the next release image together
with the phase-4 admission probe (6f798c3). Until then the Delete
button keeps failing honestly, and future drill hand-backs can use
the endpoint instead of the spdk-tgt bounce.

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

Status (2026-07-04): core items DONE in four commits (14d8cd5
code-split — entry chunk 1013→296 KB, recharts/reactflow as on-demand
vendor chunks; d0d9fbc primitive kit + semantic Tailwind status
palette; ad89e2c view migration — the three destructive flows share
ConfirmModal, every hand-rolled width-% div is the accessible
ProgressBar, decorative rebuild-gear spin removed, boot/tab/detail
loading is skeletons, EventsTab on AsyncView; b552660 login unified
with the app shell + the dead usingMockData plumbing removed).
Topology scoping (Decision 3): already satisfied as-built — the RAID
topology renders one selected volume, not a cluster-wide graph; no
change needed. Remaining deliberately open: the full Button/IconButton
sweep across legacy views (primitives exist; adopt as views are
touched) and the two honesty debts below.

LIVE-VALIDATED on runj 2026-07-04 (frontend-only image `phase4.0`,
digest 22630817…, spot-builder build + surgical roll): 13/13 checks —
full tab tour under the code-split build (every chunk loads, zero
page errors), the unified login serving the shell chrome, the lazy
volume-detail modal opening live against r3-e2e (Healthy, 3/3 in
sync, epoch #645) with its `?volume=` deep link reopening after
reload+login, and all Phase 3 URL-state behaviors intact. Screenshot
review: login/overview/detail read as one visual language; recharts
verifiably arrives via its on-demand vendor chunk.

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

## v1.5.0 RELEASED (2026-07-04) — the dashboard release ships

Chart `flint-csi-driver-chart:1.5.0` pushed with `flint-driver:1.5.0`,
`spdk-tgt:1.5.0` (digest promotion of 1.4.0 — docker/ inputs
unchanged) and `spdk-dashboard-frontend:1.5.0`; aliases 1.5/1/latest
on all three. runj fully rolled including the node DS; the DS roll
rode over two live writers with zero restarts and every leg back
in_sync ("admitted at reassembly after fenced final delta"). The
dashboard deployment now runs the image's own nginx.conf — the chart
ConfigMap mount was dropped in the same rollout.

**The release checks earned their keep — P1 found and fixed.** The
delete-endpoint live check (expect 409 on a disk with lvols) instead
returned 200 no-op and DELETED the LVS under r3-e2e's third leg:
`count_lvols_in_lvs` matched `driver_specific.lvol.lvol_store_name`,
a field SPDK's bdev_get_bdevs does not emit, so live lvol counts were
always 0 — the dashboard disk table's lvol column included — and the
delete guard could never fire (the unit tests' fixtures had encoded
the same wrong field name). No acked data was lost: r3-e2e ran
Degraded on 2/3 legs; the disk was re-initialized through the agent
and the full Tier-2 chain rebuilt the leg (catch-up → standby →
fenced-delta admission at restage, after a writer bounce — catch-up's
hygiene guard rightly refuses while the volume's own consumer raid is
assembled on the replica node, an operator-bounce case). FIXED
9bf5674: the counter matches `lvol_store_uuid` + the `<lvs>/<name>`
alias, and delete_blobstore re-counts against fresh SPDK state
immediately before the destructive RPC; regression tests use the live
v26.05 JSON shape. Re-verified live: disks report real lvol counts
(14/14/3) and the delete on a populated disk returns the agent's 409.

Gate: kuttl 8/8 + clean-shutdown PASS on the release digests (first
run 7/8 — ephemeral-inline schedules node-locally and landed on the
LVS-less builder node; the builder is now cordoned during gate runs,
runbook updated).

Small items now tracked: ~~the dashboard delete proxy wraps the agent's
409 + refusal message as a generic 502 "Node agent returned: 409
Conflict" — pass the agent's status and body through so the UI shows
WHY the delete was refused (next backend pass)~~ FIXED
(`agent_error_passthrough` relays the agent's status code and JSON body
verbatim); tests/system Makefile single-test targets ran kuttl without
--config (fixed).

## Snapshot timeline redesign (2026-07-04, commit 61bb80b)

The Snapshots tab's "Topology View" was replaced by a real per-volume
**Snapshot Timeline**. The old view was structurally dead: it plotted
`creation_time` that the node agent stamps with `Utc::now()` on every
list call (SPDK lvols store no creation time — the `snapshot_service.rs`
TODO), and it grouped by `replica_bdev_details`, a field `/api/snapshots`
never populates — so it always rendered its empty state ("0 Replica
Snapshots" in the header chips was the same bug). Verified live on runk
against a 2-replica volume with 3 user snapshots + 6 retained epochs
before the rewrite.

**Honest data, not new pixels.** `GET /api/snapshots/timeline?volume=`
merges three sources that each carry REAL times or none at all:

| source | contributes | why trustworthy |
|---|---|---|
| VolumeSnapshotContent CRs | user snapshots: name, ns, readiness, **status.creationTime** | `status.snapshotHandle` IS the SPDK lvol name (join key); creationTime is the CSI cut time |
| PV `replica-sync-state` annotation | epochs with **EpochEntry.recorded_at**, current epoch, per-replica sync | the engine's own retained-window record |
| SPDK node fan-out | which nodes hold each snapshot's copies | live bdev truth |

Orphans (SPDK `snap_*` with no CR) are listed time-less and are never
plotted at a fabricated position; epoch stragglers outside the retained
window are counted (`untracked_epochs`), not drawn. Unit tests pin the
merge with live-shape fixtures (runk annotation + VSC nanos stamp).

**Design** (adapted from production timeline idioms — Honeycomb markers,
Grafana annotations, Elastic APM deployment lines, GitHub's density
strip, map cluster markers): two lanes with different encodings — user
snapshots as violet diamond flag markers with oversized hit targets and
+N cluster chips on collision; engine epochs as a blue bucketed density
ribbon (overlap impossible by construction, O(buckets) at any epoch
count). Absolute wall-clock ticks; green "now" pulse anchors the right
edge; hover = read-only crosshair; **click pins a popover** that holds
metadata and the actions — never buttons in hover-only tooltips.

**User-snapshot delete (admin)**: `DELETE /api/volumesnapshots/{ns}/{name}`
deletes the **VolumeSnapshot CR** (driver-guarded, 409 on foreign
drivers) so the snapshot-controller retires content + SPDK copies per
deletionPolicy — the legacy SPDK-direct route (`DELETE /api/snapshots/{id}`,
still present, still UI-unwired) would silently orphan the CR. RBAC:
`volumesnapshots` verbs gained `delete` (chart + live runk ClusterRole).
Geometry (domain/ticks/buckets/clusters) lives in `timelineLayout.ts`,
pure and unit-tested. Suites: 579 Rust / 89 vitest.

Follow-up candidates: ~~SnapshotDetailModal's disabled Delete stub should
either wire the CR path (needs the VSC join there too) or be dropped~~
DROPPED (deliberate: deletes live in the timeline's CR path; the modal is
SPDK-level and carries no CR reference); brush-to-zoom context strip
(focus+context) once volumes carry hours of history; ~~`/api/snapshots`
still double-counts per node in the header chips~~ FIXED 2026-07-05
(see below).

## Snapshot chip honesty + fleet-scale nodes tab (2026-07-05)

**Flat `/api/snapshots` merged per-node copies into logical snapshots.**
The endpoint used to concatenate every node agent's list, so a
2-replica snapshot was two rows ("Total Snapshots" chip double-counted)
and `replica_bdev_details` was never sent (the "Replica Snapshots" chip
summed a phantom field — permanently 0). `get_all_snapshots` now merges
rows by snapshot name (the cross-node join key, same rule as the
timeline's `collect_spdk_snapshots`) into one `DashboardSnapshot` per
logical snapshot with one `replica_bdev_details` entry per node copy.
The response is a typed contract (`Vec<DashboardSnapshot>` in the
OpenAPI spec, replacing the "untyped passthrough"); the snapshots tab
aliases the generated type, and the compiler flushed more dead drift:
the clone-lineage "relationship enhancer" ran on a field
(`clone_source_snapshot_id`) the endpoint has never sent — deleted.
Chips are pinned by a component test against typed MSW fixtures; merge
logic unit-tested in Rust (name-keyed, node-sorted, serde-default
tolerant of older agents).

**Ghost snapshots flagged in the timeline** (same day): a user snapshot
whose SPDK copies were all deleted out-of-band leaves a VolumeSnapshot
CR that still claims ready while restore would fail — and the flat list
can't even show it (no copies ⇒ no row). The timeline, being CR-driven,
is the one surface that still renders it: ghosts (user event, not
orphan, zero holding nodes) now draw as a hollow red diamond with a "!"
badge, the legend counts "N without copies", and the pinned popover
says plainly that the data is gone and the CR delete is the clean-up
path. DECIDED against the broader "n/m copies vs volume.replicas"
under-replication chip: current replica count is the wrong baseline
(legs rebuilt after disk loss don't recreate historical snapshot lvols;
replica-count changes shift the target; epochs flap mid-rotation) — an
honest n/m needs copies-at-cut recorded engine-side at snapshot time,
tracked as a future engine item.

**Nodes tab rebuilt fleet-scale** (owner decision 2026-07-05: full
redesign over row-compaction — the browse-a-paginated-list model was
already wrong at the 50-node target's edge):
- Backend `GET /api/nodes` (viewer-gated): per-node rollup from the
  same cached aggregate as the other Phase-1 projections — disk/volume/
  local-NVMe counts, out-of-sync replica count, capacity, and a
  `health` verdict (critical = unhealthy disk or failed volume/replica;
  warning = degraded volume or out-of-sync replica; uninitialized
  disks are deliberately onboarding work, not a health condition).
  Rollup is a pure function with unit tests; payload grows with node
  count only.
- `NodesFleetView` replaces `FilteredNodesView` (deleted): health facet
  chips with counts (All/Critical/Warning/Ready/Uninit. disks), a
  status-cell heatmap (one cell per node, click = drill-in + scroll),
  and a problems-first list of one-line node rows. Rows are
  `content-visibility: auto` so offscreen rows cost no layout/paint —
  no pagination. Search still matches disk model/PCI/volume names via
  the aggregate. Volume-filter context from other tabs is preserved
  (banner + per-row match counts).
- Drill-in: a row expands to the full per-disk/per-volume detail
  (aggregate-fed `NodeDetailView`, now a collapsible card whose header
  is the projection row), URL-synced as `?node=` scoped to the nodes
  tab like the other detail params. En route the detail table's
  allocation bar was fixed — it divided GB by bytes and always read
  ~0%.
- `status.ts` gained the node-health vocabulary (`NODE_HEALTH_STYLES`);
  the heatmap, chips, and rows all render from it.

Still open from the design-system section: the `Button`/`IconButton`
primitive was never actually built (no such component exists; ~22 files
still use raw `<button>`) — the as-touched sweep policy stands, but the
primitive itself is missing.

Disks tab scale posture (assessed 2026-07-05): filtering is already
exception-first (health/LVS/node/utilization/capacity facets + search +
sort) and Disk Setup got the bulk/fleet treatment in 2d, but
`DisksTable` renders ALL filtered rows — no pagination or windowing.
Fine at the 100s-of-disks target, degrades near ~1000 rows; candidates
when needed: row windowing like the nodes list, and adopting the
existing `/api/disks` projection.

## LIVE-VALIDATED end-to-end on AWS (2026-07-05, cluster `runl`)

All of the above was exercised on a real cluster — trove-provisioned
on-demand CP + 3× i4i.large **spot** workers with NVMe instance storage,
Flint 1.7.0, dashboard rolled to working-tree images `snapfleet.*`
(built on a c5d.4xlarge spot builder). Findings and fixes from the run:

- **Build gate (real finding, fixed):** the driver image's pinned rustc
  1.92 overflowed the trait-solver recursion limit (E0275) on the warp
  route chain once `/api/nodes` pushed it past ~20 `.or()` filters;
  local rustc 1.96 compiled it silently. Added `#![recursion_limit =
  "256"]` to the lib and `csi-driver` bin crate roots; verified against
  a locally-installed 1.92 toolchain. Without this the release build
  would have broken.
- **`/api/nodes` proved new:** 404 on the stock 1.7.0 backend, 200 after
  the roll — the projection is unambiguously the new code.
- **Fleet view (9/9 browser checks):** facet counts from the live
  projection (All·4 / Ready·4 / Uninit·3 / Critical·0), one heatmap cell
  per node, facet filter narrowing, cell-click drill-in writing `?node=`,
  and the deep link surviving reload + re-login.
- **Snapshot chips (proven at API and UI):** a 3-replica volume with
  three VolumeSnapshots returned **3 logical rows / 9 node copies** from
  `/api/snapshots` (pre-fix: 9 rows); the tab rendered Total 3 / Replica
  9 / Ready 3 — the exact bug pair (double-count + phantom-0) both gone.
- **Ghost drill (real-world path):** deleted all three SPDK copies of one
  snapshot out-of-band via the node agents, leaving the CR `readyToUse:
  true`. The flat `/api/snapshots` dropped the row entirely (0 copies ⇒
  no row — confirming why the flat list *can't* surface ghosts), while
  the timeline flagged `nodes=0 orphan=false ready=true` and the UI drew
  the red flag; the popover showed "No SPDK copies exist… restore will
  fail" with the CR-delete remedy enabled.
- **New finding + fix (cluster-hidden ghost):** three snapshots cut
  within 7s collapsed into one `+N` cluster marker, and the collapsed
  marker gave **no** ghost signal — the red flag only showed in the
  legend, and the drill-through popover. That is the Decision-1 failure
  mode (a ghost reading as healthy on a lane scan). Fixed: a cluster
  containing ≥1 ghost now draws a red ring + corner dot and its
  aria-label reads "N user snapshots (M without copies)"; unit test added
  (identical-timestamp burst). Re-rolled as `snapfleet.1`.

## UI consistency wave: type ramp, fleet-scale disk views, Button primitive (2026-07-06)

User-reported: font sizes drifted between views, and the per-disk
initialization display could not scale to 100s of disks. A code audit
confirmed both; this wave fixes them plus what it touched on the way.
Verified against a 6-node / 590-disk mock backend (fixtures amplified,
playwright screenshots) — build + 130 vitest + lint all green.

- **Typography ramp (new, tailwind.config.js):** semantic `fontSize`
  tokens — `text-page-title` (24px/700), `text-section` (18px/600),
  `text-stat` (24px/700). The stat-tile number previously rendered at
  three sizes (30px DisksTable / 24px StatCards / 20px DiskSetupTab +
  Snapshots); all tiles now use `text-stat`. `Card`'s title had no size
  class (16px, smaller than the ad-hoc 18px headers beside it) — now
  `text-section`, and all hand-rolled section/modal headers are
  tokenized to it. DisksTable's tile labels dropped from 18px-semibold
  to the label convention every other tile uses.
- **Disk Setup at fleet scale:** dense rows are the default view (cards
  remain an option; the dead `'table'` ViewMode is gone). Each node
  group header gained a **status strip** — one clickable cell per disk
  (free/driver-bound/LVS-ready/needs-unmount/system, semantic palette),
  so a 250-disk node reads at a glance even collapsed and cells toggle
  selection. Grouped bodies render at most 60 rows until "Show all N"
  (`GROUP_RENDER_CAP`), groups get `content-visibility: auto`, and a
  fleet >80 disks lands with groups collapsed (decided once, after all
  node agents have reported). Shift-click range order now mirrors
  exactly what is rendered (collapsed/capped groups contribute
  nothing). Full-page height on the 590-disk mock: 17,426px → 1,918px.
  The scale claim in the info panel is now true as written.
- **DisksTable pagination:** 25/50/100 per page with the VolumesTable
  pager contract — the unbounded-rows item from the 2026-07-05
  assessment is closed. Also fixed en route: "Total Free Space" insight
  divided GB by 1024³ (always ~0GB), and the orphan-delete flow's
  `window.location.reload()` is now a `['dashboard']` query
  invalidation.
- **Button/IconButton primitive (built at last):**
  `ui/Button.tsx` — variants primary/secondary/danger/ghost/link, sm/md,
  icon + spinner support, shared focus ring; `IconButton` requires an
  aria-label. ~50 raw `<button>`s migrated across 14 files (heaviest:
  DiskSetupTab, VolumesTable, DisksTable, NodeDetailView). Deliberate
  carve-outs left raw: segmented controls, aria-pressed facet chips,
  tree expanders, custom pager number windows.
- **Overview charts to the topology/timeline quality bar:** the volume
  pie is replaced by a horizontal stacked status bar (status hexes from
  `status.ts`, 2px surface gaps, labeled count chips — a pie hides
  close values); DiskStatusChart lost its hardcoded hexes (imports the
  `status.ts` tokens), dashed grid → solid hairline, recessive axes,
  white segment strokes, `maxBarSize`. Both charts now render in the
  `Card` shell. Palette CVD-validated (worst adjacent pair ΔE 32.7).
- **Honesty fix:** Disk Setup's delete dialog had "Industry Best
  Practice Options" checkboxes (migrate/snapshot/force) that were never
  read nor sent — the agent's `/disks/delete` takes only node+PCI and
  409s while lvols exist. Replaced with the kit `ConfirmModal` (typed
  device-name gate kept) describing only what actually happens. Debug
  `console.log`s and dead commented code in DiskSetupTab removed.

Still open from the design-system section: raw pills vs `Chip` in
legacy views (81 raw `rounded-full` spans remain, as-touched policy),
and raw palette classes vs semantic tokens (~31:1) including inside
`status.ts` itself.

### LIVE-VALIDATED on AWS cluster `runm` (2026-07-06)

The wave was validated end-to-end on a trove-provisioned cluster (i4i.large
on-demand CP + 3× i4i.large spot workers, us-west-1, flint chart 1.8.0
SPDK mode), driving the working-tree frontend via vite dev + port-forward
against the real backend. Zero page errors across every tab.

- **Fresh-cluster landing → new Disk Setup UI → real bulk init:** bare `/`
  auto-landed on Disk Setup with onboarding; per-node status strips showed
  the real disks (436GB scratch NVMe free + EBS system disk red-celled);
  "Select all uninitialized (cluster)" → Initialize 4 disks → kit
  ConfirmModal → batch panel "4 ok"; `/api/nodes` confirmed
  disks_uninitialized 4→0. Tiles flipped Free 4→0, LVS Ready 0→4; strip
  cells turned green.
- **Fleet nodes tab / overview charts / tables:** all rendered from live
  data in the new Card shells; volumes provisioned on the freshly-initialized
  disks (2 PVCs Bound + writers); stacked status bar + chips correct.
- **Timeline validated on a 2-replica volume** (violet diamonds at real CR
  times, in_sync replica chips, honest copy counts 2×1 + 2×2 = 6).
- **NEW BACKEND FINDING (open, not this wave):** on the legacy
  single-replica path the CSI `snapshotHandle` is the snapshot **UUID**,
  while the replica-set path returns the SPDK lvol **name** — the timeline
  join (`snapshotHandle == lvol name`) therefore fails for single-replica
  volumes: their user snapshots come back `orphan: true` with no timestamp
  and the lanes stay empty (the flat list still joins by name and shows
  them). Same name-vs-uuid class as the replica-drill P1s; fix server-side
  (mint the lvol name as the handle on the single-replica path too, per the
  identity contract).
- Dev ergonomics: `vite.config.ts` proxy target is now overridable via
  `VITE_API_PROXY_TARGET` (local :8080 was taken by trove during the run).
