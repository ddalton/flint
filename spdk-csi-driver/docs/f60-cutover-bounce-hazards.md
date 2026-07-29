# F60 (candidate family) — cutover bounce hazards: the unbelted planner, the double creator, and the churn loop

**Status:** found 2026-07-29 by the cutover modeling tranche (`formal/`
runs 11a–11e and 12a–12e) plus a code scout of `cutover.rs` / `rwx_nfs.rs`.
**FIXED IN CODE 2026-07-29** (and the belt's own liveness is now gated —
runs 12f/12g, added after a code review found the unbounded-refusal bug
the model structurally could not express: `BouncePreflight` is a guard,
so a blocked bounce was merely a disabled action, and under a crash
budget "transiently unavailable forever" is unreachable) — §1 (the commit-time preflight), §3 (BOTH
halves of the double-creator fix), and §4 (persisted attempt bookkeeping
with capped backoff, plus the orphaned-flag sweep). §4b was checked and
deliberately NOT fixed: the shipped per-leg suppression already closes
it. The mutation runs that found each defect are now its regression test,
and the constants naming a belt (`BouncePreflight`, `ReconcilerBelt`,
`DetachWaitHonored`) describe SHIPPED behaviour with `FALSE` as the
pre-fix world. Owed: a live gate (drill 3.12/3.6e with the kill switch
off, exercising the fallback bounce through the new belt).

## What landed

| Fix | Where |
|---|---|
| Commit-time preflight over every recorded writer; refuses while any is transiently unavailable, blind-fails-closed | `bounce_preflight` / `recorded_writers`, called at the top of `execute_cutover` |
| Shares the gate's `node_gone_secs` threshold so a permanently-NotReady node cannot strand the volume | `CutoverConfig::gate` |
| Time-bounded recreate claim taken before the delete, released on every exit path. The annotation stores an EXPIRY sized from the configured `detach_timeout` (+60s), not a fixed TTL — a fixed one would silently expire mid-wait whenever an operator raised `FLINT_CUTOVER_DETACH_TIMEOUT_SECS`, reopening the race with no signal. Boundedness is enforced by the READER (a 900s horizon cap), so a dead bouncer or a bad clock cannot disable the reconciler indefinitely | `BOUNCE_IN_FLIGHT_ANNOTATION`, `bounce_claim_deadline`, `bounce_claim_active`, `set_bounce_in_flight` |
| The reconciler honours the claim | `nfs_reconcile_decision`'s new `bounce_in_flight` arm (`rwx_nfs.rs`) |
| Detach timeout hands off instead of recreating into a staged volume | `execute_cutover`'s timeout branch + `CutoverDetachTimeout` event |
| Persisted attempt counter with capped exponential backoff, charged on success AND failure, cleared on progress | `CUTOVER_ATTEMPTS_ANNOTATION`, `attempt_gate`, `attempt_backoff_secs` |
| Orphaned data-path flags cleared when their flagging node is gone | the sweep in `cutover_tick` + `DataPathFlagOrphaned` event |

12 new unit tests, including the polarity test (blind must refuse), the
bounded-claim fail-open test, and the inverted detach-timeout contract.

**Scope note up front, because it inverts the obvious worry.** The first
thing to check about a bounce is whether it can strand an RWX volume:
`execute_cutover` deletes the `flint-nfs-<vol>` server pod — a BARE pod
with no controller behind it — captures the replacement spec only in a
local variable, then waits (`await_detached`, up to `detach_timeout`,
polling every 2s) before re-creating it. A crash, a lease loss, or a
failed `create` in that window looks like it should leave the volume with
no server.

**It does not, and that is deliberate.** `rwx_nfs.rs`'s liveness
reconciler (`nfs_reconciler_pass`, leader-gated, 30s tick, spawned in the
controller at `main.rs:389-399`) recreates the server through the
publish-side ensure machinery whenever its truth table says
`[pvc-backed, PV not deleting, ≥1 client attachment, liveness ∈
{Absent, Dead}]` → `Recreate`. F35 (an evicted server sitting in phase
`Failed` forever) is exactly why the `Dead` cell is there. So the
availability question is closed in code, and the hazards below are about
something else.

