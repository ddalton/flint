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
lossy fold, `FoldCovers` guards the commit, and the mutation
`FoldNoCoverageCheck` reproduces the failure.

## The bug class

The model carried exactly the right invariant and could never have
reached the violation, because `FoldPlan` *defined* the roll-up's
contents to be the union of its inputs. The property under test was
written in as an axiom. This is the fourth time on this project that
the abstraction, rather than the code, was the bug.

The audit below looked for the same shape everywhere else.

## OPEN — F2: the restore's proof is weaker than it claims

**This is worse than the fold defect, and it is not fixed.**

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

The coverage check now stops a bad fold from ever landing, which breaks
this chain at step 1. But the proof still does not prove what it says it
proves, and no invariant in the model would catch a bucket that cannot
be restored while somebody is happily serving from disk. A fix needs
`retained` and `proved` as model variables and a new
`Inv_BrokenBucketIsRefused`.

## OPEN — F3: a third CAS site the model does not know about

`restore::maybe_repack` CASes the snapshot, and it is not an action in
`Next`. It names the **directory** rather than the belief (the shape of
the `FoldCasFromDisk` mutation), and it does **not renew the lease
before its CAS** (the shape of `FoldNoRenew` — the very thing
`fold::commit` carries a twelve-line comment about). It also uses
`repack -a -d`, which drops unreachable objects. Two shipped mutations
in one path, reachable whenever `FLINT_FORGE_FOLD_FACTOR=0` — which is
the comparison drills' own control arm.

## OPEN — F4, F5: smaller

- The mid-fold guard covers the LIST sweep only; `fold::sweep_ledger`
  has no such guard, though the module header claims both sweeps.
- The sweep's etag check is atomic in the model and read-once-then-loop
  in the code. That is the declared grace axiom, but the 3600 s grace
  must outlive a base rebuild's whole-repository upload, and that has
  never been measured.

## Also fixed alongside

`fsck`'s faults go to **stdout**; only chatter goes to stderr. The
refusal reported stderr alone, so runcd's log blamed `notice: HEAD
points to an unborn branch` while thirteen commits were missing.
