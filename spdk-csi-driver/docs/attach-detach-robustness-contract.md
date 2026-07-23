# The attach/detach robustness contract

**Status: design, adversarially verified 2026-07-22** — every claim below was
attacked against the actual code by an 8-verifier review (transcript in
session fd45b0c0; verdicts: 5 PARTIALLY_REFUTED, 3 HOLDS_WITH_CORRECTIONS,
all corrections folded in here). This document is the umbrella over the
F36/F36c/F38/F39 invariant family: five rules that, together, eliminate the
failure classes an unstable Kubernetes cluster (node death/flap, kubelet
stops, pod kills, restage races, API partitions) manufactures in a CSI
driver — plus the concrete two-wave implementation plan for flint.

The rules are general (any CSI driver with a node-local data plane);
each is instantiated for flint with the shipped prior art it generalizes.

---

## The five rules

### R1 — One durable intent record, CAS-fenced, generation-before-authority

Each volume has exactly one durable intent record (desired chain topology +
monotonic generation) homed on ONE API object, mutated only through
resourceVersion CAS with idempotent, stamp-free mutators. The generation is
bumped BEFORE any authority change (granting an attach, admitting a writer,
seizing ownership), so an actor holding the prior generation fails its next
token check by construction. Intent answers only "what should exist and who
is authorized" — it is never an input to liveness decisions — and
destructive machinery fails closed on an unparseable intent (never silent
rebuild-to-initial).

*Forced by:* informer staleness across API partitions + actor restarts —
two actors resume with divergent desired states and both act (F38).
*Eliminates:* the divergent-desired-state war; there is no second desired
state to fight over.

**Flint:** ChainIntent as a *separate annotation key* co-written in
`update_sync_record`'s rv-guarded patch (multi-key precedent:
replica_replace.rs:425-435); write-intent → verify-landed → mutate per the
shipped hot-rejoin template (hot_rejoin.rs:434-450). Required deltas (C5):
lift the `replica_count<=1` early-return (replica_sync.rs:950-952) so r1 /
single-survivor chains are covered; CreateVolume seeds via volumeAttributes
+ lazy synthesis (no PV exists yet); subsume the frontend datum (ublk-id /
NQN, today on the backing PV, driver.rs:1285-1291) so the two-object split
actually dies; gating writes get backoff beyond 3 bare CAS attempts;
NodeUnstage NEVER gates on an intent write (unstage must succeed during API
outage — best-effort release only).

### R2 — One visible mutating owner per volume per layer; leases expire; seizure bumps the generation

Chain mutation at a layer is serialized under a single ownership claim that
is (a) visible/enumerable, (b) a wall-clock lease with expiry, (c) held
across the entire probe→mutate window, and (d) binding on ALL mutators at
that layer — **CSI RPC handlers and HTTP endpoints included**, not just
background loops. Seizure on expiry bumps the intent generation, so a
paused-then-resumed prior holder's destructive calls fail the token check:
correctness rests on the CAS token; clocks (with an explicit skew margin,
T_back precedent catchup.rs:510-514) affect only availability.

*Forced by:* kubelet stops / pod kills / GC pauses mid-operation — any
actor can freeze at any await and resume believing it still owns the volume
(F39). *Eliminates:* invisible-claim starvation and node-local probe/act
interleaving (today compensated post-hoc: node_agent.rs:4455-4466).

**Flint:** per-volume async lock map on the shared `Arc<SpdkCsiDriver>` —
feasible because the node agent and CSI gRPC service share one process and
one Arc (main.rs:360-373, node_agent.rs:38); replaces the controller-only,
expiry-less, node-invisible volume_claims.rs. Controller-layer claims
become leased episode fields on the record.

### R3 — One chokepoint for every op that severs OR ADMITS a writer; self-host live consumption is an absolute veto

All ops that can sever a live data path (delete / stop / detach /
fence-flip) AND all writer-admitting construction (a second serving entity
over an already-consumed backend object) pass one chokepoint whose decision
takes exactly three inputs:

1. **Live probe at the layer's native observability level** — SPDK RPCs for
   bdev/subsystem consumption; **kernel opener probes** (holders scan +
   O_EXCL, fd HELD across the destructive op) for ublk frontends — no SPDK
   RPC can see kernel openers (C1). Raid consumption = `bdev_raid_get_bdevs`
   base_bdevs_list membership matched by uuid AND alias AND name
   (controller_reap.rs:100-105 pattern) — NOT the discarded bdev `claimed`
   bool (read once and thrown away, minimal_disk_service.rs:1914).
