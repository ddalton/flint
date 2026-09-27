# L-126 evidence, 2026-09-24

- `CoreWithheld3.cfg` / `.out`: the SHIPPED core `LeanCore.tla` (md5 6532ddde…),
  `LeanCoreHoldsSmall`'s claims and rules at one path and THREE barriers
  (MaxRemovals 0). `Prop_NoSilentRevert` is violated at depth 26, 558,944 distinct
  states. Route: the UI saves v2 and B publishes it; A edits v3 from the seed; B's
  commit sweeps A's in-flight upload; A's commit withholds it (parked) and step 7
  still moves A's merge base to v2; A's next barrier publishes v3 over v2 with
  `Foreign` false, so R7 records nothing.
- `P2RSyncPruned-L126-route.out`: where it was first seen, in the P1-lite sandbox's
  queue baseline `pending/p2/LeanCoreP2R.tla`, as an `Inv_NoRegress` violation.
- The code: `a_withheld_upload_republished_next_barrier_surfaces_the_version_it_never_integrated`
  (lean/syncer/src/tests.rs) fails unfixed.
- Every LeanCore world in the gate has MaxBarriers = 2, and this route needs three.
