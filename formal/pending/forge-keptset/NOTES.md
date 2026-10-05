# OPEN3: the kept-set collector as the code runs it (2026-09-29)

**The gap.** The shipped-baseline gate (24/24, 2026-09-28) checks the
reclaim through `FoldCommit`'s at-rest arm, and that arm always names
the fold's own roll-up `f` beside the kept set. `ForgeSync.tla:1176`
argues that this covers the code's collector "a fortiori". That was
never checked. `restore.rs` `reclaim_inner` builds no pack at all:

1. It reads what the snapshot's refs reach.
2. A greedy drops a named pack when every REACHABLE object in it is
   held by a pack still kept.
3. It asserts that condition over the final set.
4. It renews the lease.
5. It CASes the snapshot to `packs \ drop`.
6. It unlinks the dropped packs. It does not add them to `retained`.

It runs once, between the restore and `Phase::Serving`. It declines
when fewer than two packs are named, or when a named pack is not on
disk with its `.idx`.

**The sandbox.** `ForgeSyncKeptSet.tla` is `ForgeSync.tla` (md5
1c201e9d) with these changes:
- Two new actions. `KeptSetRead` covers the read, the drop set and the
  renewal. `KeptSetCommit` covers the CAS and the unlink.
- With `ReclaimKeptSet`, `FoldPlan` no longer runs in the reclaim
  window.
- Four mutations and three probes.

The drop set is ANY set that passes the code's final check, so a
green covers every order the greedy could take.

**Worlds** (`gen.sh`, from the gated shipped cfgs; expectations in
`WORLDS.tsv`, written before any run):
- `Holds` and `Live`: the claim.
- `AnyDrop` (no coverage test): must fire.
- `VsOriginal` (each drop tested against the original set, the pair
  bug the greedy's comment describes): must fire.
- `WhileServing` (outside the window): must fire.
- `NoRenew` (no renewal before the CAS): must fire.
- The three probes (a commit lands; it drops residue; it drops a pack
  holding a reachable object) must each be violated, or `Holds` is
  vacuous.
- `OffLive` (ReclaimKeptSet off) must reproduce `ForgeSyncLive`'s
  1,781,559 distinct exactly. That shows the sandbox is otherwise the
  gated module.

## Status at the pause (2026-09-30 01:20 UTC)

Module md5 `d20e8651`. The Mac ran the small worlds with a 15-minute
limit each (`RESULTS-mac-2026-09-29.txt`). The box ran the large ones
(`RESULTS-box-2026-09-29.txt`, from `~/forge-keptset-2026-09-29`).

| World | Result |
|---|---|
| OffLive | HOLDS, 1,781,559 distinct: exactly the gated ForgeSyncLive count, so the sandbox is the gated module |
| Live | HOLDS, 1,500,985 |
| **Holds** | **HOLDS, 86,289,224 distinct, depth 62, 83 min** |
| AnyDrop | fires Inv_LandedPackComplete |
| NoRenew | fires Inv_NoStragglerLandAfterRestore |
| WhileServing | fires Inv_LandedPackComplete |
| ProbeCommits | fires: a kept-set commit lands |
| ProbeResidue | fires: it drops a pack holding an unreachable object |
| ProbeCovered | **does NOT fire** — HOLDS, 86,289,224 distinct, depth 62 (EC2 2026-10-05, compiled tlc-rs, 45 s): exactly Holds' count, so the probe never trips |
| VsOriginal | **does NOT fire** — HOLDS, 86,289,224 distinct, depth 62 (EC2 2026-10-05, 42 s): the variant changes no reachable state |

**2026-10-05: both open worlds decided, and both are VACUOUS at these
bounds** (`RESULTS-ec2-2026-10-05.txt`, `out/*-ec2-2026-10-05.out`). Each reached exactly
TLC's Holds count (86,289,224 distinct, depth 62): the probe never fires
and the shrinking-set re-test never matters, because no reachable state
has two named packs holding the same reachable object — the case both
need. The coverage half of the rule is still refuted only by AnyDrop. To
exercise it, a world needs packs that overlap: a second fold, or pushes
that share an object. Both had been "undecided" since 09-29 only because
the checker was slow: tlc-rs evaluated the VIEW in its interpreter for
every state (fixed with compiled VIEW parts; 77K -> 1.9M distinct/s).

What is shown: the code's collector (no roll-up, drop only what the
KEPT packs cover, CAS, unlink) holds on the shipped rules, and it does
collect the residue it exists for. What is NOT yet shown:
- whether the model ever drops a pack for being COVERED (ProbeCovered);
- whether the greedy's re-test against the shrinking kept set is
  load-bearing (VsOriginal).

Both need two named packs holding the same reachable object. Until
ProbeCovered fires, the coverage half of the rule is exercised only by
AnyDrop's refutation.

**Resume.** Read `~/forge-keptset-2026-09-29/RESULTS.txt` on the box.
If the box was powered off before both lines appear, rerun:
`ssh ddalton@10.0.0.249 'cd ~/forge-keptset-2026-09-29 && setsid nohup ./after-holds.sh > run2.log 2>&1 < /dev/null &'`.
run.sh skips decided worlds. These runs use no checkpoint, so a world
that was cut off starts over.

## Committed 2026-10-03: what this records

The box run was cut off when the box was shut down on 2026-09-30.
`RESULTS-box-2026-09-29.txt` is identical to the box's own
`RESULTS.txt`, and it has no ProbeCovered or VsOriginal line: **both
worlds are UNDECIDED, not run to completion.** Their Mac 15-minute
runs also gave no result (`RESULTS-mac-2026-09-29.txt`). The
three-world claim (OffLive, Live and Holds hold, and four of the
must-fire worlds fire) stands as written above. The coverage half of
the rule is still exercised only by AnyDrop's refutation. To finish
it, rerun `ProbeCovered` and `VsOriginal` with `run.sh` (it skips
decided worlds) on a box that can give each world its 6-hour cap.
