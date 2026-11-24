# The catch-up models: files, revisions, and how to reproduce them

These are the models behind §5 (the catch-up and its trust rule) and §12 (the membership
stamp) of `spdk-csi-driver/docs/incremental-replica-rebuild.md`, and the record that §12's
*Delivery order* cites by md5. §12's gate item 9 asked for all of them on main; this file maps
them. Every md5 below is the first 8 hex digits of the file's md5.

## The models

| Model | File | md5 | What it is |
|---|---|---|---|
| FlintCatchup | `FlintCatchup.tla`, `FlintCatchupProof.tla` | | §5's catch-up at block-content level; proved for every bound (TLAPS). In the CI gate. |
| FlintCatchupT | `FlintCatchupT.tla` | `8ac3cf00` | rev 15: the per-epoch trust rule, stage-only. `FlintCatchupTInd.tla` and `FlintCatchupTProof.tla` prove it (all 7,673 obligations). |
| FlintCatchupS | `FlintCatchupS.tla` | `61ae6829` | rev 17: the membership stamp, derived from T rev 15 by `gen-FlintCatchupS.py`. `FlintCatchupSInd.tla` (`c70f9660`) and `FlintCatchupSProof.tla` (`9ed5580a`, built by `gen-FlintCatchupSProof.py` from `sproof/`) prove it (all 19,016 obligations). `sproof/model/FlintCatchupS-r17.tla` is the same file, pinned. |
| FlintCatchupSR | `FlintCatchupSR.tla` | `a202f0f8` | rev 18: rev 17 plus resize (gate item 7). Its batches are `results/2026-11-07-resize/` and `results/2026-11-09-resize-sym/`. |

### FlintCatchupS revisions

Each revision is derived from `FlintCatchupT.tla` rev 15 by its own generator. The generators
are kept exactly as they ran; `check-catchup-gen.sh` reruns all of them in a scratch `HOME` and
checks every output byte for byte (it never writes into the repo).

