# Catch-up gate: status at the end of 2026-11-07 (handoff for Monday)

This follows `catchup-gate-remaining-2026-11-06.md`. Everything below ran
on the on-prem cluster vc01NN.halxg.cloudera.com (user ddalton), not EC2.
Items 2, 3 and 8 are written into §12 (the five `vc` rows of the batch
table and the three ticks under *Delivery order*) on the branch
`catchup-gate-cluster-2026-11-07`, together with the controls, the
cluster batch and the resize model. The forge batch is not committed.

All cluster processes were stopped at 16:41–16:43 PST. A scan of
vc0101–vc0140 found no checker, tlapm, prover or driver of ours left.

## The gate items

| Item | Status | Where |
|---|---|---|
| 2: stamp-proof controls | **done**: 27 controls, 23 killed, 4 not killed | `formal/results/2026-11-07-controls/` |
| 3: three-replica zombie arm | **done**: a full pass, symmetry and VIEW | `formal/results/2026-11-07-vc-cluster/` |
| 7: resize | modelled (rev 18). Two replicas: full passes. Three replicas: bounded only | `formal/FlintCatchupSR.tla`, `formal/results/2026-11-07-resize/` |
| 8: monitor-skips | **done**: the skip does not replace the decree's gate | `formal/results/2026-11-07-vc-cluster/` |
| 9: model files on main | not started | needs the original Mac's untracked files |

### Item 2: the controls (`formal/results/2026-11-07-controls/`)

- **Inputs:** asserted by md5 before every job: proof `9ed5580a`, Ind `c70f9660`, model `61ae6829`.
- **tlapm:** `bfa9468`, built from source on vc0112.
  - Tarball sha256 `1453bd21…f4ec8aa526`, a new pin: the original `5eaf3aa7…` tarball was not available.
  - `lsp` and `jsonrpc` are pinned to 1.24.0.
  - tlapm's own test suite never ran green, because Rocky 9 has no `/usr/bin/time`.
- **Isabelle:** it works, once the Isabelle tree sits at the build host's absolute path on every node (`isa-tree` sha256 `114daa03…`). The install is not relocatable: its heaps hard-code the build tree's paths. That is very likely why EC2 failed ("false" in under a second).
- **BASE:** 5,432 proved, 13,584 trivial, 0 failed, 19,016 in all. That is the c31 record plus the two Isabelle steps, 131:5 and 1262:3, now proved.
- **Table:** `controls-table.txt`, md5 `2cb881b2`; `RESULTS.txt`, md5 `8e71cb5a`.
- **Not killed:** `K-Decree`, `K-RecordUp`, `K-EvenVouch` and `M-BaseBelowTgt`. `M-BaseBelowTgt` repeats the T record's result for the select-time bound.
- **Killed:** the other 23, with each failing lemma and step named by line.
- **Caveats for §12:**
  - A "failed" step was not checked for being a timeout under load. Rerun the steps that fail under more than one flip (3639:3 and 7970:22) alone, on an idle node, with more time.
  - fb12 peaked at 59–87 GB here, against 13.6 GB on EC2.
  - The wall times are not comparable with EC2: the nodes are shared.

### Items 3 and 8, with the bounded arms (`formal/results/2026-11-07-vc-cluster/`)

The batch's files:
- `mkcfg.py` writes every cfg.
- `drive.sh` is the queue driver; `drive.out` holds one line per run.
- `FlintCatchupSSym.tla` (`9f274f80`) adds SYMMETRY over model-value ids and `Inv_ReadUnique`: every maximal view the read could choose agrees on seq, gen, size and members.
- `FlintCatchupSSymV.tla` (`35a7ec38`) adds a VIEW that leaves out the three write-only ghosts `vouched`, `tornMarked` and `tornServed`.

