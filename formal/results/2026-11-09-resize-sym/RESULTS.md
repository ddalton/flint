# Resize at three replicas: the symmetry wrapper for FlintCatchupSR (gate item 7)

Spec: `formal/FlintCatchupSR.tla` rev 18 (`a202f0f8`), wrapped by
- `FlintCatchupSRSym.tla` (`b2884e17`): `SYMMETRY Perms` over the replica ids and `Inv_ReadUnique`,
  the 2026-11-07 cluster batch's `FlintCatchupSSym` (`9f274f80`) on SR;
- `FlintCatchupSRSymV.tla` (`f7dbaffd`): a VIEW of every variable but the three write-only ghosts
  `vouched`, `tornMarked` and `tornServed`, as `FlintCatchupSSymV` (`35a7ec38`).

Cfgs from `mkcfg.py` (`62ade60c`), which imports the resize batch's knobs, shapes and invariants
(`../2026-11-07-resize/mkcfg.py`): each arm is a 2026-11-07 arm with only the ids (model values),
`Inv_ReadUnique`, `SYMMETRY` and `VIEW` changed. Two-replica runs: tla-mc 0.3.0, `-compile -workers 16`,
on the Mac (`run.sh`; `run.out` has one line per run, logs in `logs/`, gitignored).

## Why the symmetry is sound

- Rev 18 adds no use of replica order. Its per-leg state (`lv`, `cells`, `prv`, `mv`) is functions on
  `R`, its quantifiers range over `R` or `members`, and its `Min` and `Max` are over lvol sizes.
- Order is used where rev 17 uses it: `First`, only with `CoverageWalk = FALSE` (every arm here has it
  `TRUE`), and `NewestS`'s `CHOOSE` among the maximal views. Rev 18's `VAtt` also needs the stamp
  readable at the leg's tail; `Inv_ReadUnique` ranges over that `VAtt` and is checked in every run.
- The VIEW: rev 18 only sets the three ghosts or leaves them `UNCHANGED`, and no resize action, guard
  or invariant reads them. Its own ghosts (`shortAdm`, `unfinished`, `wiped`) stay in the view.
  `ViewVars` is `vars` in its order less exactly those three (checked mechanically).

## Two-replica validation: all 18 runs as expected

Resize on, 2 replicas, 1 failure, MaxVer 1, MaxEpoch 2, MaxAsm 3. Distinct states and depth; "2026-11-07"
is `Res_*_r2f1a3` with natural ids (`../2026-11-07-resize/RESULTS.md`). Every run: no error, and
`Inv_ReadUnique` holds.

| Shape | 2026-11-07 | Ids | Sym | IdsV | SymV |
|---|---:|---:|---:|---:|---:|
| lost | 48,953,028 / 43 | 48,953,028 / 43 | 24,480,770 / 43 | 47,933,656 / 43 | 23,970,987 / 43 |
| crash | 55,993,081 / 44 | 55,993,081 / 44 | 28,001,936 / 44 | 54,543,650 / 44 | 27,277,113 / 44 |
| zombie | 346,834,214 / 47 | 346,834,214 / 47 | 173,421,363 / 47 | 299,009,468 / 46 | 149,508,893 / 46 |

- **Ids** (model-value ids, no symmetry) reproduces the natural-id spaces exactly, generated counts
  too (238,777,563; 303,729,475; 1,723,913,181).
- **Sym** halves each space (two replicas: two permutations) and keeps every verdict and depth.
- **The VIEW** changes no verdict. It takes the zombie arm one level shallower, as it did rev 17's
  (35 to 34 in the 2026-11-07 cluster batch).

The planted bugs and design arms under SymV each fail their intended invariant at the 2026-11-07 depth:

| Arm | Fails | Depth (2026-11-07) |
|---|---|---|
| `Bug_NoReserve` | `Inv_ArrayLeBases` | 3 (3) |
| `Bug_NoFinish` | `Inv_MoveFinished` | 5 (5) |
| `Bug_ShortAdmitted` | `Inv_ShortNeverServes` | 4 (4) |
| `Bug_FsEarly` | `Inv_FsRangeClean` | 5 (5) |
| `Design_NoRetryRun` | `Inv_VacatedZero` | 4 (4) |
| `Design_AckAfterGrow` | `Inv_FsRangeClean` | 6 (6) |

## Three replicas, on the cluster

`R3_SymV_{lost,crash,zombie}_r3f1a3`: the `Res3` arms under SymV (3 replicas, 1 failure, MaxVer 1,
MaxEpoch 2, MaxAsm 3), every invariant of the 2026-11-07 arms plus `Inv_ReadUnique`. tla-mc 0.3.0
`-cluster`, compiled, 40 workers a node, on 37 nodes (vc0140 node 0; `drive.sh`, `drive.out`), capped
at 4 hours each. With natural ids on 2026-11-07 they were bounded only: lost clean through depth 27
(4.51 B), crash through depth 27 (5.79 B), zombie through depth 23 (3.59 B).

| Arm | Verdict | Distinct states | Generated | Depth | Checked in |
|---|---|---:|---:|---:|---:|
| lost | **full pass** | 6,317,146,257 | 42,154,217,149 | 55 | 2,074 s |
| crash | running | | | | |
| zombie | queued | | | | |

- **Lost:** symmetry cuts the depth-22 queue 5.96 times against the natural-id run (35.6 M against
  212 M), close to the 6 permutations. The levels peaked at depth 33 (a 610 M queue) and shrank from
  there to depth 55.
