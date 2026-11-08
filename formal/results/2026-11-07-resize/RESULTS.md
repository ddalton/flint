# Resize in the membership-stamp model: results (gate item 7)

Spec: `formal/FlintCatchupSR.tla` (rev 18 = rev 17 plus `Resize`). Checker: tla-mc 0.3.0,
`-compile -workers 16`, on the Mac. **Nothing was ever started on the cluster nodes in this task**
(vc0115 to vc0118 were never used; I had no processes there). Every run below is local.
Big runs used `-fpmem 12000 -queue-mem 8000` to stay under the macOS file limit (hard limit 1024).
The action table and timing assumptions are in `README.md`; cfgs in `cfg/` (made by `mkcfg.py`),
logs in `logs/`, runner `run.sh`, capped runner `cap.sh`.

## Verdict

- With `Resize = FALSE`, SR is rev 17: distinct states and depth match S exactly in all three
  shapes at both sizes (six pairs).
- With `Resize` on and every Bug_ knob off, the five invariant sets hold over the full space for
  two replicas in all three shapes (lost, crash, zombie) at both bounds. No real finding.
- Four planted bugs each fail the intended invariant; two design alternatives fail as expected.
- Three replicas: **not complete**. Lost was bounded (about 807M distinct states to depth 22, no
  violation seen, killed by the macOS file limit). Crash and zombie were not run to a result.

## Action table

See `README.md` for the full map to code and doc. In short:

| Action | Stands for |
|---|---|
| ExpandStart / ExpandDone | controller expand: belt check, then ack once every leg's move is done |
| GrowLeg / CopyStamp / ZeroOld / ClearPrev | `bdev_lvol_resize` on a leg, then the stamp tail moves with the lvol (`prv` marks the old tail) |
| CrashMove / RetryLeg | the two crash windows and the re-drive |
| RaidResize | `raid1_resize`: min over bases less the reserve, asynchronous |
| FsGrow | NodeExpandVolume |
| StageRec | window-1 record fallback when no leg has a readable stamp |
| RFollow (on Stage and StageAdmit) | create takes the reserve off, finishes the move, aligns the standby, sets the ghosts |
| Fit / GuardOK | leg-size guard (largest cohort, floor) |

## Rev-17 identity (S vs SR, Resize = FALSE)

Same cfg, same constants, on `FlintCatchupS.tla` and `FlintCatchupSR.tla`. Distinct states and
depth are equal in every pair. (Generated counts equal too.) "v1e2a3" and "v2e2a3" are the
cfg suffixes: 2 replicas, MaxVer 1 or 2, 2 epochs, 3 asm.

| Shape | cfg | distinct | depth | S time | SR time |
|---|---|---|---|---|---|
| lost | v1e2a3 | 4,218,568 | 31 | 2.6 s | 2.6 s |
| lost | v2e2a3 | 40,625,350 | 35 | 29.5 s | 30.0 s |
| crash | v1e2a3 | 5,916,705 | 32 | 3.7 s | 4.5 s |
| crash | v2e2a3 | 73,835,786 | 36 | 55.8 s | 61.4 s |
| zombie | v1e2a3 | 26,146,988 | 35 | 16.2 s | 16.8 s |
| zombie | v2e2a3 | 261,440,370 | 39 | 201.1 s | 185.2 s |

The first attempt at zombie v2e2a3 died on both modules with "Too many open files" at about
101M distinct. It was rerun with `-fpmem 12000 -queue-mem 8000` and completed on both. The identity
logs were made before a later edit to `StageRec`'s conjunct order; that action is gated by `Resize`,
so it cannot change a `Resize = FALSE` run.

## Every run

`Res_*` are two replicas, Resize on, all Bug_ knobs off; `r2f1a3` / `r2f1a4` = 2 replicas, 1 fault,
3 / 4 assemblies. All verdicts are "no error" (exit 0) unless stated. The first r2f1a3 lost and crash
runs were made before the `StageRec` reorder (a no-op for those states); lost and crash r2f1a4 and zombie
both were after. Re-running lost and crash r2f1a3 on the final file is cheap (under 3 min) and listed below.

