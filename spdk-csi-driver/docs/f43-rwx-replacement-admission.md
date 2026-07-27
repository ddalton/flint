# F43 — RWX multi-replica re-placement never restores redundancy (claim starvation)

**Status:** FIX IMPLEMENTED 2026-07-26 (items #1, #7, #8 — unit-tested, 850
tests green; see "Implementation record" below for the pinned decisions and
site corrections). Live acceptance (drill #5, the permanent leg-node-kill
variant) still pending — that is the gate for calling F43 CLOSED. Found live
on runad 2026-07-23 (RWX `flint-r2`, numReplicas=2). Not a regression — a
pre-existing gap the attach/detach contract already earmarked (see "Why
deferred"). No data-loss component: the volume serves correctly **degraded**
throughout.

**Scope of impact:** RWX (NFS) volumes at **numReplicas ≥ 2** only. RWO of
any replica count is unaffected (validated: drill 2.5, F41/F42 PASS,
zero loss). The chart default is `numReplicas: "1"`, so this is an opt-in
config.

---

## Symptom

Terminate a backing-raid leg node of an RWX numReplicas=2 volume under active
write load. Observed on runad (rc6):

1. **F42 holds** — `fast_io_fail` faults the dead leg in ~20s; the backing
   raid goes `online 1/2`; I/O never stalls (ledger flowed continuously).
2. **F40 dispatch holds** — replace fires for the RWX volume (the runac
   `is_rwx` skip is gone); a new leg is placed on a healthy node and
   catch-up converges it to `standby` (lag ≤ max_lag).
3. **Admission never happens** — the standby **parks forever**. The backing
   raid stays `1/2`; redundancy never restores. (>15 min live, no progress.)

## Root cause — cutover is claim-starved by catch-up

RWX standby admission is owned by **cutover** (`plan_cutover` →
`BounceNfsPod`), not hot-rejoin: `plan_hot_rejoin` returns
`Wait("RWX volume — the Tier-1 NFS bounce owns reassembly")` by design. With
a converged standby and a pvc-backed NFS pod, `plan_cutover` *would* bounce
the NFS server → restage → `admit_standbys_at_stage` admits the standby.

But cutover never runs. `src/volume_claims.rs` is a **process-global,
expiry-less, priority-less** exclusive mutex ("at most one long-running op
per volume, whoever claims first"). The controller log shows, every tick:

```
[CLAIMS] volume claimed by another operation — skipping this tick
         wanted_op=cutover held_by=catch-up held_secs=0
```

Catch-up cannot simply stop: the **epoch scheduler advances on a 30s timer**
(writes-independent — confirmed by pausing pg-load: epochs kept advancing
22→24 with zero app writes). Each new epoch drops the converged standby back
to lag=1, so catch-up re-acquires the claim to re-chase it, indefinitely.
Catch-up (the maintenance loop) permanently out-races cutover (the resolution
loop). This is a **fairness** failure, not a wedge — `held_secs=0` each time.

## The fix is R2, NOT quiesce

- **Not quiesce.** Routing RWX through hot-rejoin's `bdev_raid_add_base_bdev
  --skip-rebuild` would work technically (the backing raid is an ordinary
  raid1) but **contradicts the documented design**: Tier-2 "Option B"
  (`docs/UnansweredOn7b.md`, 2026-07-01) deliberately confines the
  correctness-critical skip-rebuild SPDK patch to RWO — the one class with no
  other non-disruptive admission path — because a wrong `skip_rebuild`
  admission corrupts silently. RWX has the (near-transparent) NFS bounce, so
  expanding the patch's blast radius to RWX buys transparency at the cost of
  the exact risk Option B rejected. R4's terminal rung *is* `BounceNfsPod`.

- **The fix is R2's controller-claim replacement.** The attach/detach
  contract (`docs/attach-detach-robustness-contract.md`) already prescribes
  it:
  - **R2** ("leases expire; seizure bumps the generation") — *Eliminates:
    invisible-claim starvation*; **Flint:** "replaces the controller-only,
    expiry-less, node-invisible `volume_claims.rs`. Controller-layer claims
    become leased episode fields on the record."
  - **R4** — "cutover BounceNfsPod — today default-off and **starved**; must
    be wired and enabled."

  Concretely: give the controller claim (a) a wall-clock lease with expiry
  and (b) **arbitration** so the *resolver* (cutover, for a converged
  standby) preempts the *maintainer* (catch-up). Rationale: admitting the
  standby *resolves* the degraded state (standby→in_sync, catch-up then has
  nothing to do); catch-up only maintains the status quo — so the resolver
  should win. Lease-expiry alone is insufficient here (catch-up is an
  *active* re-claimer, not a paused holder): an explicit priority rule is
  required. Keep hot-rejoin RWO-only.

## Why this was deferred (and why it surfaced only now)

1. **R2 was half-shipped.** Wave 2 delivered R2's node-local lock
   (`node_volume_locks.rs`, item #10, a TOCTOU *correctness* fix) and the F39
   *visibility* fix (`log_claim_skip` + acquisition timestamp — "make
   starvation observable"). The controller-claim **expiry + arbitration** was
   left as a v1.20.0 item with no wave-2 table entry.
2. **Availability, not correctness.** A starved cutover = degraded-but-serving,
   **zero data loss**. The correctness-first campaign (destroy-while-consumed
   R3, acked-loss laundering R4) out-prioritized it.
3. **Unreachable until wave 2.** `FLINT_CUTOVER` was default-OFF before wave
   2 — with no cutover running, there was no contender to starve. Wave 2
   enabled cutover but didn't add the arbitration to let it win.
4. **The triggering drill variant was never run.** (Corrected 2026-07-26:
   the campaign DID run 3.6-family drills at RWX numReplicas=2 — runz runs
   1-3, 3.6c on runaa at `SC=flint-r2 MODE=RWX WITNESS=1`, 3.6d on runab —
   but every one was a TRANSIENT kill (kubelet stop; the node returned or
   the server restaged elsewhere), so admission always happened at the
   natural restage and cutover starvation was structurally unreachable.
   The never-run cell is the **permanent-termination / re-placement**
   variant: terminate a backing-LEG node for good while the NFS server's
   node stays up — no natural restage ever comes, so only cutover can
   admit the replacement. runad is the first cluster to run that variant.)

## Acceptance drill (add to the matrix as 3.6/r2 — RWX re-placement)

RWX `flint-r2`, WITNESS=1, continuous write load. Terminate a backing-raid
leg node (not the NFS server's node); delete its Node object (trove has no
cloud-controller node GC). Expect the full autonomous chain:
`fast_io_fail fault → 1/2 → stale-mark → replace → catch-up → standby →
cutover BounceNfsPod → restage admit → 2/2`, with **zero acked loss** (oracle
`acked ⊆ ledger`). Today it stops at `standby`.

## What already works (do not re-litigate)

- **RWO F41/F42** — drill 2.5 PASS: dead-leg fault ~20s, I/O never stalls,
  replace→catch-up→hot-rejoin→2/2, DB-VERDICT PASS (4466 acked, zero loss).
- **F40 dispatch for RWX** — replace fires for RWX (runac's `is_rwx` skip
  removed); the new leg is placed and converges. Only the *admission* step is
  gated.
- **F42 for RWX** — the backing raid faults the dead leg and keeps serving.

---

# Implementation record — 2026-07-26 (items #1, #7, #8)

Landed together, unit-tested (850 crate tests green, +27 new). The design
questions this doc left open were pinned as follows; a six-lens audit of
this doc against the flint tree and SPDK v26.05.1-pre preceded the landing
(all 26 flint citations and all SPDK citations verified; both §6.4 hazards
confirmed at source).

**#1 — leased + arbitrated claims (`volume_claims.rs`), the F43 fix.**
- **Preemption = reservation, NOT live seizure.** The doc's own evidence
  (`held_secs=0` every tick) shows catch-up releases and re-acquires each
  cycle — the starvation is a *between-acquisitions fairness race*. A
  resolver denied by a live maintainer holder posts a reservation; while a
  live reservation stands, maintainer re-claims are refused, so the
  resolver wins the natural release. No mid-copy abort exists anywhere.
- **In-process, not record-persisted** (decision 2026-07-26). The
  single-controller assumption is load-bearing already; a controller
  restart RELEASING claims is correct; correctness rests on the record's
  chain-gen, never on this registry (it is anti-waste). Persistence would
  add a kube CAS write per volume per tick and new failure modes for zero
  F43 benefit. R2's "episode fields on the record" remains open only as a
  visibility upgrade, to fold into #2's schema work if ever needed.
- **Classes are per claim SITE, not per module.** `cutover` and
  hot-rejoin's Rejoin window are **resolvers**; `catch-up`, hot-rejoin's
  marked-dispatch reconcile (new op `hot-rejoin-reconcile` — it performs
  the same work catch-up does and must not preempt it), and future
  `expand` are **maintainers**. Unregistered ops default to maintainer.
  Intra-class is first-come. The epoch scheduler defers cuts for BOTH
  hot-rejoin ops (`is_hot_rejoin_op`), preserving pre-change behavior.
- **Convergence stays `plan_cutover`'s job.** Cutover only try_claims
  after the planner returns a bounce decision (standby present, lag ≤
  max_lag) — the registry's class rule is unconditional.
- **Anti-starvation both directions** (catch-up's claim carries the F40
  replace dispatch): a reservation lapses when idle
  (`FLINT_CLAIM_RESERVATION_TTL_SECS`, 180) and even while actively
  refreshed lapses at an age cap (`FLINT_CLAIM_RESERVATION_MAX_SECS`,
  900) into a backoff (`FLINT_CLAIM_RESERVATION_BACKOFF_SECS`, 120) that
  guarantees the maintainer a periodic turn under a persistently failing
  bounce.
- **Lease = wedge-backstop only** (`FLINT_CLAIM_LEASE_SECS`, default
  14400): seizure bumps the entry generation so the stale holder's RAII
  drop is inert; it never aborts the old task (rev-5 makes the overlap
  safe-wasteful). Renewal deliberately omitted — catch-up's no-watchdog
  design stands; FLINT_COPY_STALL_SECS remains the wedge detector.
- Kill switch `FLINT_CLAIM_ARBITRATION=disabled` restores v1.19
  first-come. `log_claim_skip` gained the third outcome ("yielding to a
  reserved resolver"). Regression test:
  `resolver_wins_within_two_ticks_of_a_reclaiming_maintainer`.

**#7 — ublk construction guard.** TWO insertion points, not one:
- the node agent's `/api/spdk/rpc` boundary
  (`guarded_destroy::construction_boundary_verdict`, wired beside the R3
  destruction interception) — covers EVERY remote construction, because
  all controller flows and the driver's own NodeStage `bdev_raid_create`
  route through the executing node's agent;
- `nvmeof_export::ensure_export`'s pre-add_ns probe — the one LOCAL
  executor (unix socket), reached from exactly the post-restart
  rehydration flows the stale-ublk hazard targets.
  
  Probe = `bdev_identity_and_bytes` + `ublk_consumer_of` (forms-matched:
  name/uuid/aliases, the F36 lesson). ublk `Method not found` = no ublk
  support = no consumer (nvmeof-backend clusters must not brick); any
  other probe error DEFERS (F37). Refusal texts dodge every benign
  classifier (not-found / EEXIST / EBUSY — enforced by test). Site-list
  corrections vs this doc's original text: `driver.rs:992`
  (`create_nvmeof_target`) has no callers, `controller_operator.rs:369`
  is a disabled bin, `raid/raid_service.rs` is caller-less, and
  `catchup.rs` issues NO `bdev_raid_create` (admission only returns the
  base list; the create is `driver.rs ensure_raid1_bdev`). A construction
  lint test now mirrors the destruction lint. Kill switch
  `FLINT_CONSTRUCTION_GUARD=disabled`.

**#8 — leg-size guard (`leg_size_guard.rs`), three layers.**
- **Comparison is BYTES** (`num_blocks × block_size`), not num_blocks —
  lvstores auto-detect block size, so heterogeneous fleets can carry
  512-vs-4096 legs.
- **"The record's expected size" does not exist** (VolumeSyncRecord has
  no size field): the floor authority is `pv.spec.capacity` with `>=`
  semantics (MiB/cluster round-up), fetched best-effort.
- **The rule exploits the hazard's asymmetry** (larger leg = waste,
  shorter leg = corruption): keep the largest-size cohort, exclude
  shorter legs loudly (`LegSizeMismatchExcluded`), and FAIL the stage
  when even the largest cohort is below the capacity floor
  (`LegSizeGuardRefusedStage`) — the all-legs-short and single-survivor
  direct-serve cases member comparison cannot see. No majority voting: a
  lone grown survivor beats a stale majority.
- Layers: (a) the driver's **assembly belt** over the finalized base list
  (before the freshness gate and the record stamp; covers phase-3 legs,
  admitted standbys, and — via an inline check — forced-stale legs, the
  two routes that bypass `admit_standbys_at_stage`); (b)
  `admit_one_standby`'s pre-`record_in_sync` check (a refusal is a clean
  `StandbyAdmissionDeferred`, never an in_sync record for a leg the raid
  won't contain); (c) the construction boundary's create/add size arms as
  the last-resort backstop (also upgrades the hot-add short-leg park from
  an opaque `-EINVAL` to a named refusal at `hot_rejoin.rs`'s two add
  sites, which both route through the boundary). Kill switch
  `FLINT_LEG_SIZE_GUARD=disabled`.

**Post-landing adversarial review (same day, 18 findings → 12 confirmed →
all fixed, 855 tests green):**
1. *Claim generation was per-entry and reset on entry removal* — a
   seized-from holder's late drop could evict a LATER innocent holder
   (repro-proven: it also silenced the epoch scheduler's hot-rejoin cut
   deferral). Generations are now registry-wide monotonic
   (`next_generation: AtomicU64`); releases also garbage-collect dead
   entries (a volume deleted mid-episode no longer leaks its entry).
2. *Kill-switch split-brain*: the boundary's #8 size arms were gated on
   `FLINT_CONSTRUCTION_GUARD`, so `FLINT_LEG_SIZE_GUARD=disabled` turned
   the documented escape hatch into a harder brick (belt off → mixed-size
   create → boundary 409). The size arms now gate on
   `FLINT_LEG_SIZE_GUARD` (read on the NODE AGENT process — the boundary
   runs there); the ublk arms stay on `FLINT_CONSTRUCTION_GUARD`. Tests
   are env-free (`construction_boundary_verdict_gated`).
3. *Forced-stale-only assembly bypassed #8 entirely* (belt skipped on an
   empty in-sync list → no floor → the inline check inert → a solo short
   stale leg direct-serves with no boundary to catch it — the exact C2-B
   hazard). The capacity floor is now fetched whenever the guard is
   enabled, independent of the belt.
4. *`parse_quantity` couldn't parse `Ti`* (k8s canonicalizes BinarySI to
   the LARGEST even suffix) — the floor silently vanished for exactly the
   large volumes, and replica re-placement refused Ti volumes outright.
   Now parses Ki..Ei, decimal SI, decimal mantissas, and exponent forms;
   an unparseable capacity logs loudly instead of degrading silently.
5. *Belt-excluded writers tripped the F36c gate* as transiently-missing
   (manufactured 180s defer + a false AckedTailRisk): size-excluded legs
   now count as PRESENT for missingness — their lineage attached; the
   exclusion is a serving-size decision, not a missing writer.
6. *The #7 boundary could permanently wedge the single-survivor
   direct-serve → raid redundancy-restore path*: the volume's own stale
   direct-serve ublk disk over a leg 409'd the create with no healer (the
   rehydrate reaper skips multi-replica PVs). A pre-assembly CONVERGER now
   stops disks attributable to THIS volume (id ∈ {`flint.io/ublk-id`
   annotation, hash fallback}, bdev ≠ the raid itself) through the guarded
   stop — whose F37 kernel-opener probe still vetoes a genuinely live
   consumer. Foreign-id disks stay untouched (the boundary refusal is the
   correct outcome for those).

**Still open after this landing:** #2 (ChainIntent — needs its own design
pass), #3 (taint feed — needs detector-site enumeration), #4 (cordon
escalation — reachable now that #1 lets cutover run; still lowest
priority), #5 (the live acceptance drill, permanent-variant — the gate for
closing F43), and the kind race tier's deploy/scenario/runner build (the
mock needs a per-bdev size knob before #7/#8 scenarios have teeth).

---

# v1.20.0 scope — the deferred "completeness half" of the contract

F43 is not the only item the two-wave campaign deferred. The waves shipped
the **correctness-urgent half** of each contract rule and deferred the
**completeness half** to v1.20.0 (consistent pattern: R2 shipped its
node-local lock, deferred its controller-claim → F43; R1 shipped its
generation counter, deferred its intent record → #2 below). Land these
together, since #1 and #2 share the record-schema work and #5 validates both;
#7 is independent (it reuses the existing guarded-destroy probe). #8 is the
admission-side size guard — it must precede any volume-expansion work (see
"Ordering constraint" at the end).

### 1. F43 / R2 controller-claim arbitration (this doc — headline)
Replace the process-global, expiry-less, priority-less `volume_claims.rs`
with R2's **leased, visible, arbitrated** controller claims (episode fields
on the record). Add an explicit **priority rule: cutover (resolver) preempts
catch-up (maintainer) for a converged standby** — lease-expiry alone is
insufficient because catch-up is an *active* re-claimer, not a paused holder.
Keeps hot-rejoin RWO-only. Un-starves the RWX admission (`BounceNfsPod`).

**Design input — size the arbitration for a fourth claimant.** Volume
expansion (`docs/volume-expansion-status-2026-07.md`) will have to enter the
registry. Today `ControllerExpandVolume` is the **only controller mutation
path that takes no claim at all** (`main.rs:2240-2249`; the three claimants
are `catchup.rs:2491`, `cutover.rs:995`, `hot_rejoin.rs:2464,2511`) — tolerable
only because it resizes one lvol on one node. A multi-replica fan-out expand is
a long-running per-volume op and must claim. Classify it as a **maintainer**
(yields to cutover and hot-rejoin), and note it is an *active* re-claimer like
catch-up, since external-resizer retries a failed `ControllerExpandVolume`
indefinitely. So build the priority rule as a resolver/maintainer **class**,
not a hardcoded cutover-beats-catch-up special case — otherwise #1 is rewritten
when expansion lands. Detail: "Ordering constraint" below.

### 2. R1 ChainIntent record — wave-1 item 7 (ready-node exclusion refusal)
`chain-gen` (the CAS generation counter) landed (`main.rs:1680` bump-before-
attach; `node_agent.rs` phantom-hygiene re-read), but the full **ChainIntent
desired-topology record did NOT** (`driver.rs:1937`: *"When the chain-intent
record lands…"*, future tense). So wave-1 item 7 — **outright-refuse an
intent-driven exclusion of a HEALTHY (Ready-node) leg** — is still deferred;
today it is defer-then-serve-with-risk. NOTE: the interim already closes the
acked-loss *laundering* hole (evented, bounded serve), so this is
structural hardening, not an open data-loss hole — lower urgency than F43,
but it is the same "half-a-rule-landed" shape and belongs here.

### 3. Wire the `out-of-service` taint feed (R-upstream)
`node.kubernetes.io/out-of-service` (and the unreachable `NoExecute` taint)
are **consumed by no NodeGone detector** (grep-empty in `src/`), yet the pNFS
operator runbook already tells operators to apply the taint on a dead node.
Operator-expectation mismatch: applying it accelerates nothing today (the
existing NodeGone signals — Node-object deletion, NotReady threshold — still
fire, just slower). Feed the taint into the NodeGone detectors as death
evidence. **Never treat a taint as an I/O fence** (contract "Lean on
upstream").

### 4. R4 cordon/anti-affinity escalation (lower priority)
For a **persistently ineffective** bounce. The common same-node-reuse case is
ALREADY handled by the bounce taint (`BOUNCE_TAINT_KEY`, `cutover.rs:66-77` —
forces a restage even on same-node placement). Deferred piece
(`cutover.rs:31-32`): a further scheduling-hint escalation when a bounce stays
`CutoverIneffective` across cooldowns. It is *evented*, not silent, so this is
the lowest-risk item — but it becomes reachable once #1 lets cutover run for
RWX-r2.

### 5. Chaos drill 3.6/r2-perm — RWX numReplicas≥2 backing-LEG node kill (PERMANENT)
(Renamed 2026-07-26 — the old title "nfs-server NODE kill" named the one
kill vector that structurally CANNOT demonstrate the fix: killing the
server's node resurrects the NFS pod on a survivor → fresh NodeStage →
`admit_standbys_at_stage` admits at restage without cutover ever running.
The recipe was always right: **terminate a backing-raid LEG node, NOT the
NFS server's node, and delete its Node object** so no natural restage ever
comes — only then does admission depend on cutover's bounce and exercise
the #1 arbitration.) AWS-gated; the permanent variant was never run in the
2026-07 campaign (the transient 3.6 family was — see "Why deferred" #4).
This is the acceptance for #1 (and exercises #4). Recipe: see "Acceptance
drill" above. runad ran it once and found F43; make it a standing matrix
entry.

### 6. The 4 MUST-VERIFY-ON-REAL-SPDK assumptions (not fixes — latent risks)
From the contract's wave-2 drill list; each is a "the guard assumes X but SPDK
may do Y" risk. **Resolved 2026-07-24 by source read of SPDK v26.05.1-pre**
(the tree flint targets — matches `mock-spdk-tgt.py`'s reported version).
These are static code-path reads (claim/event/RPC *wiring*), definitive for
what they cover; a runtime confirmation is still nice-to-have but there is no
ambiguity left in the code. **Three of four hold; #4 is a real hazard.**

1. **`bdev_lvol_start_shallow_copy` to a busy lvol — GUARDED (EBUSY/EPERM,
   never silent interleave).** Two mechanisms:
   - *Same destination:* the dst takes `SPDK_BDEV_CLAIM_READ_MANY_WRITE_ONE`
     (`module/bdev/lvol/vbdev_lvol.c:2055`); a second copy fails
     **synchronously** with `-EPERM` (`lib/bdev/bdev.c:8867`; `claim_verify_rwo`
     at `bdev.c:9590` scans open descriptors).
   - *Same source, different dst:* the blob's `locked_operation_in_progress`
     flag returns `-EBUSY` (`lib/blob/blobstore.c:7509`) — but
     **asynchronously**: `start` returns success + an `operation_id`; the error
     surfaces only via `bdev_lvol_check_shallow_copy`.
   - *Flint:* SAFE. `catchup::shallow_copy` (`src/catchup.rs:1302`) polls
     `bdev_lvol_check_shallow_copy` (`catchup.rs:1350`) and surfaces the error
     state (test `shallow_copy_surfaces_error_state`). The async delivery does
     not reach flint as a false success.

2. **Allowed-host REMOVAL — SEVERS (because flint uses the RPC, not the C
   API).** The bare C API `spdk_nvmf_subsystem_remove_host` blocks new connects
   only — the header says so (`include/spdk/nvmf.h:908`); the allowed-host check
   is connect-time only (`lib/nvmf/ctrlr.c:848`), with **no I/O-path check**.
   But the **RPC** `nvmf_subsystem_remove_host` does the documented two-call
   pattern: remove-from-list **then** `spdk_nvmf_subsystem_disconnect_host`
   (`lib/nvmf/nvmf_rpc.c:2038`), which walks all qpairs, disconnects the
   matching host, and polls a drain loop until torn down.
   - *Flint:* SAFE — no fencing hole. Flint issues the RPC method
     (`src/hot_rejoin.rs:535`, `src/nvmeof_export.rs:345`), so it gets active
     severing. Caveat: severing completes only when the disconnect+drain
     finishes within `timeout_ms`; flint sends none, so it rides the default.

3. **Deleting an lvol hot-removes its nvmf ns — RELIABLE, no use-after-free.**
   ns-add opens the bdev with an event cb (`spdk_bdev_open_ext_v2(...,
   nvmf_ns_event, ...)`, `lib/nvmf/subsystem.c:2564`). On
   `SPDK_BDEV_EVENT_REMOVE` the chain `nvmf_ns_event → nvmf_ns_hot_remove →
   pause → remove_ns` closes the desc and frees the ns
   (`subsystem.c:2235`). The bdev is kept alive until the last descriptor
   closes (`lib/bdev/bdev.c:9317`) — no UAF; a concurrent explicit `remove_ns`
   is safe (the REMOVE event sees `desc->closed` and skips). Failure windows
   are OOM / subsystem-destroy only — negligible.
   - *Caveat (intersects R5):* the `bdev_lvol_delete` RPC response is **gated
     on the subsystem pause→remove→resume cycle**. Against a busy subsystem
     that pause can take real time — relevant to the R5 RPC-deadline scenario.

4. **ublk-served bdev vs `bdev_raid_create` / `nvmf_subsystem_add_ns` — HAZARD:
   duplicate construction SILENTLY SUCCEEDS.** ublk opens the bdev for write
   but **never claims it** (`lib/ublk/ublk.c:1912` — no `spdk_bdev_module_claim_*`
   anywhere in the ublk module), so `claim_type` stays `SPDK_BDEV_CLAIM_NONE`.
   Both raid's base-add claim (`module/bdev/raid/bdev_raid.c:3519`) and nvmf's
   add-ns claim (`lib/nvmf/subsystem.c:2592`) check only `claim_type != NONE`,
   so both **succeed silently** → two live writers, no mutual exclusion. The
   legacy `SPDK_BDEV_CLAIM_EXCL_WRITE` blocks a *later open* but never scans for
   an *existing* unclaimed write-opener (the newer `READ_MANY_WRITE_ONE` would,
   but neither raid nor nvmf uses it). Contrast: nvmf-then-raid IS blocked,
   because nvmf claims.
   - *This is the one open risk.* SPDK will not stop it — the fix is a
     **flint-side control-plane guard**: refuse `bdev_raid_create` /
     `nvmf_subsystem_add_ns` over a bdev this node is currently serving via
     ublk. **Promoted to v1.20.0 item #7 below** (the construction-side mirror
     of the existing guarded-destroy). That guard is control-plane logic the
     **kind race tier CAN regression-test** (assert the construct-over-ublk-
     served call is refused), even though the underlying SPDK permissiveness
     cannot be reproduced there — the same "half on each tier" split as R2/F43.
     NOTE: the race-tier mock's canned `add_ns`/`raid_create` return success, so
     it would give *false confidence* here until the flint-side guard exists to
     assert against.

### 7. ublk-served-bdev construction guard — from §6.4 (the SPDK #4 hazard)
The source read (§6.4) confirmed the worst case: neither `bdev_raid_create`
(base-bdev add) nor `nvmf_subsystem_add_ns` takes a claim that conflicts with
ublk's unclaimed write-open, so **constructing a raid or nvmf namespace over a
bdev this node is already serving via ublk silently succeeds** → two live
writers on one bdev, silent corruption. SPDK will not stop it; the guard must
be flint-side.

**It is the construction-side mirror of the existing guarded-destroy.**
`guarded_destroy.rs` already refuses a raid *delete* while a ublk disk consumes
it (`guarded_destroy.rs:190-205`), probing via `ublk_consumer_of` →
`ublk_get_disks` (`guarded_destroy.rs:423-436`). The fix reuses that same probe
in the *construction* path: before `bdev_raid_create` / `nvmf_subsystem_add_ns`
over a base bdev, refuse if `ublk_consumer_of` reports a live ublk disk on it.

**Reachability / priority — structural hardening, not a happy-path hole.** Not
reachable on the normal backing-first, frontend-last construction order; it
requires a *stale/rogue* ublk disk surviving into a reconstruction — the
phantom/re-mint family (cf. `identity.rs:90-91,774-775`: ublk devices re-minted
under the hash-fallback id across abrupt csi-node restarts). Same duplicate-
construction risk class as R1's chain-gen and R3's guarded-destroy. So: lower
urgency than F43 (#1), but genuine silent corruption if the race is hit, and
cheap (reuses the guarded-destroy probe) — it belongs in the same landing.

**Kind-testable (unlike the SPDK behavior itself).** The guard is control-plane
logic: the race tier can seed a mock `ublk_get_disks` reporting a live consumer,
then assert the driver *refuses* `bdev_raid_create` / `add_ns` over that bdev —
a new race-tier scenario alongside R1/R2/R5. Per §6.4, the mock's canned
construction success means this scenario only has teeth once the guard exists.

### 8. Leg-size precondition on admission + expand health-gate
**The hazard (full derivation in C2 under "Ordering constraint" below):** once
legs can differ in size — which only volume expansion makes possible — the two
admission paths fail in opposite ways. A hot-add to a live raid is **refused**
`-EINVAL` (`bdev_raid.c:3570-3573`) → parked standby, loud, no data risk. A
NodeStage **reassembly** (`admit_standbys_at_stage`, `catchup.rs:1973`) instead
brings the raid back at `min(leg blockcnt)` with **no error whatsoever**
(`raid1_start`; the `-EBUSY` shrink guard at `bdev.c:5737-5741` cannot fire on
a fresh bdev with no open descriptors) — **a silent shrink underneath an
already-grown filesystem.** That is the data-integrity case, and SPDK will not
stop it.

**Two guards, both flint-side control plane:**
1. **Admission-side (the real fix).** Before including a leg in
   `bdev_raid_create` / `bdev_raid_add_base_bdev`, compare its actual
   `num_blocks` against the other members and against the record's expected
   size. Refuse-and-event a short leg rather than constructing over it. Compare
   **measured `num_blocks`, not requested bytes** — `resize_lvol` rounds up to
   MiB (`minimal_disk_service.rs:409`) while `create_lvol` sizes in bytes, and
   both land on lvstore cluster granularity, so a byte-level comparison will
   produce false mismatches.
2. **Expand-side (cheap belt).** Refuse `ControllerExpandVolume` when the
   record shows any replica not `in_sync` — a degraded volume should not start
   an expand at all. Land this with the expansion work; it is one precondition
   in the fan-out path.

**Why it belongs in v1.20.0 and not in the expansion work.** Guard 1 hardens
the **admission step #1 exists to unblock**, and it is the difference between
an opaque `-EINVAL` park (indistinguishable from F43 itself, which will make
#5's drill ambiguous to read) and a named, evented refusal. It also stands on
its own: any future path that produces a size-diverged leg — a partially-failed
expand, a hand-repaired lvol, a restore — gets the same protection. Landing the
guard first means expansion cannot ship the hazard, in either direction.

**Reachability / priority — same shape as #7.** Not reachable today: without
expansion every leg is created at `pv.spec.capacity`
(`replica_replace.rs:239`) and catch-up sizes rebuilt heads from the source
head exactly (`catchup.rs:1713-1725`), so legs cannot diverge. It is a
**prerequisite**, not an open hole — lower urgency than #1, higher than #4,
and cheap.

**Kind-testable.** Like #7, this is control-plane logic the race tier can cover
without real SPDK: have `mock-spdk-tgt.py` report a short `num_blocks` for one
leg, then assert the driver **refuses** the create/add and emits, instead of
constructing a shrunken raid. Note the same caveat as #7 — the mock's canned
construction success means the scenario only has teeth once the guard exists.

---

# Ordering constraint — volume expansion must land after #1

`docs/volume-expansion-status-2026-07.md` scopes multi-replica / RWX volume
expansion. The two workstreams are **architecturally independent** — disjoint
code paths (expand: `main.rs:2138-2259` / `4354-4443`,
`minimal_disk_service.rs:405-426`, dashboard; F43: `volume_claims.rs`,
`cutover.rs`, `catchup.rs`, record schema), neither reads the other's state,
and F43's fix changes no expand path — but the **ordering is not free**. Four
coupling points, in priority order. Verified against `main` @ 2026-07-24 and
SPDK v26.05.1-pre.

### C1. Expansion must join the claim registry — and the current registry can't take it
See "Design input" under #1 above for the classification rule. The reason it
*must* claim rather than stay outside: SPDK's `spdk_blob_resize` returns
**`-EBUSY`** when `locked_operation_in_progress` is set
(`lib/blob/blobstore.c:8040-8051`) — exactly the flag catch-up's shallow copy
sets on the source blob (§6.1). So an unclaimed expand firing mid-catch-up
fails on that leg → **partial expansion** (some legs grown, one not). Adding
expand to today's expiry-less, priority-less mutex instead makes F43 *worse*:
a second active re-claimer competing in a registry that cannot arbitrate.
**Therefore: #1 first, then wire expand in.** Implementing expand's claim
integration against the current `volume_claims.rs` means writing the
arbitration twice.

### C2. Expanding during a degraded window → parked standby OR silent shrink
`raid1_resize` is deliberately degraded-safe — it **skips absent legs**
(`desc == NULL`, `raid1.c:594`) and grows the raid on the survivors, setting
`base_info->data_size = min_blockcnt` on all of them. That is precisely what
makes this reachable: expand while a leg is faulted (the F42/F43 window) and
the raid's `blockcnt` rises without that leg. What happens when the stale,
still-old-sized leg comes back **depends on the admission path**, and the two
differ sharply:

**A — hot-add to a LIVE raid (`hot_rejoin.rs:892,1260`, `--skip-rebuild`):
refused, loudly.** `raid_bdev_free_base_bdev_resource` clears `data_offset` but
**not `data_size`** (`bdev_raid.c:429-459`), so the vacated slot still carries
the grown size. The re-add then trips:

```c
} else if (base_info->data_offset + base_info->data_size > bdev->blockcnt) {
        SPDK_ERRLOG("Data offset and size exceeds base bdev capacity %lu on bdev '%s'\n", ...);
        rc = -EINVAL;            /* module/bdev/raid/bdev_raid.c:3570-3573 */
```

Result: a **permanently parked standby — the exact F43 user-visible failure**,
from an unrelated cause, on the very step #1 exists to unblock. No corruption.

**B — fresh raid CREATE at reassembly (`admit_standbys_at_stage`,
`catchup.rs:1973`): SILENT SHRINK.** That path deliberately defers when the
raid is already ONLINE (`catchup.rs:2006-2021`), so its admission *is* a
NodeStage reassembly — a fresh `bdev_raid_create` where every
`base_info->data_size` starts at 0 and is therefore set to that leg's own
`bdev->blockcnt` (`bdev_raid.c:3568-3569`). `raid1_start` then takes the
minimum and applies it to the raid itself:

```c
RAID_FOR_EACH_BASE_BDEV(raid_bdev, base_info) {
        min_blockcnt = spdk_min(min_blockcnt, base_info->data_size);
}
...
raid_bdev->bdev.blockcnt = min_blockcnt;    /* module/bdev/raid/raid1.c raid1_start */
```

The raid comes back **smaller than it was**, with **no error at all** — the
`-EBUSY` shrink guard in `spdk_bdev_notify_blockcnt_change`
(`bdev.c:5737-5741`) does not apply, because a freshly-created bdev has no open
descriptors yet. And the filesystem on it was already grown to the larger size
by `NodeExpandVolume`, so ext4/xfs now believes in blocks the device no longer
has: I/O errors on the tail, remount-ro, potential corruption. **This is a
data-integrity hazard, not an availability one** — and it is the reason the
guard is v1.20.0 item **#8** rather than a note on the expansion work.

`replica_replace.rs:239` sizes the placeholder from `pv.spec.capacity` and
catch-up's `revert_head_to_empty` sizes from the source head exactly
(`catchup.rs:1713-1725`), so the steady state is consistent; the expand window
is the race that makes legs diverge in the first place.

### C3. The undersized-leg direction fails quietly
If an undersized leg does get in and a resize event fires, `raid1_resize`
computes `min < blockcnt` → `spdk_bdev_notify_blockcnt_change` rejects the
shrink with `-EBUSY` while descriptors are open (`lib/bdev/bdev.c:5737-5741`)
→ logs "Failed to notify blockcount change", returns false, and `data_size` is
left un-updated (`raid1.c:607-616`). The raid stays large over a small leg. No
corruption and no unwind — but no event either. Worth an emit if expansion
lands.

### C4. RWX expansion collides with cutover — and only #1 makes the collision reachable
The expansion doc's layer-4 fix (patch the backing PVC capacity so kubelet
drives `NodeExpandVolume` on the NFS server's node) needs that pod **stable and
mounted** while `resize2fs` runs. `BounceNfsPod` deletes and restages it, with
`BOUNCE_TAINT_KEY` forcing a restage even on same-node placement
(`cutover.rs:66-77`). Today they cannot collide — cutover is starved for
RWX-r2, which *is* F43. Once #1 lands, they will: RWX expansion must hold the
claim across the backing-PVC-driven FS grow, or be preempted cleanly and
retried. Note also the backing PV is flint-provisioned (`rwx_nfs.rs:227-333`)
and its handle is `VolumeRef::NfsBacking`, refused by a **separate** arm of
`expand_refusal` (`identity.rs:194-196`) — RWX expansion has to open both arms,
and drill 3.6/r2 (#5) should then run once with an expand in flight.

### Attach/detach regression risk (the narrow question)
For the scope the expansion doc recommends shipping first — **multi-replica RWO
grow on the `nvmeof` backend — essentially none.** `NodeExpandVolume` runs
after staging on an already-mounted device (`findmnt`/`blkid`/`resize2fs`,
`main.rs:4416-4427`) and touches no staging, publish, export or assembly logic;
the controller side adds a fan-out loop and reuses `kernel_nvme_ns_rescan`
(`node_agent.rs:1772-1787`).

**The ublk piece is a genuine regression vector.** Part 1 of the required SPDK
patch adds a `SPDK_BDEV_EVENT_RESIZE` case to `ublk_bdev_event_cb`
(`lib/ublk/ublk.c:1530-1538`) — the same callback whose `REMOVE` handling the
DEL_DEV / F8 / F9 detach work depends on — plus a kernel ≥ 6.16 floor. Keep it
last, as the expansion doc's own §2.2 sequencing already recommends.

### Recommended sequence
1. **#1 (F43 / R2 leased + arbitrated claims)** — the substrate expansion needs.
1b. **#8 admission-side leg-size guard** — before any leg can diverge in size.
2. Multi-replica **RWO** grow on `nvmeof`, registered as a maintainer claimant,
   with #8's expand-side not-`in_sync` refusal.
3. **RWX** expansion (cutover-vs-expand ordering — cheap once #1 gives priority
   semantics).
4. **ublk** online grow last, gated on the SPDK patch + kernel floor, with the
   clean-refusal fallback.

---

Related: R1/R2/R4 in `attach-detach-robustness-contract.md` (see the wave
tables + "Deferred deliberately"); `docs/UnansweredOn7b.md` (Option B,
hot-rejoin RWO-scoping); `docs/attach-detach-campaign-2026-07.md` (skipped
drill 3.6/r2, MUST-VERIFY list); `docs/volume-expansion-status-2026-07.md`
(volume expansion — see "Ordering constraint" above).
