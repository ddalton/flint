# The fold that lost objects, and what the audit found behind it

2026-09-07. Written after cluster runcd's cold-start leg wedged a
repository, and after auditing `formal/ForgeSync.tla` for the bug class
that let it through.

## What happened

A fold produced a rolled-up pack that did not hold everything its
inputs held, and its commit stopped naming those inputs anyway. The
snapshot then named 55 refs and 3 packs, with 13 of those refs needing
commits that lived only in packs it no longer named. The next cold
restore could not prove the repository, and the syncer refused to serve
it — correctly — and crash-looped.

Nothing was lost. Every missing object was still in the bucket, in the
pack that originally carried it. The batch log named the culprit
exactly: `packs_added` at seq 77, `packs_removed` at seq 127.

**Root cause (found afterwards, in the code).** A batch names every
pack in the directory, not the packs of the pushes it judged; and git
migrates a push's pack out of quarantine as soon as `pre-receive`
passes, before the blocking `proc-receive` hook runs. So under
concurrent pushers a queued push's pack is named one batch before its
ref moves. The batch log shows it: batch 77 named 29 packs and moved 19
refs; batch 78 moved 13 refs and named 2. A base rebuild planned in
that gap took those packs as inputs; its `--all` could not reach commits
no ref pointed at yet; its commit unnamed their only packs. The
thirteen missing commits are the thirteen refs of batch 78.

**Fixed, twice over:** a base rebuild's commit now keeps named any input
pack holding an object that became reachable after the rebuild read its
refs (the task records those tips; the commit asks git what arrived
since). And `fold::run_task` checks coverage from the pack indexes
before it uploads, with the two kinds contracted differently — a tier
fold must hold every object its inputs hold; a base rebuild may drop
objects but never a reachable one. `ForgeSync.tla` may now produce a
lossy fold, and `FoldCovers` guards the commit.

**Correction, 2026-09-07 (later the same day).** This paragraph ended
"and the mutation `FoldNoCoverageCheck` reproduces the failure". It does
not, and never did. Run to completion for the first time — on the
current model AND on HEAD's, the one this sentence was written against
— it is exhaustively CLEAN: 35.5M distinct states, 0 left on queue,
depth 57, 40 minutes, no error. It is a structural no-op. The constant
gates only the precondition `(FoldNoCoverageCheck \/ FoldCovers(s))`,
while the packs the commit actually unnames are filtered independently
by `D == {q \in S : holds[q] \subseteq holds[f]}`. With that filter
standing, nothing uncovered is ever unnamed, so nothing can strand
however the precondition is set.

**The two halves of the fix are not equal, and this is the finding.**
The commit's FILTER — keep named any input holding an object that became
reachable after the rebuild read its refs — is load-bearing by itself.
The pre-upload check in `fold::run_task` is a belt over braces that
already hold. Remove the filter instead (`FoldSupersedesArrivals=TRUE`,
`D == S`) and runcd's exact loss appears in **72 seconds**:
`Inv_LandedPackComplete`, a landed push's objects held by nothing.

That mutation had a cfg and **no run in `scripts/check-tla.sh` at all**
until now, so the gate has never exercised the failure this whole
tranche was built around, while the doc credited one that cannot fail.
Both are fixed: `FoldSupersedesArrivals` is a required-fail run, and
`FoldNoCoverageCheck` is recorded there as a documented non-run with the
reason.

It is the same shape as F4 and as the fold defect itself: a claim about
the model, believed because it was plausible, never run to the point
where it could have been refuted. Three of the four vacuous or stale
gate items found today had been green — or absent — for as long as they
had existed.

## The bug class

The model carried exactly the right invariant and could never have
reached the violation, because `FoldPlan` *defined* the roll-up's
contents to be the union of its inputs. The property under test was
written in as an axiom. This is the fourth time on this project that
the abstraction, rather than the code, was the bug.

The audit below looked for the same shape everywhere else.

## F2: the restore's proof was weaker than it claimed — CODE FIXED, MODEL OPEN

**Fixed in the code 2026-09-07.** A proof is now taken over the packs
the snapshot NAMES, never the object directory: `fsck` and the
incremental `rev-list` run against a scratch object directory holding
hardlinks to the named packs alone (`ScopedOdb`, `gitcmd.rs`), and the
incremental arm lapses when a pack the last proof used stops being
named rather than when its file goes. A fold therefore does cost one
full proof, exactly as `follow::prove`'s doc comment had always said it
did. `FOLLOW_VERSION` went to 2: the field's meaning changed, and a
version-1 file would have parsed as a claim it never made.

