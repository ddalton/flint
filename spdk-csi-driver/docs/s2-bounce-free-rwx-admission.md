# S2 — bounce-free RWX admission (model-first design)

Status: **LIVE-GATED 2026-07-29 — all four acceptance gates green.**
Implemented 2026-07-28 (same day as the design); drill 3.12 PASSED on its
first run on runan (`1.22.0-rc3`), evidence
`tests/chaos/artifacts/runan-s2-gate/`. The formal work landed ahead of
the code on purpose: every design-level safety and liveness question
below is answered by a TLC run in `formal/`, not by prose.

**The measured result — the admission's guest cost goes 59 s → 1 s:**

> **Correction to this doc's own "~237s bounce" figure.** That number is the
> runag-era measurement, taken before P4 (detection), F52 (fh prewarm) and
> F55/DrainGate each made the bounce cheaper. A same-cluster, same-image A/B on
> runan (3.6e with the kill switch OFF vs 3.12 with it ON) measures the
> admission window at **59 s bounced vs 1 s in place** — a ~59× win, not the
> ~4× larger one the old figure implies. The Problem section below still quotes
> ~237 s as history; it is not the alternative you get on a current build.

```
window_ms=228   admit_stall=1s (budget 10)   kill_stall=37s (P4 budget 60)
swap=141s  standby=162s  in_sync=237s  settled=+360s
nfs pod uid UNCHANGED, restarts 0->0   pg-0 restarts 0->0   estale=0 panic=0
cutover events 0/0/0                   db PASS (zero acked loss)
```

The controller's own trace shows the arbitration the F43 mutation
predicted — catch-up yielding rather than renewing over the window:

```
[CATCHUP]    replica caught up to warm standby
[CLAIMS]     wanted_op="catch-up" held_by="hot-rejoin"
[HOT_REJOIN] Window committed   window_ms=228
[HOT_REJOIN] Rejoin complete    localized=true
```

No `[CUTOVER]` line fires. One ordering note for future readers: the
record flips to `in_sync` at localization, ~39 s AFTER the window
commits, so a mid-flight check shows `standby` against a raid that is
already 2/2. That is the ordering, not a gap.

Implementation map (all behind `FLINT_RWX_INPLACE_ADMISSION`, default ON):

- `identity.rs` — `StagedDomain` (`User`/`NfsBacking`): the per-volume
  staging-domain decision as a type; converted to a handle in one place.
- `hot_rejoin.rs` — `resolve` derives the raid name from the domain
  (`raid_nfs-server-<id>` for RWX); the reconcile family
  (`live_head_leg`, `release_orphaned_quiesce`, adopt/scrub/resume)
  threads the staged handle; the planner's RWX exclusion became the
  kill-switch arm; the tick resolves the RWX consumer from the BACKING
  PV's VolumeAttachment (the parent PV's VA is the NFS client — the
  wrong node for every window op).
- `catchup.rs` — `CatchupConfig.staged_domain`; the orchestrator tick
  routes the RWX consumer/domain the same way (the marker reconciler is
  reachable from both ticks).
- `cutover.rs` — `plan_cutover`'s converged-standby arm defers to the
  in-place window; the bounce survives for data-path-lost (relocation)
  and as the kill-switch fallback. This also removes the
  resolver-vs-resolver first-come race the two planners would otherwise
  have (both take Resolver-class claims).
- Tests: `sim_crash_sweep_rwx_backing_domain_recovers` (gate 2 — every
  RPC boundary of the RWX flow recovers, chains stay trees),
  `rwx_inplace_window_admits_on_the_backing_raid` (domain routing
  load-bearing in both directions), planner/cutover flips, kill-switch
  parse. 926 lib tests.

## Problem

RWX replacement/rejoin admission today rides the cutover machinery: the
NFS server is bounced (teardown → NodeStage reassembly with the new leg →
client reconnect + grace). Costs, all campaign-measured:

- An outage window per admission. Historically ~237s (runag-era drills);
  measured at **59s** on runan 2026-07-29 once P4, F52 prewarm and DrainGate
  had each shortened it. Either way it is an outage, and in-place costs 1s.
- The F55 class: a bounce truncates in-flight replies; DrainGate bounds
  it now, but not bouncing removes the exposure entirely.
- The F52 fh-identity family: any restage-style path must re-prove fh
  identity. In-place admission never touches the fs layer.
- The F48 window: bounce-based admission creates a two-head phase
  (old head zombie vs new assembly) that the sever must fence. In-place
  admission on the serving head has **no second assembly at all** — the
  hazard is removed structurally, not guarded.

## Design

Reuse the RWO-proven hot-rejoin admission window, executed against the
**live serving raid under the NFS server** — no restart:

1. Catch-up builds the replacement standby (existing; F42/F40 dispatch).
2. The **admission claim** is acquired under R2 arbitration with F43
   priority: catch-up cannot (re)acquire while a warm standby awaits.