2. **Configured-consumer authority** — VolumeAttachment plus target-side
   admission config (allowed-hosts). **Zero live connections ≠ no
   consumer**: kernel initiators reconnect autonomously for up to
   ctrl_loss_tmo (1800s). "Never stored records" is weakened to: never the
   driver's own intent/sync record for liveness; attach authority and
   target config state are REQUIRED inputs (C3).
3. **The intent generation** — which may authorize fencing/destruction ONLY
   of other-host, prior-epoch consumers. A live self-host consumer, or a
   busy/unknown frontend opener, is an **absolute veto no token can
   override** — the F38 destroyer held the current generation by
   definition (C2). Three-valued hostnqn scoping (self / other-admitted /
   other-stale via live allowed-hosts, nvmeof_export.rs:305-306): plain
   self-vs-other deadlocks the runf-eviction flow, whose fenced-out zombie
   controllers demonstrably persist live (nvmeof_export.rs:355-388).

Probe failure branches on error class: target-verifiably-missing ⇒ allow
the idempotent no-op (else NodeUnstage wedges forever after a tgt restart);
transport/unknown ⇒ defer to the ladder — never fail open. (Precedent
correction from review: F9 fails OPEN on probe error today,
node_agent.rs:1481; F37's "never reap blind" is the fail-closed precedent.)
Enforced structurally: CI grep lint (identity.rs Phase-4 pattern), all
bypasses routed or deleted — dashboard raw `bdev_lvol_delete`,
force_unstage, rehydrate reap, `delete_phantom_raid_local` (which today
fail-opens on a k8s API error: get_attached_node None-on-error → delete
fires), the `/api/spdk/rpc` passthrough. Snapshot-class deletes are
documented-exempt (SPDK clone-pinning EPERM is the intrinsic guard).
Fencing whitelists derive from the VolumeAttachment, NEVER intent.frontend
— intent says WHERE to build; attach authority says WHO may write. With
FLINT_NVMF_FENCING=disabled there is no live rightfulness signal: the
chokepoint refuses destructive automation rather than pretending safety.

### R4 — Recovery is a recorded ladder; every rung bounded; dead-device escapes are rungs; loss only ever bounded+evented; terminal = bounce the consumer

Escalation state (strike → flag → repair → degrade → terminal) lives as
episode data on the record, each rung with an entry condition, budget, and
successor. Probe-independent escapes for dead devices (DEL_DEV,
node_agent.rs:3639-3690) are rungs INSIDE the ladder — fail-closed must
never equal wedged-forever. Any sanctioned data exposure (stale-survivor
serve) is deadline-bounded, persisted, and evented as recorded loss;
intent-driven exclusion of a leg whose node is HEALTHY is refused outright,
never deferred-then-served (C2). Terminal rung bounces the consumer
workload so the platform re-drives the whole attach chain (cutover
BounceNfsPod — today default-off and starved; must be wired and enabled).

Writer-set integrity pins that make the F36c gate un-launderable (C2):
`reconcile_membership` stops defaulting new identities to InSync
(replica_sync.rs:246-257 — they enter Stale, reaching InSync only via the
fenced final-delta admission) and stops pruning writer-set members
unconditionally (262-272 — writers leave only through the replacement
flow); the gate evaluates missing writers against the RECORD's writer set
independent of intent membership (driver.rs:1904-1905 silencing hole).

### R5 — No unbounded awaits; independent stall domains; snapshot probes; cancellation = compensation

Every external wait — RPC, API call, lock acquisition, loop cycle, copy
progress — carries a deadline (spdk_native.rs call_rpc has none today:
connect/write_all/read_line all unbounded; per-method budget table needed —
`bdev_wait_for_examine` and the shallow-copy poll get longer budgets).
Fast-path health detection is O(1)-in-volumes whole-list snapshots (3-4
RPCs/tick, parity with today's 2) with ZERO API dependency in steady state
— intent from an informer cache; fail-closed API reads reserved for
destructive decisions (C5: the 10s paths run API-free today and must stay
so). **Keep the two-task cadence split** (10s detectors / 60s monitor) —
the review refuted "today's redundancy is illusory": fresh-connection-per-
call and the k8s-axis asymmetry give the split real fault isolation; the
minimalist move is per-sub-pass budgets inside the existing structure, NOT
a single-loop fold (C4). A fired timeout executes structured compensation,
never a bare future-drop (C6): detach the catch-up copy controller (else
the F36 head_live_consumer guard counts the driver's OWN copy connection as
a consumer and livelocks the replica stale forever — note the hard subcase
copy-source == consumer-node where hostnqn cannot distinguish them); unwind
unadopted replica_replace placeholders (+ a new reaper rule:
`vol_<v>_replica_<i>` whose PV exists but whose node left the replica list
— PV-absence authority can never condemn these); the watchdog writes the
hot-rejoin back-off/inline-deny entries itself (hot_rejoin.rs:2515-2532).

---

## Lean on upstream — use and feed, do not reimplement (C8)

- **kubelet's CSI retry loop IS the outer reconciler.** Convergence inside
  one idempotent attempt; the structural no-op check goes FIRST
  (wipefs-before-mounted-check saved only by the blkid signature guard,
  main.rs:2563-2620, is the anti-pattern).
- **k8s ≥1.34 volume reconstruction** closed F29 upstream; the F29 probe
  stays as defense-in-depth for out-of-band shapes but is no longer
  load-bearing.
- **Node lifecycle as death evidence:** feed the operator-applied
  `node.kubernetes.io/out-of-service` taint (and unreachable NoExecute)
  into the NodeGone detectors — the pNFS runbook already tells operators to
  apply it while no detector consumes it. Never treat a taint as an I/O
  fence.
- **VolumeAttachment is THE attach authority**; resourceVersion CAS is the
  fencing substrate — no side-channel lease service.
- **The A/D controller's 6-minute force-detach is an UNFENCED detach
  signal.** The driver owns fencing (admission flip at next stage + guarded
  unstage). Optional narrowing: flip admission at ControllerUnpublish for
  RWO block volumes (machinery exists, nvmeof_export.rs:290-345).
- Consolidate the three node-gone constants (600s replica-replace / 360s
  gate / comment-only 6-min mirror) into ONE substrate-anchored config; the
  upstream horizon is hard-coded and un-overridable (k8s #129805) — record
  the assumption once.

## Deliberately not covered

Probe→act atomicity at the target (no probe-and-destroy primitive exists;
R1+R2 shrink the window; a carried-patch delete-if-idle RPC is the optional
closer — the leased-quiesce patch is precedent). Kernel autonomy
(ctrl_loss_tmo reconnects, lazy-umount holders — probed and fenced, not
controlled). Clock truth (margins + generation-CAS, no NTP dependency).
Storage-target semantic drift (pinned by the real-cloud tier, permanently).
Physics below the replica layer (R4 bounds and events loss; cannot prevent
it). The PVC-before-VA upstream ordering gap (keeps its defensive
force-unstage).

## MUST-VERIFY-ON-REAL-SPDK (wave-2 drill entries)

1. `bdev_lvol_start_shallow_copy` to a dst that already has a copy in
   progress: EBUSY or concurrent interleave?
2. Does allowed-host REMOVAL sever an established controller connection, or
   only block new connects?
3. Does deleting an lvol reliably hot-remove its nvmf namespace?
4. Does a ublk-served lvol block `bdev_raid_create` / `nvmf_subsystem_add_ns`
   the way an nvmf write-open does? (If ublk takes no exclusive claim,
   duplicate-construction over a ublk chain may silently succeed — R3's
   creation-side check is the guard either way.)

---

## Implementation plan (two waves; S <1d, M 1-3d, L ~1wk)

### Wave 1 — v1.19.0: destroy-while-consumed + unbounded-stall core

**STATUS: IMPLEMENTED 2026-07-22 (all 8 items; 814 lib tests green, +15 new).**
Two verified deviations from the plan text, both deliberate:

- *Ready-node exclusion refusal (item 7) is deferred to wave 2.* Without an
  intent record there is no way to distinguish an intent-driven exclusion
  of a healthy leg from an availability-driven one (F33's shape is a READY
  node running a dead tgt — refusing on NodeReady would turn the validated
  3.6/2.4 recovery paths into unbounded outages). Wave 1 instead evaluates
  non-member writers as transiently missing (defer → bounded, evented
  serve-with-risk — never silence), which closes the laundering hole; the
  refusal upgrade lands with the ChainIntent record.
- *Raid-delete guarding is consumer-based, not state-based.* A blanket
  ONLINE-refusal would break the legitimate anti-zombie and phantom-hygiene
  deletes (their raids are ONLINE with no frontend). The chokepoint refuses
  on live consumers (ublk disk over the raid, export with live controllers)
  regardless of state — which also closes the latent D2 (deleting a
  CONFIGURING raid under a live frontend).
- *raid_service::delete_raid_bdev was NOT deleted* — the review called it
  unused, but driver.rs calls it at two sites through the injected HTTP
  transport (boundary-guarded on arrival). The genuinely dead path
  (`cleanup_nvmeof_target` → nonexistent route) was removed.

The F38 fix (item 6) landed structurally rather than as edits to
`drop_stale_local_exports`: the drop posts its deletes to its own node's
`/api/spdk/rpc`, so the boundary's self-host absolute veto refuses the
consumer-chain drop (the F38 D1 step) while the runf-eviction zombie
(foreign, unadmitted post-fence) still passes — the caller already treats a
refused drop as non-fatal and proceeds to the claim, which surfaces any
genuine staleness as the visible EPERM retry.

| # | Change | Effort | Closes |
|---|--------|--------|--------|
| 1 | Deadlines in `spdk_native.rs::call_rpc` + node-agent funnel; per-method budget table | S | every silent RPC hang (F39 substrate) |
| 2 | Per-sub-pass `tokio::time::timeout` in the 60s monitor + 10s detectors; keep two-task split | S | one hung pass stalling the other 8 |
| 3 | `guarded_destroy.rs` chokepoint (~400 lines, mostly relocation of F9/F37/head_live_consumer/raid-base-matcher/is_missing/allowed-hosts parsers) with the R3 decision matrix | L | destroy-while-consumed, class-level |
| 4 | Reroute ~15 unguarded destructive sites; fix `delete_phantom_raid_local` fail-open + add ONLINE-refusal; destructive-method denylist on `/api/spdk/rpc`; delete 2 dead paths | M | "all ops behind the chokepoint" true on day one |
| 5 | CI lint: destructive RPCs only from `guarded_destroy.rs` (+ documented snapshot exemptions) | S | bypass regrowth |
| 6 | F38 fix per docs/f38-reentry-export-drop.md with the C2 inversion: self-host live = absolute veto; other-host = fence-then-drop; re-entry idempotence | M | F38 class (current main still severs) |
| 7 | Sync-record laundering pins (reconcile_membership Stale-by-default; writer-set exits only via replacement; gate reads record writer set independent of membership; Ready-node exclusions refused) | M | acked-loss laundering chain |
| 8 | NodeStage ordering: mounted/no-op check structurally before wipefs | S | stage idempotency by structure |

### Wave 2 — v1.20.0: generation, locks, homing, escalation, compensation

| # | Change | Effort | Closes |
|---|--------|--------|--------|
| 9 | `flint.io/chain-gen` as separate CAS-co-written key; bump-before-attach; chokepoint re-reads before commit; r1 early-return lifted; CAS backoff | M | cross-node TOCTOU; corrupt-record generation reset |
| 10 | Per-volume node-local lock (probe→commit), binding on CSI handlers + HTTP mutations; hold time bounded by #1 | M | node-local TOCTOU |
| 11 | Annotation homing: one resolver/accessor in the record layer + lint; degraded-direct read moves to the user-PV record (retires the rc3 dual-write); frontend datum subsumed | M | F32/rc3 class structurally |
| 12 | Structured cancellation for the F39 watchdog (detach copy controller; placeholder unwind; back-off written by watchdog); drain-or-kill before re-copy | M | F39 abort-residue class |
| 13 | Placeholder-lvol reaper rule in orphan_sweep | S | abort-stranded placeholders |
| 14 | Episode persisted at flag/clear sites (strikes stay in-memory); FLINT_CUTOVER enabled; F36c marker read fail-closed (unwrap_or_default → defer) | M | ladder survives restarts; terminal can fire; defer bound can't collapse on API blip |

Deferred deliberately: C7's full planner/pure-function extraction (house
style exists in 5 modules; not needed for the four properties); the
taint-feed (cheap, closes nothing in wave scope); the server-side
delete-if-idle patch (only if live validation shows the residual window
matters).

## Test-tier mapping

Pure unit: chokepoint decision table ({live-self, live-other-admitted,
live-other-stale, configured-idle, missing, transport-error} × {token
fresh/stale}), ladder transitions, lease state machine, mutator idempotence
and generation monotonicity, compensation planner (freshness_gate.rs is the
shipped model — 12 tests). Kind tier (see project memory
`project-kind-local-tier`): CAS contention, hung-socket injection per RPC
method, handler-vs-detector interleavings, bypass lint, rung budgets. Real
cloud (final validation only): F38/F39 replays, fenced-zombie persistence,
reconnect-gap non-deletion, dead-device rung, the four
MUST-VERIFY-ON-REAL-SPDK items, 3.6-class stale-serve accounting.
