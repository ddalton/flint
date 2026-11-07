# Catch-up gate: what remains (handoff, 2026-11-06)

A handoff for picking this up in another environment. The source of truth
is the checklist under *Delivery order* in
`spdk-csi-driver/docs/incremental-replica-rebuild.md` (§12, rev 8). This
file says where each open item stands, what it needs, and which inputs
exist only on the workstation that ran the work.

**The user's gate (2026-10-31):** no work on the raid1 patch, the driver or
the node agent starts until every item in step 1 of *Delivery order* is
done and cited under its md5.

## What is on main

PR #70 was rebase-merged on 2026-11-07 at 01:40Z:

| Commit | What |
|---|---|
| `ac4d4a3a` | FlintCatchup: `Spec => []Inv` under the shipped catch-up; 4,265 obligations proved |
| `4a0beb32` | FlintCatchupT: the per-epoch trust rule in the clean setting; 7,673 obligations proved on a fresh cache, 13 controls. Record: `formal/results/2026-10-29-flintcatchupt/RESULTS.txt` |
| `732f2b5d` | FlintCatchupS: the membership stamp on the final setting; 19,016 obligations proved on a fresh cache. Record: `formal/results/2026-10-31-c31-ec2/RESULTS.txt` |
| `504ef11d` | The four FlintA2Probe cfgs get the 20 constants the FlintReplication tranches added (the PR's TLC gate caught the gap) |

Stamp proof inputs, all on main:

- the proof `formal/FlintCatchupSProof.tla` (md5 `9ed5580a`, generated);
- the inductive invariant `formal/FlintCatchupSInd.tla` (`c70f9660`);
- the model, rev 17: `formal/FlintCatchupS.tla` and the pinned copy
  `formal/sproof/model/FlintCatchupS-r17.tla` (`61ae6829`);
- tlapm `bfa9468`.

It proves `THEOREM Safety == Spec => []Goal` for any finite replica set,
with `Goal` = `Inv_NoNewLoss /\ Inv_MembersHold /\ Inv_RaidHolds /\
Inv_AckStamped`. The setting is `ASSUME Full`: the rev 12 knobs, the
user's 10-31 rules, and crash and zombie moves both on. It is safety only.

The proof file is generated: `formal/gen-FlintCatchupSProof.py` builds it
from `FlintCatchupTProof.tla`, `FlintCatchupT.tla` and
`formal/sproof/*.tla-part`. **The generator hard-codes its directory** as
`~/github/flint-rebuild-proof/formal` (line 6). In another checkout, either
clone to that path or edit that one line. Run with no arguments, it
regenerates the proof in place; a clean export reproduces `9ed5580a`.
`formal/sproof/tlapm-tally.py` tallies a `tlapm.out` per obligation id.
The runner's location-keyed summary undercounts by one, because two
obligations share `135:3:135:20`.

Two steps need Isabelle: 131:5 (MinProp) and 1262:3 (ReadAt). **Isabelle is
broken on the EC2 instances** (0 of 217 attempts, "false" in under a second).
Both steps were proved on the Linux box with its tlapm `bfa9468`
(`/mnt/nvme/tlaps/unpack`), by
`formal/results/2026-10-31-c31-ec2/box-isa/rec/isa-rec.sh`, which asserts
every input md5.

## Open gate items

### Item 2: the stamp proof's controls (needs the user's decision)

The proof is done; the controls are not. A control flips one `ASSUME Full`
line, or removes one rule from the model, and re-runs all 19,016
obligations on a fresh cache. Nothing else changes.

- **KILLED:** a step that uses the flipped line fails. That step is where
  the proof depends on the rule.
- **Not killed:** the proof holds without the rule, so the rule is not
  needed for safety. T's select-time base bound was one of these.
- **What a kill shows:** a failed step shows the proof *uses* the rule
  there, not that the rule is *necessary*. Necessity needs a
  model-checking counterexample.

The candidates, about 27:

- **The stamp's rules (11):** Stamp, RecordUp, MajorityRead, Decree,
  DecreeGated, DecreeSparesLast, RelView, EvenVouch, TornExcluded,
  TornNeverSole, ZombiePreStampLegal.
- **The user's 10-31 rules (5):** WholeMark, InWriteGuard, MonitorGuard,
  DrainStamps, ZombieTriple.
- **T's premises carried into S (6):** CutStampsAll, RaidFailsAtDrop,
  BaseInclusive, PinRetention, StageOracle, LocalLegGuard.
- **T's model rules (5):** SourceTrust, CutGone, BaseBelowTgt, BulkBelowTgt,
  DemoteReverts.

The choice, as put to the user on 11-01:

- all ~27 (~4 h, ~$8);
- only the 16 stamp and 10-31 rules (~2.5 h, ~$5);
- or waive them.

flint-cb's session recorded the user's "go ahead with the proof controls"
(PR #69). flint-e4 asked again in its own session before provisioning; the
user asked what the controls are, got the list above, and has not chosen
yet.

How to run them:

- **Instance:** one pure-spot m7i.48xlarge. One control is the sixteen line
  ranges fb1..fb16 (5 threads each, `--cleanfp --stretch 5`), about 32 min
  wall and ~150 GB; four controls fit at once.
- **Inputs:** every control asserts the md5s of the proof, the Ind and the
  model before tlapm runs, and prints them in its summary. Every input file
  is verified in S3 at its size before `run-instances`. tlapm is pinned by
  sha256.
- **Scripts:** one script, no hand-offs between scripts, no bare `wait`.
- **Counting:** 131:5 and 1262:3 fail on EC2 under every flip. Name them and
  set them aside; don't count them as kills.
- **Output:** a table in the T record's format (per control: the flip, the
  failing lemmas and steps, the per-id counts from `tlapm-tally.py`, wall,
  peak RSS). Add a RESULTS section with the table's md5, the instance id,
  start and end, the teardown and its zero-set check (no instance, volume
  or spot request in us-east-1/2 and us-west-1/2; bucket and policy gone),
  and the cost.
- **Tick:** the item ticks when that table is cited in §12 under its md5.

### Item 3: the three-replica zombie arm (should tick with item 2)

§12 says the stamp proof supersedes this item. The user chose the proof on
10-31, and it covers any replica count, so no acceptance of the depth-27
bound is asked and nothing needs to run.

One loose end: the symmetry check for the three-replica zombie pair was
restated as uniqueness of the read fields and was being rerun. No result is
recorded, and §12 cites no three-replica zombie symmetry result.

### Item 7: resize (needs the user's decision)

The model has fixed-size legs. Two ways to close the item:

- **Model it:** one to two days to build the arm. It needs per-leg size, the
  stamp's tail offset moving with the lvol, the raid following the lvol, the
  two windows, the short leg, and `raid1_resize`'s minimum over bases less
  the reserve. Then hours on the box: 2 replicas full, 3 bounded.
- **Exclude it from the first cut:** the code then refuses to expand a
  stamped volume, and that refusal ships together with the code that calls
  it.

### Item 8: the monitor-skips question

The question needs classing by a rev 10 or later arm with the pre-stamp
write legal; c21's arm neither confirms nor refutes it. That is about an
hour on the box at two replicas, then bounded at three. §12 calls it a
follow-up unless the patch adopts the ungated decree with the monitor
skipping held legs. The patch uses §12's gated decree, so this is the
user's call to keep in the gate or not.

### Item 9: the rest of the model files on main (about half a day)

PR #70 put the proved models and their proofs on main. These are still
missing, and every md5 the §12 table cites must be on main:

- the FlintCatchupS revisions the table cites (rev 7 to rev 12), each md5
  reproduced from its generator or copied from its batch directory, then
  verified;
- the model-checking cfgs, the liveness wrappers, and the model generators
  (`gen-FlintCatchupS*.py`);
- the reachability files;
- the results tables and the scripts that build them;
- no generated logs.

Assemble and verify these, then open a PR at the user's word.

## After the gate

Steps 2 to 5 of *Delivery order*:

1. The raid1 patch with its unit tests, on the box. It now sits on SPDK
   v26.09 (`45ff773c`). The stamp hook `_raid_bdev_remove_base_bdev` and the
   shared RPC/drain path keep their names and callers there; the function
   moved 25 lines.
2. The driver and node-agent changes behind one flag, default off until the
   kind drills pass.
3. The kind drills and the pins.
4. Retire the interim guards in §5's order.

## Follow-ups outside the gate

- **Three-replica liveness**, deferred by the user on 10-31. The arm is
  ready: cfgs `FlintCatchupL17L3M` (`45b953f8`) and `FlintCatchupL17C3M`
  (`c0f8f964`), Reps {1,2,3}, MaxFails 2, `Prop_BarrierEndsM`.
- **A "healthy member" guard on the drain**, for availability, not safety.

## Not in this workstream

The daily `cve-scan` has failed on main since 2026-11-05 on
RUSTSEC-2026-0441 (`pin-utils` 0.1.0, no fixed version) in five lockfiles:
flint-store, forge/syncer, lean/gateway, lean/syncer and spdk-csi-driver.
Releases refuse until it is fixed or accepted with a reason and an
`ignoreUntil` date in `osv-scanner.toml` (`scripts/cve-gate.sh`).

## Inputs that exist only on the original workstation

These are untracked in `~/github/flint-rebuild-proof` on the Mac that ran
the work (~3,400 files; `formal/results` is ~330 MB, mostly generated
output). Until item 9 lands, the controls, item 8 and the liveness arm need
them copied over. Don't commit the generated parts: logs, `*.out` and
tarballs.

- **`formal/`:** 489 untracked files. These are the model-checking cfgs,
  the liveness wrappers, the model generators, and the superseded rev 16
  copy `formal/sproof/model/FlintCatchupS.tla` (`d9062ab6`). The proof
  brief in `formal/sproof/AGENTS.md` still names rev 16.
- **`formal/results/2026-10-31-c31-ec2/`:**
  - the EC2 scaffolding: `PLAN.txt`, `launch.sh`, `userdata.final.sh`,
    `teardown.sh`, `in3/run3.sh` (the S3-inbox job runner),
    `in3/switch.sh`;
  - the sixteen range jobs `jobsrc/fb1`..`fb16`: `job.sh`, the three
    `.tla` files and `cap`. They print the md5s; a control's job must also
    *assert* them;
  - the three-replica liveness arm `jobsrc/lv4`;
  - the tables `tables-c31.md` (`4403d2b7`), `tables-lv.md` and
    `tables-r17.md`, with `mktables-c31.py` (`238a5103`) and
    `mktables.py`.
- **`in3/tlaps.tgz`:** the pinned tlapm build (46 MB, sha256
  `5eaf3aa705b69250afdd53e073ec145c49dfd771d77784a6315fd7d05bad4e44`). It
  is a tarball: copy it, or rebuild from tlapm `bfa9468` and pin the new
  sha.
- **The other results directories:** `2026-10-29-flintcatchupt/`,
  `2026-10-30-c27-ec2/`, `2026-10-31-c33-ec2/` and `2026-10-31-box-probes/`
  hold the tables and batch directories item 9 draws on.
- **On the Linux box (10.0.0.249):** `/mnt/nvme/sproof-2026-10-31` holds the
  box Isabelle runs, and `/mnt/nvme/tlaps/unpack` is the tlapm with a
  working Isabelle.

Provisioning rules for any EC2 run: a plan and a cost approved by the user
first; pure spot; every input verified in S3 before launch; a teardown with
a zero-set check at the end.