The model is rev 17, `61ae6829`, under the proof's `ASSUME Full`, checked with tla-mc 0.3.0 `-cluster`, compiled. The sizes are mine: one block, `MaxVer` 1, `MaxEpoch` 2, `Retain` 1, `MaxAsm` 3. They are **not** c27's constants: a calibration against the §12 counts found no match.

**Full passes at 3 replicas, 2 failures (SYMMETRY):**

| Arm | Distinct states | Depth | Wall | Invariants |
|---|---:|---:|---:|---|
| lost mark | 1,287,740,129 | 45 | 404 s | the four safety invariants, `Inv_ReadUnique` |
| crash | 1,894,073,451 | 46 | 557 s | the same |
| zombie move (+ VIEW) | 10,548,632,461 | 49 | 4,092 s | the same |
| expiry form (+ VIEW) | 12,527,517,379 | 49 | 4,388 s | `Inv_LossMarked`, `Inv_TornJunkMarked`, `Inv_AckStamped`, `Inv_ReadUnique` |

**Symmetry checks at 2 replicas:**
- Model-value ids reproduce the natural-id counts exactly (zombie: 26,146,988).
- SYMMETRY keeps every verdict and depth.
- The VIEW changes no verdict. It saves about 20% on the zombie and full shapes and 2–4% elsewhere.
- c29's strict form, `Inv_ViewUnique`, fails at depth 6 at 3 replicas. The two maximal views there differ only in the released view, which is why the invariant is restated as `Inv_ReadUnique`.

**Item 8: 3 replicas, 2 failures, zombie shape, natural ids, all bounded except the two violations:**

| Arm | Result |
|---|---|
| Ungated decree, monitor marking (control) | **VIOLATED** `Inv_NoNewLoss` at depth 11 |
| Ungated decree, monitor skipping held legs | **VIOLATED** `Inv_NoNewLoss` at depth 11, by another leg |
| Skip, no drain | clean through depth 30 (4.62 B) |
| No-drain control (ungated, marking) | clean through depth 26 (3.16 B); stopped by a full `/home`, rerun deferred |
| Gated decree with the skip | clean through depth 23 (2.28 B) |
| Final setting | clean through depth 23 (2.44 B) |

The traces: one leg fails out and is held, the record still marks it in_sync, and an acknowledged write lands on the other two. The drain then takes the second leg, the zombie move comes during the hold, and the ungated decree marks z stale. The only in_sync leg left lacks the write. So the gated decree of §12 is required.

### Item 7: resize (`formal/FlintCatchupSR.tla`, `a202f0f8`; `formal/results/2026-11-07-resize/`)

- **The model:** rev 18 = rev 17 plus `Resize`. `README.md` maps the actions to the code; `RESULTS.md` (`b5ac1ae1`) has every run.
- **Rev 17 identity:** with Resize off, six pairs match rev 17 exactly, up to 261,440,370 states.
- **Resize on, 2 replicas:** full passes in every shape at `MaxAsm` 3 and 4, up to 802,188,676 states. The lost-mark and crash arms were rerun on the cluster on the final file and match.
- **Planted bugs:** all four fail their intended invariant.
- **Design alternatives:** both turn out unsafe. `Design_NoRetryRun` fails `Inv_VacatedZero`; `Design_AckAfterGrow` fails `Inv_FsRangeClean`.
- **3 replicas, 1 failure, no symmetry, bounded at the cap:**
  - lost mark: clean through depth 27 (4.51 B);
  - crash: clean through depth 27 (5.79 B);
  - zombie: clean through depth 23 (3.59 B), stopped for the day.

  All three were still growing about 1.4× a level.
- **Not covered:** the hot-rejoin refusal, a second grow, a shrink, more than one block, and the driver-RPC move path. Also no probe for a create in the first window leaving junk in the array's range.

## Forge, outside the gate (`formal/results/2026-11-07-forge-rewind/`)

The model is `ForgeSync.tla` (`3f296d75`). Each world is `ForgeSyncCodeRewind.cfg` with only its bounds or its two retention rules changed (`mkcfg.py`). Every run below finished with no error.