---

## 1. The planner has no health term at all (the model's headline)

`plan_cutover` (`cutover.rs:305-385`) decides to tear down a serving data
path while reading only `sync_state` and `last_epoch`. The struct it
reads them from, `VolumeCutoverView` (`cutover.rs:271-289`), carries **no
leg health, no serving membership, and no writer set**. Nothing between
the decision and `serving = {}` re-checks whether the volume can come
back whole.

Contrast `drain_leg`, which probes the raid *before* its record round —
the maintenance path learned this lesson already, and the drain is
belted by "another leg still serves". The bounce has no equivalent.

`FlintReplicationBounceRisk.cfg` turns that into a counterexample with
**one bouncer, the lease fully honored, and no race of any kind**:

1. A leg is out of the raid while the record still calls it a writer;
   its own agent flags `data-path-lost` (this is the flag's designed
   meaning — attached here, no raid bdev).
2. The volume reassembles and that leg comes back **serving**. The flag
   has not been cleared yet — clearing is a separate pass, and the
   controller-side sweep only ever touches the `is_rwx && own_flag`
   residue class (`cutover.rs:834-851`).
3. A second leg dies. Ordinarily this is a routine fault-out: the
   survivor keeps serving, no risk is surfaced, nobody is paged.
4. The cutover tick fires on the **stale flag** and tears the volume
   down.
5. The reassembly can now only return by taking the freshness gate's
   `ServeWithRisk` arm — excusing an acked tail on a writer that was
   never verifiably dead.

The controller manufactured both the outage and the hollow risk marker.

**Proposed fix — the `BouncePreflight` belt.** Re-verify at the commit
point that every recorded writer is responsive or verifiably dead, and
refuse the bounce otherwise. `FlintReplicationBounceRaceFixed.cfg` proves
this belt carries bounce safety **with no leadership at all**, at the same
two-failure budget the counterexample needs.

One structural fact makes this the *only* belt cutover can host: **there
is no CAS anywhere in the module.** `delete_pod` uses
`DeleteParams::default()` (no UID, no resourceVersion precondition),
`recreate_pod` is a bare `create`, `taint_node`/`untaint_node` are
whole-array merge patches, and the RWX flag clear is an unconditional
merge patch. Where F59's fix could move the one-node-at-a-time rule
*into* `drain_for_maintenance`'s rv-guarded record mutation, cutover has
nothing to move a guard into — so the guard has to be a preflight
evaluated as late as possible.

## 2. The leader gate cannot close the two-bouncer race

`is_leader()` appears **once** in 1548 lines (`cutover.rs:714`), at the
top of the tick — never inside the per-PV loop and never inside
`execute_cutover`. The tick then walks every flint PV, and each bouncing
volume can block in `await_detached` for up to `detach_timeout` (120s),
against a 15s lease. This is the F59 shape exactly, on a second
subsystem. `FlintReplicationBounceRace.cfg` finds the violation **with
the gate ON**.

## 3. The liveness reconciler and the bounce are two creators of one bare pod

**Now modeled** — runs 12a-12d. This was originally out of scope; the
abstraction was wrong, and bringing the pod layer in produced an
eight-state counterexample with nothing exotic in it (12a):

1. A leg blackholes, is faulted out, recovers, and hot-rejoins as a warm
   standby — the ordinary trigger the cutover exists to serve.
2. The controller deletes the server pod. The window is `"clean"`: every
   writer is healthy, no risk of any kind.
3. The reconciler recreates the pod **while the volume is still staged**.
4. Kubelet reuses the staged volume. No NodeStage, no reassembly, no
   admission. The standby stays parked and clients ate an NFSv4
   grace-window recovery for nothing.

No partition, no zombie, no second failure, no leadership change, no
stale flag. 12a runs with the bouncer *idealized* (it recreates only
after the unstage it waited for), so the reconciler is isolated as the
sole cause.

