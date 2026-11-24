# Catch-up gate: the open runs, on one large machine

Through 2026-11-09 the gate's large runs ran on a 37-node cluster that only the original Mac
could reach (`../2026-11-09-resize-sym/drive.sh`, `../2026-11-07-vc-cluster/drive.sh`). This
directory runs the same arms on **one** large machine. The checker is the same (tla-mc 0.3.0,
compiled), and so are the cfgs and every input md5. Only the parallelism differs: one machine's
cores in place of `-cluster`.

What's open (§12 of `spdk-csi-driver/docs/incremental-replica-rebuild.md`, *Delivery order*):

| Run | Why | Gate? |
|---|---|---|
| `item7-crash`, `item7-zombie` | item 7 (resize) at three replicas. Item 7 ticks when both are full passes. `lost` passed in full on 2026-11-09 (6,317,146,257 distinct, depth 55); `crash` was stopped about 10 minutes short of its end; `zombie` hasn't run | **yes** |
| `tlapm-rerun.sh` | item 2's loose end: the steps that failed under more than one control (3639:3, 7970:22) were never rerun alone, so a kill could be a solver timeout under load | loose end of a ticked item |
| `live3-lost`, `live3-crash` | three-replica liveness (`Prop_BarrierEndsM`), deferred by the user on 2026-10-31. Two replicas passed in full | no |
| `four-zombie` | four replicas, three failures, rev 17 zombie, SYMMETRY and VIEW, deferred on 2026-11-07 | no |
| `nodrain-ctl` | item 8's no-drain control: clean to depth 26 (3.16 B distinct, natural ids), then out of disk | no |
| `smoke-bug` | the runner's positive control: a planted resize bug that must fail `Inv_ArrayLeBases` at depth 3 | run it first |

## 1. Set up

```sh
git clone https://github.com/ddalton/flint && cd flint     # main, once the model-files PR is merged
# Rust 1.88+ and a C toolchain (for -compile), then the checker the batches pinned (v0.3.0 = c8fce23):
cargo install --locked --git https://github.com/ddalton/tla-mc --tag v0.3.0
tla-mc -version                                             # must print: tla-mc 0.3.0
formal/check-catchup-gen.sh                                 # the models and cfgs reproduce: "all reproduced"
cd formal/results/2026-11-23-catchup-open-runs
./run.sh list                                               # each run's cfg, spec and modules, with md5s
COMPILE=0 ./run.sh smoke-bug                                # must end: VIOL:Inv_ArrayLeBases (as-expected ...)
```

`run.sh` copies each run's cfg and modules from `formal/` into `work/NAME/` and asserts every md5
against the one the tables cite before tla-mc starts. Every run writes three lines to `runs.out`:
the host, the tla-mc version, every input md5 and the full command, then a `DONE` line with the
verdict, its expected value, the distinct count and the depth. Logs go to `logs/NAME.log`.
`work/`, `logs/`, `meta/` and `runs.out` are gitignored.

## 2. Size the machine before starting

tla-mc keeps 8 bytes of fingerprint per distinct state: in RAM up to `-fpmem`, then on disk,
at about 1.3 bytes of RAM per state on disk. Each state also writes a 16-byte record to a trace
log on disk, and a BFS level spills to disk past `-queue-mem`. Every run here is a safety check
except `live3-*`, which also keep a behaviour graph on disk.

| Run | Expected size | Basis |
|---|---|---|
| `item7-crash` | a little over `lost`'s 6.32 B | 5,509,586,077 by depth 35 with its levels shrinking (0.89 times a level); the 2026-11-09 note puts it about ten minutes short of its end on the cluster, by `lost`'s tail |
| `item7-zombie` | the largest; perhaps 2 to 3 times `lost` | on 2026-11-07 (natural ids) its levels grew 1.6 times a level at depth 23, against `lost`'s 1.4 |
| `nodrain-ctl` | over 3.2 B; no symmetry | stopped at depth 26 by a full disk |
| `four-zombie` | unknown, likely the largest of all | never started |
| `live3-*` | unknown on rev 17; at the same bounds rev 12's safety spaces were 1.77 B (lost) and 2.33 B (crash) | c31's XAck and XCAck |

- **Memory:** with `FPMEM` at 8 MB per million expected states, the seen set stays in RAM (7 B
  states need 56 GB). The defaults are a quarter of RAM for `FPMEM` and an eighth for `QMEM`.
- **Disk:** put `META` on the largest NVMe. Plan on at least 1 TB free for the item 7 arms and
  more for `item7-zombie`. On the cluster, `crash` peaked at a 671 M-state level. Watch `df`
  during the first hour.