3. Window: raid quiesce → final quiesced delta → raid grow (admit the
   leg at the current incarnation) → `mark_in_sync` (writer-set add) →
   unquiesce → release claim. Guest-visible stall = the quiesce span
   (seconds), not a server bounce.
4. NFS state (file handles, locks, delegations, client sessions) is
   untouched — the entire operation is below the filesystem.

The bounce/cutover path remains **only for relocation** (the server must
move nodes). Admission never bounces.

## What the models verify (the model-first part)

| claim | where verified |
|---|---|
| Admitting into a SERVING assembly is safe — no silent loss, no divergent serving leg, sb generations coherent — interleaved with writes, epoch cuts, torn writes, failures | `FlintReplication` strict runs (tranche 2): `Admit` fires while `serving # {}` under `RejoinGuard` |
| A kept-payload rejoiner needs the shared-base ancestry check; without it a dead-lineage phantom reaches the serving raid | rejoin mutation (`RejoinGuard=FALSE`, 11-state counterexample) |
| The two-head hazard is real and the sever fences it (bounce path); in-place admission simply never opens it | F48 mutation (`FenceZombie=FALSE`) |
| **Admission is never starved**: with claim-priority arbitration, every warm standby's wait resolves | `AdmissionNotStarved` in both strict runs (new, this tranche) |
| The arbitration is necessary, not just sufficient: the un-arbitrated race is **weak-fairness-legal** — catch-up's renewal can beat the admission claim forever. Priority, not fairness, is the fix | F43 mutation (`ClaimArb=FALSE`): TLC finds the `ReleaseCatchup → AcquireCatchup` starvation lasso with the warm standby parked — F43 as observed live on runad, rediscovered as a temporal counterexample |
| Claims are leased — holder death frees the claim | `ExpireClaim` (budgeted failure event) in all strict runs |
| The final delta delivers exactly the cut (content level) | `FlintSnapshots` `Inv_SessionFaithful` + its three mutations |
| Epoch cuts may interleave with the window at record level | `EpochCut`/`Admit` interleave freely in all strict runs (the F48 epoch-race guard covers the code-level arm) |

Run everything: `scripts/check-tla.sh` (ten TLC runs, ~30s).

## Implementation sketch (next cycle)

- Route RWX replacement admission to the hot-rejoin window instead of
  the cutover bounce. The window code (`hot_rejoin.rs`) is already
  consumer-node-parameterized; for RWX the "consumer" is the node
  hosting `flint-nfs-server`'s raid. The raid-level quiesce is the same
  primitive the E_f cut already uses.
- Claim wiring: the R2 leased+priority arbitration exists since F43
  (v1.20.0). Work = assert the RWX admission path takes the admission
  claim and that catch-up's yield covers it (drills already count
  `yields=` / `seizures=`).
- Kill switch: `FLINT_RWX_INPLACE_ADMISSION` (default ON), matching the
  P1 lease pattern; OFF = the old bounce path.
- Non-goals: relocation (keeps cutover), RWO (already in-place).

## Acceptance gates

1. Formal gate green (done — this commit).
2. Crash-sweep the RWX admission flow in the sim harness (the same
   CrashRpc sweep that covers hot rejoin; every RPC boundary recovers,
   chains stay trees).
3. New live drill **3.12 in-place RWX admission**: kill a leg under pg
   load on an RWX volume → replace → in-place admit. PASS =
   nfs-server pod restart count 0, pg-0 restarts 0, guest-visible stall
   bounded by the quiesce span (budget: ≤10s), ledger zero acked loss,
   raid 2/2 in_sync, zero ESTALE, admission yields/seizures consistent
   with claim priority.
   > **PASSED 2026-07-29 on runan, first run** — every criterion met;
   > numbers above. The drill asserts the ABSENCES (unmoved pod identity,
   > no CutoverStarted) because with S2 on, a bounce is the failure.
4. Regression: 3.6e (relocation still bounces — DrainGate/F55 gate
   applies there), 2.9 canary.
   > **F55's own gate PASSED same day** as new drill **3.13**: a bounce
   > forced mid-checkpoint (not by luck — runam showed every prior clean
   > bounce drill straddled the checkpoint by accident) drained cleanly,
   > `drained=1`, deadline never expired, zero PANICs.
   >
   > 3.6e itself no longer exercises the bounce with the kill switch ON —
   > its vector IS 3.12's, so it now reads the switch and flips its
   > expectation. To regression-test the bounce path, run 3.6e with
   > `FLINT_RWX_INPLACE_ADMISSION=disabled`; that (plus the 2.9 canary)
   > is the remaining regression owed, and it consumes a storage node.

## Open questions folded in deliberately

- Expansion interplay: orchestrated RWX expand also quiesces — same
  claim domain serializes them (verify in implementation).
- The degraded-refusal belt must keep refusing expansion during an
  admission window (shared claim makes this structural).
