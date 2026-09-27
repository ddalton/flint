# P1-lite sandbox, paired with its same-coverage baseline — paused 2026-09-25

Models (final, local):
- `../LeanCoreP1.tla` (P2 + P1-lite), md5 `001d42b44a5f69ed6a0409f26949f12a`.
- `../LeanCoreP2R.tla` (P2 + the same restart and sync, keeping the queue, the
  merge base and the journal), md5 `16b6f7a6b3cfee8daeb62e493186af78`.

Worlds come from `../gen-p1.sh`, and the expectations in `../WORLDS-P1.tsv` were written
before any run. All runs were local (the Mac, 4 workers).

## The result so far

At 1 path, 3 barriers, 1 restart and 1 sync, with the full claims (5 invariants,
Inv_NoRegress, Prop_NoSilentRevert, and for P1-lite Prop_DeleteSettles):

| world | verdict | states |
|---|---|---|
| **P1Holds1p3b** (P1-lite) | HOLDS | 10,058,919 distinct, depth 41 |
| **P2RHolds1p3b** (queue baseline) | breaks Prop_NoSilentRevert in 25 steps: **L-126** (a withheld upload re-published over an acked UI save, silently) | 2,641,713 when found |
| P2RSyncNoPrune (baseline without L-123's prune) | breaks Inv_NoRegress, L-123's own route | 227,786 |
| P1SyncNoRegress (P1-lite, Inv_NoRegress only) | HOLDS | 10,058,919 |

So L-123's class has no route in P1-lite at all (nothing is stored between deriving
what is owed and taking it), and L-126 cannot happen because the merge base IS the
baseline.

The controls and probes all fire as intended: P1WithoutP2, P1DeleteOutranked (M3),
P1CommitBlind, P1VerifyOff, ProbeConverged, ProbeDeleteOverTheirs, and
ProbeRestartAfterCas in both models. The republish-after-restart revert (recorded,
not silent) is reachable in BOTH models at the same depth, so it is not a P1-lite
cost.

## Not finished

- **The 2-path pair without restarts** (`P1Holds2pNoRestart` / `P2RHolds2pNoRestart`):
  stopped at the pause. P1 was at 77.1M distinct states, depth 26, no violation, with
  10.6M still queued.
- **The 2-path pair WITH a restart** (`P1Holds` / `P2RHolds`) outgrows the Mac:
  75M+ states used 21 GB of disk. It is a box world.
- **One clean record.** Some worlds above ran before the last model change: the
  `took` exemption in SilentRevert, which applies to both models. Those worlds do
  not check Prop_NoSilentRevert, so their verdicts stand. Still, re-run all of them
  on the final md5s before quoting.

## Model corrections along the way (each applied to BOTH models)

1. Fetches: a barrier's fetches are taken in one step, and a sync runs BETWEEN
   barriers. The code allows nothing else (`&mut Syncer` and the state dir's flock).
   An asynchronous sync produced a counterexample in both models that no code path
   reaches.
2. Inv_NoRegress exempts a displacement that has a record (as LeanSubtree's
   Inv_ConsumeNeverRegresses does).
3. A re-upload goes to a FRESH copy handle (`upped`, `copies`, `orig`, `Content`),
   as the code's fresh key does. The same-handle re-upload gave false OneName and
   CitationsLive violations after a restart.
4. SilentRevert exempts a version the publishing writer's tree held at the path
   (`took`). A restarted writer's stale baseline otherwise flags its own later edit.
   After relaxing it, P1CommitBlind still fires.

The per-batch tallies are in `RESULTS*.txt`, with traces in `out/`.