| Rev | md5 | Model generator | Where the as-run copy is | Batches |
|---|---|---|---|---|
| 7 | `6859c997` | none kept | `results/2026-10-29-flintcatchupt/as-run/ms7/` | c21 |
| 8 | `ab167ffe` | `gen-FlintCatchupS-r8.py` | `as-run/ms8/` | c22 |
| 9 | `c41c6986` | `-r9.py` | `as-run/ms9/` | c23 |
| 10 | `2a9567c7` | `-r10.py` | `as-run/ms10/` | c24 |
| 11 | `46c89207` | `-r11.py` | `as-run/ms11/` | c25 |
| 12 | `8e54566d` | `-r12.py` | `as-run/ms12/` | c26, c27, c28, c30, c31 |
| 13 | `41fa0652` | `-r13.py` | `as-run/ms13/` | c29 (symmetry) |
| 14 | `231150e6` | `-r14.py` | `results/2026-10-31-box-probes/ms14/` | q14 (premise probes) |
| 15 | `3d409f1b` | `-r15.py` | `results/2026-10-31-c33-ec2/jobsrc/pk1/` | d15, peels |
| 16 | `d9062ab6` | `-r16.py` | `sproof/model/FlintCatchupS.tla` (the proof's first target; `sproof/AGENTS.md` still names it) | pk3, sp* |
| 17 | `61ae6829` | `gen-FlintCatchupS.py` | `FlintCatchupS.tla` | c31 (X*, lv*), the proof, items 3 and 8 |

The cfg generator is cumulative: `gen-FlintCatchupS-cfg.py` writes the cfgs of every batch
through rev 17 (249 files), and `gen-FlintCatchupS-cfg-r8.py` to `-cfg-r16.py` are its earlier
states, kept as they ran. Rev 7's cfgs and rev 8's ten reachability and identity cfgs have no
generator; they are only in `as-run/`.

### Wrappers and side modules

| File | md5 | Purpose |
|---|---|---|
| `FlintCatchupSReach.tla`, `-r10.tla`, `-r11.tla` | `2e192540` | reachability probes (no HOLD is vacuous), revs 10 to 12 |
| `FlintCatchupSReach-r9.tla` | `01df6860` | the same for rev 9; rev 7's (`7d5d053b`) and rev 8's (`3e9e01e2`) are only in `as-run/` |
| `FlintCatchupSSym.tla` | `f08c5d33` | SYMMETRY wrapper, c29's line (`as-run/ms13/` has the c29 copy `34243d06`; items 3 and 8 used `results/2026-11-07-vc-cluster/FlintCatchupSSym.tla` `9f274f80`) |
| `FlintCatchupSLiveW.tla` | `752b1a56` | liveness controls: `SpecNoReturn`, `Prop_PathReturns` (c30b) |
| `results/2026-10-29-flintcatchupt/c30/FlintCatchupSLiveW.tla` | `b719dbbf` | c30's wrapper: the file above without `Prop_PathReturns`, the one difference the c28 table records. Rebuilt 2026-11-23 from `752b1a56`; it hashes to the cited md5. |
| `FlintCatchupSLiveW2.tla` | `6604c794` | rev 17 liveness restated: `Prop_BarrierEndsM`, `Prop_BarrierEndsH` |
| `FlintCatchupTAnc.tla`, `TLive`, `TReach`, `TReachD`, `TReachQ`, `TReachZ`, `TProbeP`, `TProbeQ`, `TTieNA` | `bb5db0b8`, `22cbd165`, `6e8eeae8`, `75d2f8f9`, `24f65ad2`, `7d1cd03a`, `a22b4bc5`, `52358a02`, `780c1009` | T's ancestry, liveness, reachability, probe and tie wrappers |

### Cfgs

The 455 `FlintCatchup*.cfg` added to `formal/` with this file are not in the CI gate
(`scripts/check-tla.sh` names every run it makes). 249 are the current S cfg generator's output
(every batch's, through rev 17). The other 206 are FlintCatchupT's cfgs. Each cfg's header names its arm, and each
results table names the cfg and the model md5 of every row. The older revisions' cfgs sit beside
their models in `as-run/msN/`, which has an `MD5SUMS` for all 567 files.

## The results batches

| Directory | What ran | Tables and their scripts |
|---|---|---|
| `results/2026-10-29-flintcatchupt/` | the box batches c16 to c30 (T and S revs 7 to 13); `as-run/` holds every S model and cfg as run | `tables-c16.md` ... `tables-c28.md`, `vtable.py`, `RESULTS.txt` |
| `results/2026-10-30-c27-ec2/` | c27 on EC2: the first full three-replica passes (rev 12) | `tables-c27.md`, `PLAN.txt` and the launch, userdata and teardown scripts |
| `results/2026-10-31-c31-ec2/` | c31 on EC2: gate items 5 and 6, the proof's 16 ranges (`jobsrc/fb1`..`fb16`), the liveness arms (`jobsrc/lv3`, `lv4`), the proof agents' jobs; `box-isa/rec/` the two Isabelle steps | `tables-c31.md` (`mktables-c31.py`), `tables-lv.md`, `tables-r17.md` (`mktables.py`), `RESULTS.txt` |
| `results/2026-10-31-c33-ec2/` | c33 and c33b on EC2: rev 15 and 16 probes, invariant peels, the proof agents' jobs (`sjob*.sh` packed them) | `tables-d15.md`, `PLAN.txt` |
| `results/2026-10-31-box-probes/` | rev 13 symmetry (`ms13`), rev 14 premise probes (`ms14`), Ind v3 peel (`msp3`) | `tables-q14.md` |
| `results/2026-11-07-controls/` | gate item 2: the proof's 27 controls | `RESULTS.txt`, `controls-table.txt`, `tables.json`, `diffs/` |
| `results/2026-11-07-vc-cluster/`, `2026-11-07-resize/`, `2026-11-09-resize-sym/` | gate items 3, 7 and 8 | each has its own `RESULTS.md` or tables |
| `results/2026-11-23-catchup-open-runs/` | the runs still open, for one large machine | `README.md` |

**Deduplicated inputs.** The EC2 job directories (`jobsrc/`, `stage/`) held hundreds of
byte-identical copies of the same model, proof and cfg. Each is kept once. The batch's
`INPUTS.tsv` lists every input as it ran: its md5, its path, and where the kept copy is.
`results/restore-inputs.sh BATCH --check` verifies the list, and
`results/restore-inputs.sh BATCH OUT_DIR` rebuilds the batch's full input tree outside the repo,
checking every md5.

## Not in the repo

- **Generated output:** logs, `*.out`, traces, `box-snap-*/` and the `out/` and `jobs/`
  directories. The tables were built from them, so the table scripts can't run without them.
  They stay on the workstation that ran the batches (`~/github/flint-rebuild-proof`,
  untracked). Each batch's `.gitignore` keeps them out.
- **Tarballs:** the job bundles and `in3/tlaps.tgz` (the c31 tlapm build, sha256 `5eaf3aa7…`,
  since lost). The 2026-11-07 controls rebuilt tlapm `bfa9468` from source (sha256
  `1453bd21…`); `results/2026-11-07-controls/RESULTS.txt` §1 has the recipe.
- **88 intermediate drafts of the stamp proof** from the proof agents' jobs: generated by
  `gen-FlintCatchupSProof.py` from the `sproof/` parts of their day, and cited nowhere. They are
  `absent:` rows in `INPUTS.tsv`, md5 included. The final proof is `FlintCatchupSProof.tla`;
  `jobsrc/fa1/FlintCatchupSProof.tla` (`482059a8`, the generator's previous output) is kept,
  because §12 cites it as the cross-check.
- **Generated checker sources:** c31 asserted that each instance compiled the same Rust as the
  box (`in/gensrc.expect`: `5900f94c`, `ccac9e0e` and others, plus the dependency set
  `c5f4db1d`). tla-mc 0.2.2 `-codegen` regenerates them.
- **Two files the doc cites only in its narrative, never in a results table:**
  FlintCatchupT rev 13 (`2dbea4a0`, §5's "What the rule rests on") and FlintCatchupS rev 4
  (`d32f4f23`, the first decree trace in §12). They aren't on the workstation and aren't in
  git history. If the Linux box still has them, this finds them:
  `find /mnt/nvme -name 'FlintCatchup[ST].tla' -exec md5sum {} + | grep -E '^(2dbea4a0|d32f4f23)'`.
  Copy any hit to `results/2026-10-29-flintcatchupt/` with a line in its `RESULTS.txt`.

## Reproducing

```sh
formal/check-catchup-gen.sh                                        # every S revision and generated cfg, byte for byte
formal/results/restore-inputs.sh formal/results/2026-10-31-c31-ec2 --check
(cd formal/results/2026-10-29-flintcatchupt/as-run && md5sum -c MD5SUMS)
```

Checked on 2026-11-23: every revision from 8 to 17 and all 1,118 generated cfgs reproduce. All
567 `as-run` files match `MD5SUMS`. All three `INPUTS.tsv` lists check, and a full restore of
c31's jobs matches the original 237 model files byte for byte (the other 36 are proof drafts).
Running the check on a copy with one generator changed by one byte fails it.