**A test had pinned the defect in place**, which is why the audit found
it by reading and not by running. `the_proof_lapses_when_the_files_it_
walked_are_unlinked_not_when_the_pack_list_moves` asserted the wrong
behaviour and its comment defended it: the first draft had expected a
fold to cost a full proof, the draft was corrected to match the code,
and the comment recorded the correction as a finding. Every clause in
it was true and the conclusion was wrong. It is now
`a_fold_costs_a_full_proof_because_what_is_proved_is_the_bucket`, with
the retained files asserted to be still on disk so the leg cannot pass
for the old reason.

**Still open: the model half.** `ForgeSync.tla` has no retention at
all — `FoldCommit` drops the superseded inputs from `localPacks` in the
same step that unnames them, so the state in which a proof could be
taken over a retained pack does not exist in the model. Closing that
needs `FoldCommit` to keep the inputs on disk, an `UnlinkRetained`
action, a `provedOver` variable and a `Checkpoint` action, and two
invariants: `Inv_ProofIsOfTheBucket` (violated by a `ProveFromDisk`
mutation alone — the discipline) and `Inv_ProvedIsRestorable` (violated
only by `ProveFromDisk` AND `FoldNoCoverageCheck` together — which is
the useful part: it says in the model what is true in the code, that
the coverage check is the single thing standing between this defect and
data loss). Sized: ~31 actions' `UNCHANGED` lists, 18 configs,
`localPacks` semantics in 47 places. Not a footnote.

### What it was

The model's `Restore` asserts the proof covers only the packs the
snapshot names. The code proves over the raw object directory, and a
fold's superseded inputs are deliberately *retained on disk* for
`fold_retain_secs` (900 s). So after a bad fold:

1. the roll-up lands, its inputs are unnamed but still on disk;
2. `fsck --connectivity-only` reads the directory, finds the objects in
   the retained packs, and PASSES;
3. the server serves normally for the whole retention window;
4. `unlink_retained` removes them from disk;
5. the ledger sweep deletes them from the **bucket**;
6. the next restart cannot restore, and now nothing can.

runcd was a near miss on this chain: the pod happened to be deleted
while the objects were still in the bucket. Had the sweep run first, the
data would have been gone.

`follow::prove`'s incremental arm has the same shape: it holds
*because* the retained inputs are on disk, and returns `Proof::Nothing`
— which contradicts its own doc comment claiming a fold costs one full
proof. `follow::checkpoint` then records the retained packs, and the
unverified roll-up, as proved.

The coverage check stops a bad fold from ever landing, which breaks this
chain at step 1 — so F2 was latent, not live, and that is the only
reason runcd was a near miss rather than a loss. The proof now proves
what it says it proves, so the chain is broken at step 2 as well.

## F3: a third CAS site the model did not know about — CLOSED by deletion

`restore::maybe_repack` CASed the snapshot and was not an action in
`Next`. It named the **directory** rather than the belief (the shape of
the `FoldCasFromDisk` mutation), it did **not renew the lease before its
CAS** (the shape of `FoldNoRenew` — the very thing `fold::commit`
carries a twelve-line comment about), and its `repack -a -d` dropped
unreachable objects with no coverage check and unlinked the superseded
packs with no retention window. Three shipped mutation shapes in one
path.

**Deleted 2026-09-07**, with `Git::repack` and `repack_threshold`.
Renewing the lease would have closed one of the three; closing all
three means re-deriving `fold::commit`, and the base rebuild already
IS that — `pack-objects --all --indexed-objects --write-bitmap-index`
produces the same artefact as `repack -a -d -b` and carries the
coverage check, the renewal, the retention window and the ledger. So
the weaker duplicate went rather than being hardened. This was already
the plan of record: the compaction-tiers design's phase 4 is
"measurement, then the control's removal", and the measurements it was
gated on had been taken.

**Two corrections to this entry as first written.** It was not two
shipped mutations but three defects, the third being the missing
coverage check and retention. And "reachable whenever
`FLINT_FORGE_FOLD_FACTOR=0` — the comparison drills' own control arm"
overstated the reach: `fold_factor` defaults to 2, and of the five e2e
rigs that set it to 0, four also set `REPACK_THRESHOLD` to 100,000 or
more, so `maybe_repack` could never fire in them — they used factor 0
to mean "no compaction", which is now all it means. One rig actually
exercised it: `forge/e2e/repack/run-repack.sh`, whose full-repack arms
are now refused rather than silently run at factor 0, where they would
report the no-compaction floor under the control's name.

## F4: the ledger sweep's missing mid-fold guard — CLOSED, UNFOUNDED

As written this entry was wrong twice over, and both halves are worth
recording because the shape is the one this document is about: an
inference about the code, stated as a finding, never checked against it.

**"The module header claims both sweeps" — no header does.** There is no
such claim in `sweep.rs`'s header or `fold.rs`'s. The entry asserted a
contradiction between doc and code where the doc says nothing.

