# S2 — bounce-free RWX admission (model-first design)

Status: **designed + formally verified, 2026-07-28; implementation next
cycle.** The formal work landed ahead of the code on purpose: every
design-level safety and liveness question below is answered by a TLC run
in `formal/`, not by prose.

## Problem

RWX replacement/rejoin admission today rides the cutover machinery: the
NFS server is bounced (teardown → NodeStage reassembly with the new leg →
client reconnect + grace). Costs, all campaign-measured:

- An outage window per admission (the ~237s bounce stall observed on the
  runag-era drills; P4 fixed the *detection* side to ~36s, but the bounce
  itself remains).
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
4. Regression: 3.6e (relocation still bounces — DrainGate/F55 gate
   applies there), 2.9 canary.

## Open questions folded in deliberately

- Expansion interplay: orchestrated RWX expand also quiesces — same
  claim domain serializes them (verify in implementation).
- The degraded-refusal belt must keep refusing expansion during an
  admission window (shared claim makes this structural).
