# Review 2026-09-18, evening — the box's world logs (H1b–H1f)

TLC on the Linux box (8 workers, 12 GB heap, states on the NVMe). Each
`*.out` is one batch's summary; the `.log` beside it is that run's full TLC
output (a violated run's log carries the counterexample). Times are UTC.

The module was still being iterated during the evening, so the batches ran
on different revisions of `LeanSubtree.tla`. What each batch says, and on
which module:

## box-b

- `orphan.out` (22:38) — the first module with `Inv_NoDeleteResurrected` in
  the sweep's world: `OrphanResurrects` violates (22), and `OrphanTracked`
  STILL violates (22) — the no-crash shape of H1b, which the tightened third
  contest and `judged` then closed.
- `h1b.out` (22:52) — H1b's fixes in, before H1c–H1e: `OrphanTracked` holds
  (2,717,456 distinct; three barriers), the probes fire, and the three-barrier
  `OrphanStaleCopy` HOLDS with the identical count (8,003,267 / 2,717,456) —
  the vacuous pair that sent the orphan worlds to four barriers. The same
  batch shows H1c's discovery: `CollectorOff` (20), `ImplHolds` (21) and
  `InboxSnapshot` (23) red on `Inv_NoDeleteResurrected` under the tightened
  contest. `QueueTombstoneOverHitl` (20) is the ASSUME fallout, fixed.
  `LeakResurrects` HOLDS here because the leak rule now subsumes H1's for the
  invariant — the mutation gained `LeakSupersedesNothing=FALSE` (see box-c).
- `LeanBarrierLeaseCollectorOffRedundantEntry.log`,
  `…CollectorOffBaseLag.log` (23:27, 23:34) — the known-bad worlds of the two
  arms that were then DROPPED as redundant: each still violates
  `Inv_NoDeleteResurrected` (22, 20) with the arm off, but the fixed world did
  not need the arm once the tombstone was in.
- `final.out` (23:37–23:51) — the module before H1f (md5
  `9ed161d8daf6044ed4576b7b18f9dd7c`): `CollectorOffNoTombstone` violates
  (21); `CollectorOff` (33,982,392 / 12,447,478, depth 39), `ImplHolds`
  (25,912,345 / 8,936,778, depth 32) and `InboxSnapshot` (28,908,051 /
  9,978,206, depth 35) hold; the four-barrier `OrphanStaleCopy` violates
  `Inv_TreesConverged` (23) — and `OrphanConverges` violates it too, at 30:
  **that trace is H1f**, a superseded tombstone leaving a clean copy of the
  retired generation behind. The batch stopped there, before the gate.
- `gate.txt` (23:19) — a gate on an intermediate module that was killed to
  make room for `final.out`; not a verdict.
- `trace-check.txt` (23:14) — the trace replays on that intermediate module.

## box-c

- `leak.out` (22:57–23:05) — the four-barrier orphan world before
  `Inv_TreesConverged` was refined (its pending-work exception and the
  `instBase[p] = 0` antecedent came after): `LeakResurrects` violates (18)
  with `LeakSupersedesNothing=FALSE`, `LeakRetiredOnly` holds (91,097),
  `LeakHolds` holds (90,457), `ProbeLeakApplied` fires (20), and the
  four-barrier `OrphanStaleCopy` and `OrphanConverges` both HOLD
  (10,494,637 / 10,472,489 distinct) — the counts differ, so the arm was
  reaching states, but the invariant as first written did not see the stale
  copy. The refined invariant's verdicts are in box-b's `final.out`.

The final module (with `SupersedeRestoresBase`, md5
`babdfbf3cfae94c58993559cda64e198`) ran in `../2026-09-18-review-h1f/`.