**The guard is not missing; it does not apply.** `sweep.rs`'s
`sc.fold.is_some()` guard (`:47`, `:64`) exists for two hazards that are
specific to the LIST sweep: `abort_orphaned_uploads` would abort the
fold's OWN multipart upload, which runs on the fold's task and is
therefore genuinely in flight; and the listing sweep would see the
fold's uploaded-but-not-yet-named roll-up as unreferenced and delete it.
`fold::sweep_ledger` does neither. It touches no uploads, never lists,
and deletes only by exact key from entries written at PAST commits — a
set that cannot contain an in-flight fold's artefacts, because the
ledger is appended at step 7 of the commit and nowhere else
(`fold.rs:868` is its only writer).

It also carries three protections of its own, any one of which defeats
the only scenario that survives scrutiny — a content-addressed base
rebuild reproducing a pack name that is sitting in the ledger awaiting
deletion:

1. the etag check (`fresh.etag != sc.cell()?.etag` ⇒ return), so a pass
   never runs against a snapshot that moved under it;
2. the reference set, which is the fresh snapshot's stems UNIONED with
   the undo points' (X15) — a stem either names is skipped;
3. **Rule 2, the age at the delete by the STORE's clock** — a
   re-uploaded pack has a fresh `last_modified` and is not old enough to
   go, whatever the ledger entry's own age says.

Protection 2 is pinned by a test that already existed:
`tests.rs:3417`, "the named pack is never swept".

Adding the guard anyway would have cost something real: a base rebuild
runs for minutes on a large repository, and gating the ledger sweep on
it defers every bucket deletion for that whole window, for no safety
gained — and there would have been no failing case to pin it with.

## F5 — MEASURED by M6 on `runcg`, 2026-09-07; NARROWED, not closed

- The sweep's etag check is atomic in the model and read-once-then-loop
  in the code. That is the declared grace axiom, but the 3600 s grace
  must outlive a base rebuild's whole-repository upload, and that has
  never been measured. Bundled into the M6 drill
  (`forge-log-first-verdict-2026-09-07.md` §5), which needs the same
  rig: a base rebuild inside the measurement window on a repository big
  enough for the upload to take real time.

### What M6 delivered

M6 ran that rig, and the arm carrying the shipped floor did a
whole-repository base rebuild inside EVERY tiny-push leg — the event F5
needed and could not previously get on demand. The uploads are per-leg
resident-byte deltas under the arm's prefix; each leg's rebuild began
and finished inside its own window:

| leg | repository | uploaded | leg wall | implied rate |
|---|---:|---:|---:|---:|
| P2 pair 1 | ~384 MiB | 385.4 MiB | 69 s | >= 5.6 MiB/s |
| P2 pair 2 | ~768 MiB | 769.8 MiB | 61 s | >= 12.6 MiB/s |
| P2 pair 3 | ~1152 MiB | 1153.9 MiB | 62 s | **>= 18.6 MiB/s** |

The rate is a LOWER bound twice over: the rebuild's duration is not
isolated from the leg's, and the leg concurrently served 591-684
acknowledged pushes, so the rebuild had strictly less than the whole
window. At >= 18.6 MiB/s the 3600 s grace covers a **~65 GiB (70 GB)**
repository before the sweep could reach a pack a reader still holds.

**So the grace is not tight at the sizes forge is built for, and F5's
worry does not reproduce at 1.15 GB.** Three things stop that closing
the item outright, and they are the residual:

1. **One node type, one region, one run** — `i4i.xlarge` on `runcg`,
   `us-west-1`. No rep-to-rep spread; the third pair is a single
   number, and this project has twice had an effect dissolve on a third
   rep.
2. **Linearity is assumed, not measured.** `pack-objects` is not
   guaranteed linear from 1 GB to 65 GB, and this extrapolates over
   nearly two orders of magnitude. walgit's large repository carries a
   32 GB base, so the regime is reachable.
3. **It measures the pre-`7202c2b5` planner.** That fix makes
   cap-triggered rebuilds rarer, which only helps this bound — fewer
   rebuilds, same grace — but the number was taken before it.

**What would close it:** one leg on a repository of tens of GB with a
base rebuild inside the window, timing the rebuild directly rather than
bounding it by the leg. That is a big-repository drill and is NOT worth
a cluster on its own — bundle it with the next run that needs a large
repository anyway.

**What this replaces:** "that has never been measured." It has now, at
1.15 GB, with the limits above stated.

## Also fixed alongside

`fsck`'s faults go to **stdout**; only chatter goes to stderr. The
refusal reported stderr alone, so runcd's log blamed `notice: HEAD
points to an unborn branch` while thirteen commits were missing.