- **Speed:** on the cluster (37 nodes, 40 workers each) the resize arms ran at about 3.0 M
  distinct states per second. On one 192-core machine tla-mc has done about 1.4 M/s on a
  flint lean model (tla-mc's README). Read the real rate off the first ten minutes of progress lines and set
  `CAP` from it. A capped run is BOUNDED, not a pass.

## 3. Run

```sh
# the gate first, in this order; a full pass of both ticks item 7
META=/nvme/catchup-meta nohup ./run.sh item7-crash item7-zombie > run.stdout 2>&1 &
tail -f logs/item7-crash.log runs.out

# afterwards, optional, one at a time
META=/nvme/catchup-meta nohup ./run.sh live3-lost live3-crash > run2.stdout 2>&1 &
META=/nvme/catchup-meta nohup ./run.sh nodrain-ctl > run3.stdout 2>&1 &
META=/nvme/catchup-meta nohup ./run.sh four-zombie > run4.stdout 2>&1 &
```

- **Stop and resume:** checkpoints are written every `CKPT` minutes (default 30) under
  `META/NAME`. After a stop, run `RECOVER=1 META=... ./run.sh NAME`. tla-mc refuses to recover if
  the spec or cfg changed.
- **Don't run two arms at once.** Each assumes the whole machine.
- **Knobs:** `WORKERS`, `FPMEM`, `QMEM` (MB), `META`, `CKPT`, `CAP` (seconds), `COMPILE=0`
  (interpreted; only for small runs), and extra tla-mc flags after `--`.
- **Reading a result:** `DONE item7-crash HOLD (as-expected; expect HOLD) ... | N distinct states
  found. Depth D | ... No error has been found` is a full pass. `VIOL:<Inv>` on a run that
  expects HOLD is a finding: keep the log (it has the trace) and stop there. `BOUNDED` is a
  capped run: state the depth reached, not a pass. `ERROR` means read the log.

## 4. Item 2: the tlapm reruns

Build tlapm at commit `bfa9468b64b782be05458b7f41e3732d99d9a50b` as the controls did. The
recipe is in `../2026-11-07-controls/RESULTS.txt` §1: an opam 5.1.0 switch, `lsp` and `jsonrpc`
pinned to 1.24.0, then `make release`. The two steps here are not Isabelle steps (only 131:5 and
1262:3 are), so Isabelle's path condition doesn't matter for them.

```sh
./tlapm-rerun.sh BASE 3639 7970          # the unflipped proof: both steps must prove
./tlapm-rerun.sh K-MonitorGuard 3639
./tlapm-rerun.sh K-StageOracle 3639
./tlapm-rerun.sh K-Stamp 7970
./tlapm-rerun.sh K-TornExcluded 7970
./tlapm-rerun.sh K-TornNeverSole 7970
```

Each run is one line of the proof, on a fresh cache, at `STRETCH=20`; the record and the
controls used 5. The script asserts the three input md5s (`9ed5580a`, `c70f9660`, `61ae6829`),
applies the control's diff from `../2026-11-07-controls/diffs/`, and asserts that the flipped
proof's md5 is the one the controls table recorded. `DRY=1` prepares and asserts without running
tlapm.

- **`NOT PROVED` under the flip:** the kill stands at that step. If you want to be thorough,
  try `STRETCH=60` once.
- **`PROVED` under the flip:** the step's failure on 2026-11-07 was a timeout under load. The
  control then counts as killed only by its other steps. Rerun those with
  `./tlapm-rerun.sh CONTROL all`. K-MonitorGuard would rest on 3983:47 alone, K-TornNeverSole
  on 2386 and 8796.

## 5. Record the results

- Write `RESULTS.md` here from `runs.out`: the host, its cores and RAM, the tla-mc and tlapm
  versions, each run's input md5s, verdict, distinct count, depth and wall time. Don't commit
  the logs.
- **Item 7:** when `item7-crash` and `item7-zombie` are both full passes, write item 7 into §12
  and tick it. Cover the resize model (rev 18, `a202f0f8`), its two-replica passes
  (`../2026-11-07-resize/RESULTS.md`), the SYMMETRY/VIEW wrapper
  (`../2026-11-09-resize-sym/RESULTS.md`) and the three three-replica arms.
- **Item 2:** add the reruns to §12's item 2 note (it names the two steps as open).
- When every gate item is ticked, §12 *Delivery order* step 2 (the raid1 patch) may start.
