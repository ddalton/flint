# F43 — RWX multi-replica re-placement never restores redundancy (claim starvation)

**Status:** OPEN, deferred to v1.20.0. Found live on runad 2026-07-23 (RWX
`flint-r2`, numReplicas=2). Not a regression — a pre-existing gap the
attach/detach contract already earmarked (see "Why deferred"). No data-loss
component: the volume serves correctly **degraded** throughout.

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
4. **The triggering drill was never run.** The campaign
   (`docs/attach-detach-campaign-2026-07.md`) matrix skipped drill **3.6
   "nfs-server NODE kill (r2)" — "needs SSM+EC2 and an r2 harness; not run."**
   RWX×r2×node-kill is the empty cell between validated RWX-r1 (Phase 3) and
   RWO-r2 (Phase 2). runad is the first cluster to fill it.

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

# v1.20.0 scope — the deferred "completeness half" of the contract

F43 is not the only item the two-wave campaign deferred. The waves shipped
the **correctness-urgent half** of each contract rule and deferred the
**completeness half** to v1.20.0 (consistent pattern: R2 shipped its
node-local lock, deferred its controller-claim → F43; R1 shipped its
generation counter, deferred its intent record → item 7 below). Land these
together, since #1 and #2 share the record-schema work and #5 validates both.

### 1. F43 / R2 controller-claim arbitration (this doc — headline)
Replace the process-global, expiry-less, priority-less `volume_claims.rs`
with R2's **leased, visible, arbitrated** controller claims (episode fields
on the record). Add an explicit **priority rule: cutover (resolver) preempts
catch-up (maintainer) for a converged standby** — lease-expiry alone is
insufficient because catch-up is an *active* re-claimer, not a paused holder.
Keeps hot-rejoin RWO-only. Un-starves the RWX admission (`BounceNfsPod`).

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

### 5. Chaos drill 3.6/r2 — RWX numReplicas≥2 nfs-server NODE kill
AWS-gated ("needs SSM+EC2 and an r2 harness"), **never run** in the 2026-07
campaign — the empty cell between validated RWX-r1 (Phase 3) and RWO-r2
(Phase 2). This is the acceptance for #1 (and exercises #4). Recipe: see
"Acceptance drill" above. runad ran it once and found F43; make it a
standing matrix entry.

### 6. The 4 MUST-VERIFY-ON-REAL-SPDK assumptions (not fixes — latent risks)
From the contract's wave-2 drill list; unclear if ever validated on real
SPDK. Each is a "the guard assumes X but SPDK may do Y" risk:
1. `bdev_lvol_start_shallow_copy` to a dst already copying — EBUSY or silent
   concurrent interleave?
2. Does allowed-host REMOVAL sever an established controller connection, or
   only block new connects?
3. Does deleting an lvol reliably hot-remove its nvmf namespace?
4. Does a ublk-served lvol block `bdev_raid_create` / `nvmf_subsystem_add_ns`
   the way an nvmf write-open does? (If ublk takes no exclusive claim,
   duplicate-construction over a ublk chain may silently succeed.)

---

Related: R1/R2/R4 in `attach-detach-robustness-contract.md` (see the wave
tables + "Deferred deliberately"); `docs/UnansweredOn7b.md` (Option B,
hot-rejoin RWO-scoping); `docs/attach-detach-campaign-2026-07.md` (skipped
drill 3.6/r2, MUST-VERIFY list).