**12c is the finding that changes the fix.** With the reconciler belted,
the SHIPPED timeout path reaches the same violation on its own:
`await_detached` returning false only WARNS and `execute_cutover`
recreates anyway ("recreating anyway — a same-node reuse will surface as
CutoverIneffective"). The bouncer defeats its own wait. **These are two
independent doors to one harm, so belting the reconciler alone leaves the
bug reachable.**

The mechanism, from the scout:

The detach wait exists to hold the pod down until kubelet unstages, so
the replacement is forced to restage and reassemble the raid — the §6
same-node race the module header describes. **For that entire wait the
pod is `Absent` with client attachments intact**, which is precisely the
reconciler's one `Recreate` cell. (The cutover waits on the *backing* PV's
VolumeAttachment, `identity::backing_pv_name`; the reconciler counts VAs
on the *user* PV. Different objects — the client attachments never drop.)

Both loops are `tokio::spawn`ed in the same process under the same lease,
and neither takes a lock the other respects. `nfs_reconcile_decision` is
a pure function of `(backend_is_emptydir, pv_terminating,
attachment_count, liveness)` — **there is no input that could carry "a
bounce is in flight"**, so the absence of a guard is provable from the
signature, not merely unobserved.

The loss is two-way: the early recreate defeats the unstage wait
(same-node reuse → no reassembly → the bounce bought clients a
grace-window recovery for nothing), and then cutover's own
`recreate_pod` POSTs into a taken name → 409 → `Err` → `CutoverFailed`.
Because only `Ok(true)` inserts into the `bounces` map
(`cutover.rs:1047-1056`), the attempt is never recorded and never
judged — no `CutoverIneffective`, no cooldown, no eligibility
bookkeeping. The escalation taint is the only damping, and it is
explicitly best-effort.

**Proposed fix, now shaped by 12b and 12c together:** make the
reconciler bounce-aware (a per-volume suppression the bouncer sets and
clears) **and** make the bouncer's timeout path refuse to recreate into a
still-staged volume rather than proceeding with a warning. 12b proves the
first half holds and still converges; 12c proves the first half alone is
insufficient.

**One caveat 12b does not cover, and the shipped fix honours:** the model
gives `BounceRecreate` weak fairness, i.e. it assumes the bouncer always
completes. It therefore never examines a bouncer that dies mid-window
while its own belt holds off the only other actor able to rebuild the
pod. The suppression is therefore **bounded and reader-enforced** — the
claim carries an expiry, and `bounce_claim_active` rejects expired,
unparseable, and absurdly-far-future values alike — never unconditional,
which would reintroduce exactly the stranding hazard §0 shows the
reconciler exists to prevent. That property is pinned by
`recreate_claim_is_bounded_and_fails_open` and
`claim_deadline_outlives_even_a_raised_detach_timeout`, not by the gate:
a belt whose necessary property the model cannot express needs a test.

## 4. The churn loop the safety belt does NOT close

`FlintReplicationBounceLoop.cfg` is a **canary**, not a theorem: with
every individual bounce belted safe, a volume can still eat an unbounded
series of them. Three separately-sufficient mechanisms, all straight-line
code facts:

1. **The `Err` arm records no attempt** (`cutover.rs:1058-1067`), so the
   documented 900s minimum between attempts is never applied on any
   failure path — including the 409 that §3 produces.
2. **`CutoverIneffective` re-arms eligibility** (`cutover.rs:913-928`)
   with no attempt counter, no backoff, and no negative caching anywhere
   in the file.
3. **A `data-path-lost` flag is clearable only by the flagging node's own
   agent** (`node_agent.rs:5241-5247`, `flagged_by_me`). Once that node
   is gone the verification predicate is permanently unsatisfiable, so
   the volume is eligible forever.

**Proposed fix:** an attempt counter with backoff, persisted on the PV
(the taint's application time and the flag's `since` already are), plus
an ownership-staleness sweep for orphaned flags.

## 4b. Two-planner disjointness — checked, and NOT owed

`plan_cutover` applies neither of `plan_hot_rejoin`'s admission filters,
so on paper it can commit a full teardown whose only purpose — admitting
a particular standby — the stage admission is guaranteed to refuse.

