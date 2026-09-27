# 2026-09-19 review (H10, H2, H4): the final pipeline

Module `LeanSubtree.tla` md5 `bc67ebd9a449eeb8b393eec8dc13a0ba` (see `module.md5`),
run on the Linux box (8 workers, 12 GiB heap) as `~/lean-gate-2026-09-19/run-final.sh`.

## Two attempts

The first attempt (module `95117cde…`) ran the six deciders as expected and then
the gate stopped in its pre-flight: `view-census.py --selftest` reported that the
UNEDITED module fails the census — `gh.removalOverreach` (H4's stamp, read by
`Inv_RemovalNamesItsVersion`) had been left out of `StrictGh`. An invariant may
not read a field the view drops (only a `Probe*` may), so the two removal worlds'
"holds" from that attempt were under an unsound view. The fix is one line
(`removalOverreach |-> gh.removalOverreach` in `StrictGh`); the module was
pushed again, its checksum compared on both sides, and the pipeline rerun.

## The rerun (this directory)

- Deciders (all as expected): `LeanRemovalOverreaches` violates
  `Inv_RemovalNamesItsVersion` at depth 6; `LeanRemovalHolds` holds, 360,007
  distinct (unchanged from attempt 1: the stamp is FALSE throughout a holding
  world, so adding it to the view splits nothing); `LeanRemovalCrashHolds` holds,
  20,983,079 distinct (unchanged); `LeanBarrierLeaseStragglerLoadsSuccessor`
  violates `Inv_NoStragglerInstall` at 17; `LeanBarrierLeaseAckAfterRestart`
  violates `Inv_AckImpliesCited` at 24; `LeanBarrierLeaseAckCarried` holds,
  4,449,692 distinct (depth 32).
- The gate: **141/141 green** (`gate.txt`), a fresh run (no journal replay).
- Trace validation: ok (`trace-check.txt`).
- The census control arm (`census/summary.txt`), the seven shipped-shape worlds,
  all IDENTICAL to the 2026-09-18 h1f baseline: LeanSubtreeTakeover 566,405;
  LeanEpochOnlyHolds 92,863; LeanBarrierLeaseDeposal 619,637;
  LeanBarrierLeaseSameBytesVerified 2,366,702; LeanBarrierLeaseQueueHolds
  94,858; LeanBarrierLeaseOrphanTracked 2,718,984;
  LeanBarrierLeaseStragglerGCFenced 276,442. The three review arms
  (AckFromCarrier, FenceAfterLoad, RemovalNamesGen) and the view fix moved
  none of them; the removal worlds moved by design (RemovalNamesGen's stamp is
  kept in the view: LeanRemovalHolds 324,499 → 360,007, as recorded).
