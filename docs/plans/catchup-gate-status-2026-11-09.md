# Catch-up gate: status at the end of 2026-11-09 (handoff)

This follows `catchup-gate-status-2026-11-07.md`. Everything below ran on the on-prem cluster
vc01NN.halxg.cloudera.com (user ddalton) or on the Mac. Every result was committed and pushed to `main`
as it finished. The pushes went straight to `main` at the user's word, bypassing its pull-request rule.
The local branch `catchup-gate-cluster-2026-11-07` is even with `main`.

All cluster processes were stopped at 17:43–17:44 PST, for the day. A scan of the 37 nodes in use
found no checker of ours and nothing listening on 27470–27472. On the Mac, no driver, runner or ssh
session to the nodes is left.

## The gate items

| Item | Status | Where |
|---|---|---|
| 2: stamp-proof controls | **done** (2026-11-07); the timeout reruns are still open | `formal/results/2026-11-07-controls/` |
| 3: three-replica zombie arm | **done** (2026-11-07) | `formal/results/2026-11-07-vc-cluster/` |
| 7: resize | the SYMMETRY/VIEW wrapper is validated. Three replicas: lost is a **full pass**, crash is bounded (stopped for the day just past its peak), zombie not run | `formal/results/2026-11-09-resize-sym/` |
| 8: monitor-skips | **done** (2026-11-07) | `formal/results/2026-11-07-vc-cluster/` |
| 9: model files on main | not started | needs the original Mac's untracked files |

### Item 7: the wrapper and the three-replica arms (`formal/results/2026-11-09-resize-sym/`)

**The wrapper.**
- `FlintCatchupSRSym.tla` (`b2884e17`) is Friday's `FlintCatchupSSym` on `FlintCatchupSR` rev 18
  (`a202f0f8`): `SYMMETRY Perms`, plus `Inv_ReadUnique` checked in every run.
- `FlintCatchupSRSymV.tla` (`f7dbaffd`) adds the VIEW: `vars` less `vouched`, `tornMarked` and `tornServed`.
- Rev 18 adds no use of replica order. Order is used only where rev 17 uses it: `First` (with
  `CoverageWalk = FALSE` only) and `NewestS`'s `CHOOSE`, which `Inv_ReadUnique` covers.
- `mkcfg.py` imports the resize batch's knobs, so each arm differs from Friday's only in the ids,
  `Inv_ReadUnique`, `SYMMETRY` and `VIEW`.

**Validation at two replicas (the Mac, 18 runs, all as expected).**
- Model-value ids reproduce Friday's natural-id spaces exactly, generated counts too: lost 48,953,028,
  crash 55,993,081, zombie 346,834,214 distinct.
- SYMMETRY halves each space and keeps every verdict and depth. The VIEW changes no verdict; it takes
  zombie one level shallower (47 to 46), as on rev 17.
- The four planted bugs and two design arms under SYMMETRY and VIEW each fail their intended invariant,
  at Friday's depth.

**Three replicas, 1 failure, MaxVer 1, MaxEpoch 2, MaxAsm 3, SYMMETRY and VIEW, 37 nodes:**

| Arm | Result | Distinct states | Depth | Checked in |
|---|---|---:|---:|---:|
| lost | **full pass** | 6,317,146,257 | 55 | 2,074 s |
| crash | bounded: stopped by hand at 17:43, no violation seen | 5,509,586,077 seen | 35 (open) | 1,562 s |
| zombie | not run | | | |

- **Lost:** symmetry cuts the space close to the 6 permutations (5.96 times at depth 22). The levels
  peaked at depth 33 (a 610 M queue), much earlier than Friday's natural-id growth suggested, and then
  shrank to depth 55. On Friday the same arm was bounded at depth 27.
- **Crash:** the same shape as lost, a little larger. Its levels peaked at depth 33 (a 671 M queue) and
  were shrinking (0.89 times at depth 35) when it was stopped. That was about ten minutes short of its
  likely end, judging by lost's tail from the same point (500 s). A rerun to completion takes about
  40 minutes on 37 nodes.
- **Zombie:** on Friday its levels grew faster than lost's (1.6 times a level at depth 23, against 1.4),
  so expect it to be the largest. It is capped at 4 hours.
- **For §12:** item 7 can be ticked once crash and zombie are full passes too.

## Forge, outside the gate (`formal/results/2026-11-07-forge-rewind/`)

The model is `ForgeSync.tla` (`3f296d75`). Every run below finished with no error.

