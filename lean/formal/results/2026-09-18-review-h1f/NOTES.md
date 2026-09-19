# Review 2026-09-18, H1f and the final gate — the box's logs

Module `LeanSubtree.tla` md5 `c4208d3ad9344ecec355da78b6fdf988` (the tree's),
except pass 1, which ran `babdfbf3cfae94c58993559cda64e198` — the same
actions, before `Inv_TreesConverged`'s exception was narrowed and `tSuper`
factored into `ObjectSupersedes`. TLC on the Linux box, 8 workers, 12 GB
heap, states on the NVMe. Times are UTC, 2026-09-19.

Five passes of deciding worlds, each stopping at the first unexpected
verdict (`run-h1f*.sh`, `h1f-pass*.out`, `h1f.out` = pass 5). A later pass
overwrote an earlier pass's `.log` of the same world; each log is the LAST
run of that world.

- **pass 1** (00:05–00:24, module `babdfbf3`): the H1f arm. `SupersedeDropsBase`
  violates `Inv_TreesConverged` (30), `ProbeBaseRestored` fires (22),
  `OrphanConverges` HOLDS (10,475,529 distinct, depth 43) — the arm closes
  H1f. `CollectorOffNoTombstone` violates (20); `CollectorOff` (12,447,478
  distinct, 33,982,392 generated, depth 39), `ImplHolds` (8,936,778 /
  25,912,345, depth 32), `InboxSnapshot` (9,978,206 / 28,908,051, depth 35)
  hold — identical counts to the module before H1f: the arm reaches no
  supersede in those worlds. UNEXPECTED: `OrphanStaleCopy` (the shipped leak
  rule) HELD — with the base restored, the leak rule no longer leaves a
  settled stale copy; it re-queues the deletion at every install and the
  leak supersedes it again, and the invariant's "a queued deletion is
  pending work" excused the flap.
- **pass 2** (00:32–00:34): the exception narrowed (`DeletionPending`).
  `OrphanStaleCopy` violates (21) again, `SupersedeDropsBase` (30),
  `ProbeLeakApplied`/`ProbeBaseRestored` fire, `LeakRetiredOnly`
  (90,781) and `LeakHolds` (90,249) hold, `OrphanConverges` holds
  (10,475,529). UNEXPECTED: `LeakResurrects` (H1's known-bad world) HELD
  (95,185) — a later rule closes H1's resurrection on its own.
- **pass 3** (00:43): a first re-cut of the leak worlds. `LeakResurrects`
  with the restore off too STILL held (100,241) — it was the H1e tombstone
  (`ManifestTombstones`) that closed it, not only the restore;
  `LeakRetiredOnly` under `Inv_TreesConverged` violated (23) — H1f's own
  route, which only the restore closes; `LeakRetiredConverges` (the
  retired-etag rule with the leak rule off, restore on) violated (24) — a
  writer that never installed the generation the collector left holds a
  tombstone naming an older one, so the retired rule cannot recognise the
  leak. The cut was wrong; `LeanBarrierLeaseLeakRetiredConverges.log` is
  this pass's (the world was renamed `…LeakSkippedGeneration`).
- **pass 4** (00:46–00:47, then the gate 00:47–01:25): the final leak
  worlds. `LeakResurrects` with all FOUR rules off violates (19);
  `LeakHolds` 90,249, `LeakRetiredOnly` 91,097, `LeakRestoreOnly` 95,185,
  `LeakRuleConverges` 90,249 hold; `LeakFlaps` violates (19),
  `LeakSkippedGeneration` violates (24), `ProbeTombstoneOverLeak` fires
  (14). The gate then ran 136 runs and had ONE green mutation
  (`gate-pass4-one-green-mutation.txt`): `OrphanResurrects` — H1b's
  known-bad world — HELD (2,461,472 distinct): the tombstone closes H1b's
  resurrection route as well. The crash arms it started were killed.
- **pass 5** (01:28, then the gate 01:29–02:20): `OrphanResurrects` with
  the tombstone off violates (23); **the gate: 136/136 green** (`gate.txt`,
  no journal replay), the trace replays ok (`trace-check.txt`); the crash
  world at both `OrphanTrack` arms started at 02:20
  (`../2026-09-18-crash1-orphantrack/` when done).

`census/` — the worlds the gate prints no counts for (it keeps only a
failing run's output), run beside pass 5's gate with 2 workers and a 3 GB
heap (`census-small.sh`, `census-small.out`): the five queue-free worlds
unchanged from the morning (Takeover 566,405; EpochOnlyHolds 92,863;
Deposal 619,637; SameBytesVerified 2,366,702; RemovalHolds 324,499),
StragglerGCFenced 276,442 unchanged, QueueHolds 94,846 → 94,858,
OrphanTracked 2,717,456 → 2,718,984.
