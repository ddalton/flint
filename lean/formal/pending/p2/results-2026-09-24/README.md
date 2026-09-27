# P2 sandbox, run 2 — 2026-09-24, box `ddalton@10.0.0.249`

Model `../LeanCoreP2.tla`, md5 `c82cbb811a316222a2fd57495cbe03e5` (checked on both sides).
Every world matched its expectation. Ignore the first line of `RESULTS.txt`
("P2LiveHolds MISMATCH rc=1"): it is left over from run 1, whose output dir was moved mid-run.

| | shipped core `LeanCoreHoldsSmall` | P2 `P2Holds` |
|---|---|---|
| bounds | 2 paths, Free {p2}, A B, MaxMint 3, MaxUI 1, MaxRemovals 1, MaxBarriers 2 (+ MaxSeq 3, a no-op) | the same, without MaxSeq |
| distinct states / depth | 48,995,156 / 34 | 11,642,977 / 34 (about 4.2x fewer) |
| model lines | 908 | 562 |
| variables | 19 | 18 (the inbox's entries, saw, removals and refused are gone; gw, mv and udel are new) |
| design rules that must be TRUE | 11 | 7 safety + 1 liveness (GatewayIgnoresLease) |
| checked claims | 5 invariants + Prop_NoSilentRevert | the same 6 + Prop_UISaveCompletes (G1, under LSpec) |

Each of P2's 8 rules has a world that turns it off, and each of those worlds breaks. P2ForeignFlat was run as
"record the verdict"; it breaks Prop_NoSilentRevert, so ForeignPerPath still matters.

**The claim changed.** Inv_AckedNamed also accepts a version the human deleted and was told was deleted
(`udel`). Under P2 no writer's tree performs a UI delete, so the shipped core's accounting could not count it.
Run 1 violated the invariant along that route (UI saves v2, UI deletes it, a writer creates the path again).

**Not covered.**
- The 3-path rename world.
- G2 (writer retry cost under heavy saving), which has to be measured rather than modelled.
- The code: this is a sandbox model, and nothing in lean/ implements P2.