| World | Distinct states | Generated | Depth |
|---|---:|---:|---:|
| R1S1 Code (identity) | 1,535,961,438 | 8,875,886,270 | 70 |
| R2S1 Code, NoFix, Model | 3,552,867,452 each | 20,817,033,683 each | 71 |
| R1S2 Code, NoFix, Model | 3,759,771,406 each | 23,089,705,814 each | 72 |
| R2S2 Code | 9,901,281,628 | 62,241,783,282 | 73 |

- **Result so far:** at R1S1 (2026-10-05), R2S1 and R1S2, all three retention settings give the same
  space, down to the generated count. Neither the restore fix nor its batch-side twin changes a
  reachable state at those bounds.
- **Still to run:** R2S2 NoFix and Model, and P3R1S1 Code, NoFix and Model. R1S1 NoFix and Model are
  not needed: the 2026-10-05 tlc-rs run already has all three identical.
- **The queue:** a `STOP` file in the directory stops `queue.sh` before its next world; it is there now.

## Infrastructure learned today

- **tla-mc:** `/tmp` was cleared over the weekend. Recreate the worktree with
  `git -C ~/github/tla-mc worktree prune` first, because the stale entry blocks `worktree add`.
- **Nodes:** 37 are usable. vc0114 is still down. vc0131 is left out: 70 GB available, the same tenant
  load as Friday. vc0132 looked healthy today (97 GB, 5 GB of swap) but is still left out.
  vc0101 is the tightest node in use: 28 GB available at the lost arm's peak.
- **Scale:** 37 nodes run 2.7 to 2.8 times faster than 13 (R1S2: 1,820 s against 660 s). Forge
  checks about 5.7 M distinct/s on 37 nodes, the resize arms about 3.0 M/s.
- **Per-batch ports and directories:**

  | Batch | Port | Remote dir | Spill dir |
  |---|---|---|---|
  | catch-up | 27470 | `tla-mc-cluster` | `/var/tmp/ddalton-tlamc` |
  | forge | 27471 | `tla-mc-forge` | `/var/tmp/ddalton-tlamc-forge` |
  | resize | 27472 | `tla-mc-resize` | `/var/tmp/ddalton-tlamc-resize` |

- **Records:** `drive.out` now names each run's nodes. In forge, `queue.sh` writes that line; in the
  resize batch, `drive.sh` does.
- **Watchers:**
  - `pgrep -f` matches the watcher's own shell when the pattern appears in its command. Anchor the
    pattern (`'^bash ./drive.sh'`) or wait on a PID.
  - Don't end a monitor's pipe with `cut`: it buffers, and the events never arrive. `grep --line-buffered`
    on its own works.

## Next, in order

1. **Item 7 at 3 replicas:** rerun crash to completion (about 40 minutes), then zombie (capped at 4
   hours). One command runs both; see *How to resume*. Commit each result as it finishes.
2. **§12:** write item 7: the resize model, its two-replica passes, the wrapper and the three-replica
   arms. Tick it if all three arms pass.
3. **Item 2 timeouts:** unchanged. Rerun, alone and with more time, the steps that fail under more than
   one flip (3639:3 and 7970:22).
4. **Item 9:** unchanged. Recover or regenerate the original Mac's untracked files.
5. **Deferred runs:**
   - forge: R2S2 NoFix and Model, then P3R1S1 ×3 (about 30 minutes each for R2S2; P3 is unknown);
   - four replicas: `cfg-B_Four_zombie_r4f3a3.cfg.deferred`;
   - the no-drain control rerun.

## How to resume

- **tla-mc:** if `/tmp` was cleared, run `git -C ~/github/tla-mc worktree prune && git -C ~/github/tla-mc worktree add /tmp/tla-mc-0.3.0 v0.3.0`.
- **Item 7:** from `formal/results/2026-11-09-resize-sym/`, run
  `CAP=14400 nohup ./drive.sh FlintCatchupSRSymV.tla cfg/R3_SymV_crash_r3f1a3.cfg cfg/R3_SymV_zombie_r3f1a3.cfg > drive.stdout 2>&1 &`.
  The default `HOSTS` are the 37 nodes, with vc0140 as node 0.
- **Forge:** from `formal/results/2026-11-07-forge-rewind/`, run
  `rm -f STOP && HOSTS="$(printf 'vc01%02d ' $(seq 1 13) $(seq 15 30) $(seq 33 40))" CAP=3600 nohup ./queue.sh F_R2S2_NoFix F_R2S2_Model F_P3R1S1_Code F_P3R1S1_NoFix F_P3R1S1_Model > queue.stdout 2>&1 &`.
  Don't run it alongside item 7 on the same nodes.
- **Two-replica runs:** `run.sh NAME...` in the resize batch runs locally on the Mac (16 workers).
- **Before starting:** check every node for a leftover `../checker -cluster` and for a listener on the
  batch's port.