| cfg | distinct | generated | depth | verdict | time |
|---|---|---|---|---|---|
| Res_lost_r2f1a3 | 48,953,028 | 238,777,563 | 43 | pass | 68 s |
| Res_crash_r2f1a3 | 55,993,081 | 303,729,475 | 44 | pass | 85 s |
| Res_zombie_r2f1a3 | 346,834,214 | 1,723,913,181 | 47 | pass | 332 s |
| Res_lost_r2f1a4 | 77,314,120 | 374,771,179 | 45 | pass | 159 s |
| Res_crash_r2f1a4 | 89,929,179 | 484,452,790 | 46 | pass | 198 s |
| Res_zombie_r2f1a4 | 802,188,676 | 3,990,717,815 | 49 | pass | 1,112 s |
| Res3_lost_r3f1a3 | 807,199,864 at stop | 3,325,705,085 | 22 (open) | **bounded**: no violation seen; exit 2, "fingerprint file: Too many open files" | 908 s |
| Res3_crash_r3f1a3 | 176,884,299 at last progress | 738,176,444 | 19 (open) | **not run to a result**: killed (exit 143) at coordinator's request | 160 s |
| Res3_zombie_r3f1a3 | none | none | none | **not run**: killed during build | n/a |
| Bug_NoReserve | 127 | 179 | 3 | FAILS Inv_ArrayLeBases | build-dominated |
| Bug_NoFinish | 548 | 852 | 5 | FAILS Inv_MoveFinished | " |
| Bug_ShortAdmitted | 359 | 542 | 4 | FAILS Inv_ShortNeverServes | " |
| Bug_FsEarly | 530 | 817 | 5 | FAILS Inv_FsRangeClean | " |
| Design_NoRetryRun | 385 | 591 | 4 | FAILS Inv_VacatedZero | " |
| Design_AckAfterGrow | 2,147 | 3,960 | 6 | FAILS Inv_FsRangeClean | " |

Wall for each Bug_/Design_ run was 19 to 20 s, almost all of it the spec build (checking itself is
under 0.03 s). The Res3_lost depth-22 level was not finished when the process died, so the claim is
"no violation in the roughly 807M states seen, depth 22 open", not a pass. Res3_crash progress lines
(not a verdict) reached depth 19 with no violation.

## Planted bugs: traces, each confirmed as the intended bug

State lines below are the abbreviated traces read from the logs (leg 1, leg 2; lv = lvol size, cells = positions
holding stamp bytes).

1. **Bug_NoReserve** (`Reserve = FALSE`; expect Inv_ArrayLeBases): ExpandStart; GrowLeg(1) (lv = <<3,2>>);
   RaidResize with no reserve taken off gives rsz = 2, but leg 2 (not grown) has lv - 1 = 1: the array range
   covers leg 2's stamp tail. Intended bug. (An earlier version of this cfg fired at depth 1 because RaidResize
   and Create did not require a grown leg or take the reserve; fixed in the model, see Modeling note 2.)
2. **Bug_NoFinish** (`FinishAtCreate = FALSE`; expect Inv_MoveFinished): ExpandStart; GrowLeg(1); CopyStamp(1)
   (prv = 2, cells {2,3}); the array drops, then a create with leg 1 as the only member leaves prv = 2 set
   (unfinished = TRUE). The assembly trusted the next retry instead of finishing the move. Intended bug.
3. **Bug_ShortAdmitted** (`SizeGuard = FALSE`; expect Inv_ShortNeverServes): ExpandStart; GrowLeg(1)
   (lv = <<3,2>>); the assembly admits both legs, leg 2 short: shortAdm = TRUE. Intended bug.