**The model says do not fix this.** With the per-leg maintenance marks
that landed in the wave-2 code wave (`SuppressScoped = TRUE`), a standby
on an unaffected leg is still admissible, so a bounce planned for it is
not doomed, and `Inv_NoDoomedBounce` holds with the planner left exactly
as it ships. The door was open only in the pre-fix volume-wide world.
The per-leg suppression fix closed a door nobody knew it was closing.

Two caveats. The hot-rejoin **marker** half of the same gap is untested —
markers live in `FlintClaims`' window abstraction, not here. And this
conclusion is only trustworthy because the invariant is attributive: the
run was first written against the shared pointless-rebounce canary, and
an A/B showed that canary fires with the filter ON as well as OFF, which
had briefly produced the opposite conclusion. A mutation run whose
invariant is violable for reasons other than the mutation proves nothing
about the mutation.

## 5. Open question booked, not answered: `AdmitAtStage` vs zombie fencing

`admit_standbys_at_stage` (`driver.rs:1967` → `catchup.rs:2301`) runs in
the NODE process, under no volume claim, with the raid not yet created,
and commits `record_in_sync` — **writer-set growth** — before
`freshness_gate::evaluate` rules (`driver.rs:2089`). The audit's verifier
argued this is the safe direction; the tranche set out to machine-check
that, and `Inv_WriterSetGrounded` is that check.

Its first run found a composition that is *not* about the bounce: the
at-stage admission grows the writer set while a partitioned old head (the
F48 zombie) can still be acking writes, leaving a recorded writer that
never served and does not hold the acked tail. The core safety theorems —
`Inv_NoSilentLoss` included — hold across it, so the invariant now
carries a `zombie = {}` guard and this is booked as an open question
about the at-stage admission's fencing order rather than as a finding.
Resolving it means tracing where NVMe-oF host fencing (`allowed_hosts`)
lands relative to the stage-time admission.

---

## Verification status

| Claim | Instrument |
|---|---|
| A belted bounce is safe, and the volume converges after one | `FlintReplicationBounce.cfg` (strict, two failures, both planner arms, `AdmitAtStage`) |
| The shipped planner manufactures the outage AND the hollow risk | `FlintReplicationBounceRisk.cfg` must violate `Inv_NoBounceInducedRisk` |
| The lease cannot close the two-bouncer race | `FlintReplicationBounceRace.cfg` must violate it with the gate ON |
| The preflight ALONE carries bounce safety | `FlintReplicationBounceRaceFixed.cfg` (belt ON, no leader gate) must hold |
| The belt does not close churn | `FlintReplicationBounceLoop.cfg` must violate `Inv_NoPointlessRebounce` |
| §3 (double creator) — the reconciler defeats the detach wait | `FlintReplicationBouncePod.cfg` must violate `Inv_BounceNotSilentlyDefeated` (bouncer idealized, so the reconciler is isolated) |
| The belt holds and the volume still converges | `FlintReplicationBouncePodFixed.cfg` (strict + liveness) |
| §3 second door — the bouncer defeats its OWN wait on timeout | `FlintReplicationBounceTimeout.cfg` must violate it with the reconciler already belted |
| A bounce planned for a standby the stage admission will refuse (pre-fix world) | `FlintReplicationBouncePlanner.cfg` must violate `Inv_NoDoomedBounce` |
| **The SHIPPED world already closes it** — no planner filter owed | `FlintReplicationBouncePlannerScoped.cfg` (strict, per-leg suppression + unfiltered planner) |
| The hot-rejoin MARKER half of the same gap | **none** — lives in `FlintClaims`' window abstraction |
| A bouncer that dies mid-window under its own belt | **none** — `WF(BounceRecreate)` assumes completion; why the suppression must be bounded |
| The belt does not starve the remediation it guards | `FlintReplicationBounceStarve.cfg` must violate `RemediationNotStarved`; `FlintReplicationBounceBounded.cfg` (shipped bound) must hold |
| The volume never lacks a serving NFS pod for N seconds | **drills only** — deliberately outside the model's abstraction |