| World | Distinct states | Depth |
|---|---:|---:|
| R1S1 Code (identity) | 1,535,961,438 | 70 |
| R2S1 Code: two rewinds | 3,552,867,452 | 71 |
| R2S1 NoFix: two rewinds | 3,552,867,452 | 71 |
| R1S2 Code: two re-pushes | 3,759,771,406 | 72 |

- **Identity:** R1S1 matches the 10-05 record exactly, so tla-mc reproduces tlc-rs on this spec.
- **Two rewinds:** Code and NoFix are the same space down to the generated count, 20,817,033,683. So the restore fix changes no reachable state at two rewinds either.

## Infrastructure learned today

- **Nodes:** 39 are reachable, and vc0114 is down. vc0131 and vc0132 are left out of the search: their other tenants leave too little memory, and vc0132 swapped.
- **Spill directory:** `/home` is 37 GB per node, so the spill goes to `/var/tmp`, which has 592 GB.
- **Guards:** the drivers stop a run when `/var` has under 100 GB free or any node has under 8 GB of memory available.
- **Memory:** tla-mc 0.3.0 `-cluster` overshoots `-queue-mem` and `-fpmem` by about 30 GB per node on wide levels. Use `-queue-mem 8000 -fpmem 8000`.
- **Hung sessions:** after a cluster run, the Mac's ssh sessions to the peers can outlive their checkers. `drive.sh` releases them a minute after node 0's verdict.
- **Busy checks:** `top -bn1`'s first frame is not a current CPU reading. Use `top -bn2 -d1`.
- **ssh:** only the Mac can reach the nodes. Moving the orchestration to a node needs a key on every node, and the user said to leave it on the Mac for now.

## Monday, in order

1. **§12:** items 2, 3 and 8 are written and ticked (2026-11-07 evening). Still to write: the resize model with its 2-replica passes (item 7, not ticked: three replicas are bounded only).
2. **Item 2 timeouts:** rerun, alone and with more time, the steps that fail under more than one flip, so a kill can't be a solver timeout.
3. **Item 7 at 3 replicas:** add a SYMMETRY/VIEW wrapper for `FlintCatchupSR` (validated at 2 replicas the same way), then rerun the 3-replica resize arms.
4. **Item 9:** recover the original Mac's untracked files (`~/github/flint-rebuild-proof`), or decide which to regenerate, then open the PR at the user's word.
5. **Deferred runs:**
   - four replicas: `cfg-B_Four_zombie_r4f3a3.cfg.deferred`;
   - the no-drain control rerun;
   - forge R1S2 NoFix (`cfg-F_R1S2_NoFix.cfg.deferred`), R2S1 Model, R2S2 and P3R1S1.

## How to resume

- **tla-mc:** 0.3.0 source in `/tmp/tla-mc-0.3.0` (a worktree of `~/github/tla-mc` at `v0.3.0`; recreate with `git -C ~/github/tla-mc worktree add /tmp/tla-mc-0.3.0 v0.3.0` if `/tmp` was cleared). Node 0 needs cargo and gcc: vc0140 has both for the catch-up batch, and vc0101 for forge.
- **Catch-up batch:** from `formal/results/2026-11-07-vc-cluster/`, run `CAP=<s> nohup ./drive.sh <SPEC>.tla cfg/<X>.cfg ... &`, then watch `drive.out`.
- **Forge:** `formal/results/2026-11-07-forge-rewind/drive.sh` is the same, on vc0101–vc0113, port 27471.
- **Controls:** `formal/results/2026-11-07-controls/driver.py` resumes from the nodes' state. The tlapm install and the Isabelle tree are still on vc0101–vc0113 under `~/flintcg`.
- **gitignore:** `logs/`, `run/` and the generated logs are gitignored in each batch directory. Check `formal/results/2026-11-07-resize/` before committing.