4. **Bug_FsEarly** (`FsAfterCtl = FALSE`; expect Inv_FsRangeClean or Inv_FsLegsFit): ExpandStart; GrowLeg(1)
   (cells {2,3}, old tail still holding stamp bytes, no move); an assembly of leg 1 with leg 2 stale and the
   array resized (rsz = 2); FsGrow (fsz = 2) before the controller ack. The fs now covers position 2, which still
   holds stamp bytes: Inv_FsRangeClean. Inv_FsLegsFit did not fire first because the short leg was stale, not in
   sync. This is the intended bug (fs grown early over the un-moved stamp). The cfg expect line was widened to
   "Inv_FsRangeClean or Inv_FsLegsFit" for this reason.

## Design alternatives (fail by design)

- **Design_NoRetryRun** (`RetryRuns = FALSE`; Inv_VacatedZero): GrowLeg(2); CrashMove(2) (mv = 5, window 1);
  RetryLeg(2) returns a same-size no-op success (mv = 4) with nothing copied or zeroed, so a leg marked done has
  cells {2} below its tail 3. A retry that treats "already grown" as success never completes the move. This is a
  check of the doc's rule that the retry re-runs the move; it is not a finding on the code.
- **Design_AckAfterGrow** (`MoveBeforeAck = FALSE`; Inv_FsRangeClean): both legs grown (mv = 1, not moved);
  RaidResize; the controller acks on "grown"; FsGrow covers position 2 with stamp bytes still at the old tail.
  Acking before the move is done is unsafe.

## Findings with every Bug_ off

None. The correct model holds all invariants over the full two-replica space in all three shapes
at both bounds. The 3-replica lost arm found nothing in its bounded part.

Modeling notes (how the model was corrected while writing; none is a code finding):
1. `StageRec` first read `head'` (through `NewAsm`) before the `UNCHANGED head` conjunct and the checker stopped
   with "head' is read before the action assigns it"; fixed by ordering. Modeling.
2. The first Bug_NoReserve fired at depth 1 to 2 because RaidResize and Create used the reserve with no grown
   leg. RaidResize now needs a grown leg, and Create always takes the 1-cluster reserve (MinSz). Modeling.

## Not covered, honestly

- Three replicas: Res3_lost only bounded (about 807M states, depth 22 open); Res3_crash and Res3_zombie have no
  result. The target of a bounded 3-replica pass in each shape is not met.
- The hot-rejoin refusal (guard 2): rev 17 has no live base add.
- One grow only, one block at position 2, one data cluster; no shrink, no second grow.
- Atomic move steps, atomic stamp copy (snapshot copy assumed: the barrier-before-copy race is not modelled),
  atomic create. Asynchronous raid resize, but only after a grow.
- The short-leg and floor guards are mostly postconditions: ExpandDone needs every leg grown, so a short leg with
  the fs grown is unreachable in the correct model. Bug_ShortAdmitted shows the guard bites at assembly, not that
  a real short-with-fs state arises.
- The driver-RPC move path for a leg under no raid (not yet in code) is one mechanism with the raid path.
- Window-1 junk at the old tail carries no `prv`; a create in that window could leave junk in the array range,
  cleaned only by the RPC re-drive. No probe invariant (a "CreateRangeClean") was written, so this
  possibility is neither confirmed nor refuted here.
- Safety only. No liveness.
- Not run on the Mac and not run elsewhere: the three Res3 cfgs, after the coordinator stopped Mac use.

## Still to run (on the cluster, by the coordinator)

All of `cfg/`, spec `FlintCatchupSR.tla`, safety only:
- `Res3_lost_r3f1a3` (to completion; the Mac stopped at about 807M distinct, depth 22)
- `Res3_crash_r3f1a3`
- `Res3_zombie_r3f1a3`

Optional: re-run `Res_lost_r2f1a3` and `Res_crash_r2f1a3` on the final file (the earlier runs preceded the
`StageRec` reorder, which does not change them), and write and run a create-time junk probe (see above).
