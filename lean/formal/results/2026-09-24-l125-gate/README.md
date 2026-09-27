# L-125 gate, 2026-09-24 (box, 6 workers, -Xmx16g)

The core and refinement worlds re-run after LeanCore gained
`OutrankedRemovalLeavesBaseline` (the L-125 fix in step 7).
Module md5s, identical on the box and locally:
- LeanCore.tla `6532ddded15070912d130a948b3fdf60`
- LeanRefine.tla `5885974b96576bf27071612cfbf2fb42`
- LeanSubtree.tla `d2dd061afb75ee6d937bf22d20d0268b` (unchanged; the L-125
  LeanSubtree patch waits in `pending/`)

17/17 as expected (`RESULTS.txt`, TLC output in `out/`):
- 5 probes FIRE, including the new `ProbeRemovalOutranked` (2.39M distinct),
  so the new arm is reachable.
- 9 known-bad worlds each break the claim they target.
- LeanCoreHoldsSmall HOLDS: 49,030,962 distinct, 13 min 18 s. (Before L-125
  it was 49.0M, so the rule barely changes the state space.)
- LeanRefineQueue HOLDS: 606,916 distinct. The refinement is unchanged,
  because LeanRefine maps the rule to FALSE.
- LeanRefineProbe HOLDS: 1,965,383 distinct, 13 min 39 s.
