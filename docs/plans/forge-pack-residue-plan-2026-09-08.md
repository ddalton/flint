# Reducing the pack-pinning residue at its source — PLAN REJECTED

**Status: both proposed changes are refuted. Nothing here should be
built.** The document is kept because the refutations are the useful
part, and because two of the errors came from comments already in the
tree.

Companion to `docs/plans/forge-pack-pinning-2026-09-08.md` (the finding
and the refutation of "direction 1").

## What still stands

- **The diagnosis.** Reproduced on git 2.50.1 with positive controls: a
  `pre-receive` that exits non-zero leaves the repository byte-identical
  (packs unchanged, zero loose objects, no `tmp_objdir`, `cat-file -e`
  on the refused commit exits 1); flip only the hook's exit to 0 and
  every oracle flips with it. A push refused later by `proc-receive`
  finds `GIT_QUARANTINE_PATH` already unset and its pack already in
  `objects/pack/`, where it stays as a dangling commit. Migration
  happens between the two hooks, exactly as the finding says.
- **The narrow safety claim behind change 2** — declining a fold INPUT
  cannot unname a pack. `superseded` derives only from `res.inputs`
  (`fold.rs:716`) and only shrinks; it is the sole driver of
  `next.packs.retain` (`:826`), `Retained` (`:867`) and `LedgerEntry`
  (`:874`). There is a stronger proof than the plan gave:
  `ForgeSync.tla:894` already plans `\E S \in SUBSET belief[s].packs`,
  so TLC has already explored every input subset; restricting that
  nondeterminism cannot break a safety invariant it already searched.

## Change 1 — REJECTED: it catches none of the class it was written for

1. **Wrong class.** `judge()` tests staleness BEFORE ancestry and
   returns early (`batch.rs:563-567`, then `:570-576`), and F14's arm A
   never force-pushes — every edit is a commit on its own clone — so
   `old` is always an ancestor of `new`. Arm A's 28 refusals were
   `stale info: fetch first`, not non-fast-forward. A fast-forward
   predicate passes every one of them. The repo already recorded this
   (`forge-sharded-single-writer-2026-09-07.md:83`: *"of 30 concurrent
   pushes to one ref, 1 wins and 29 get `stale info: fetch first`"*).
2. **The plan's `<old>` premise was false.** `<old>` on `pre-receive`'s
   stdin is the CLIENT's claim, not the server's ref — shown with two
   ordinary `git push` commands racing, where the loser's hook saw
   `old=03c63092` while the ref stood at `81f3df84`. Trusting it is a
   false-ACCEPTANCE bug in exactly the concurrency case forge is for:
   `is_ancestor(claimed, new)` says fast-forward, `is_ancestor(server,
   new)` says otherwise, the push is waved through and git rejects it at
   the ref lock — in the probe, leaving a 262 MB pack. A check here must
   `git rev-parse --verify "$ref"` and never trust stdin.
3. **Near-zero value on the target workload.** forge's decided merge
   surface is `refs/for`, which change 1 EXEMPTS. Arm D (the
   recommended shape) refused 0 of 20; arm A (the shape forge tells
   fleets not to use) refused 28. The residue that matters comes from
   arm E — `refs/for` conflicts, 15 of 20 BY DESIGN — and change 1 does
   nothing for it. The file API is a second bypass: it calls
   `run_batch` directly (`server.rs:397,464`) and never runs the hook.
4. `receive.denyNonFastForwards` is NOT a cheaper substitute — measured,
   it also leaves the pack behind, because git's own deny happens at the
   same late point as the ref lock.

## Change 2 — REJECTED: it disarms the guard it sits in front of, and does not fire on real content

1. **It would make the coverage rule's only executable controls
   vacuous.** `a_base_commit_may_not_unname_a_pack_whose_push_lands_after_it`
   and `a_pack_named_before_its_ref_moves_...` both assert
   `plan.inputs().contains(&qpack)`, where `qpack` is precisely a pack
   no ref reaches — change 2's own predicate. After change 2 those
   assertions fail; after "fixing" them the tests PASS WITH THE COVERAGE
   BLOCK DELETED, because the dead pack is no longer a supersede
   candidate. That is `A || B` with the other arm open, against the one
   rule this whole finding says must never be relaxed. `ForgeSync`'s
   reachable-coverage cfg is a MUTATION, not a check on shipped code, so
   nothing in the tree would catch a later relaxation.
2. **The predicate is false on real pushes.** `gitcmd.rs` does not set
   `receive.fixThin`, so git's default applies and `index-pack
   --fix-thin` appends the delta bases the server already holds — which
   are reachable. Measured, one dimension moved: a one-line edit to a
   4,000-line file leaves a 4-object residue pack of which **1 object is
   reachable** (a 10,382-byte live blob, `chain length = 1`), so the
   pack is not excluded and the dead triple rides into the roll-up
   exactly as today. The control (a non-deltifying `echo a` → `echo c`)
   leaves 3 objects, 0 reachable, and does fire. runcl's 3-object shape
   is the CONTROL arm: F14 writes tiny non-deltified files. The rule
   works on the rig's workload and not on a repository with content.
3. **It turns the pack-count cap into a latch.** Dead packs can never be
   folded, so `tiers.len() >= cap` latches permanently. Depending on
   placement that either fires `Plan::Base` on every tick and after
   every batch — a whole-repository rewrite, the M6/runcg defect
   `7202c2b5` reintroduced and made permanent, which `fold.rs:1104`
   exists to prevent — or makes `forced` permanently true and defeats
   the 256 MiB floor.
4. **"Exclude them from the cap's count" deletes the only bound.**
   `fold.rs:1127` CONTROL 1: *"with nothing foldable the cap MUST still
   rebuild, or the count is unbounded."* And the plan's open question
   "how many tiny packs is too many" was never open — the design
   answered it: `fold_max_packs` 64, against git's own
   `gc.autoPackLimit` of 50 (`forge-compaction-tiers-design.md:1283`).
   The cost is linear or worse in the pack count across `restore.rs`
   fetch planning, `ScopedOdb::build`'s per-pack hardlinks on every
   proof, the `packs` list in every `snapshot::cas`, `local_packs` on
   the push path, the sweep, and git's own mmap of every pack.
5. **The cost model was wrong.** `planned()` is a synchronous fn on the
   SERVING LOOP (`fold.rs:439`, called at `server.rs:714` right after
   `run_batch`), `plan` is deliberately pure, `reachable_from` at
   `fold.rs:601` is in `run_task` and not reusable at plan time, and
   "cached per snapshot seq" never hits because the seq increments on
   every batch CAS. The module's "never on a push's path" rule is about
   BYTES; this would put a repository-sized CPU walk there.
6. **The unit rig cannot test it.** `stage_commit` packs with no
   `--fix-thin` pass, so it produces only the all-dead shape. Any green
   unit test for change 2 measures the rig's artefact, not the system.

A better predicate exists — *"contributes no reachable object that no
other named pack holds"* — which fires on both arms of the fix-thin
experiment and also collects two shapes change 2 would newly pin (an
empty named pack; a dead pack wholly covered by a live one). It does NOT
escape objection 1: it is still reachability-keyed, so it still disarms
the coverage tests. Anything in this family needs a positive control for
the coverage rule that does not route through fold planning.

## What this leaves

The residue that matters is the `refs/for` conflict class, on the
workload forge recommends, and neither change addresses it. The only
measure that removes residue regardless of class is the reclaim
(direction 4) — which, after three critiques, is also the only candidate
with no interaction hazard: it does not touch the coverage rule, the
planner, or the cap. Its cost (a repack plus a full-repo upload on the
cold-start path) is real and unchanged.

**Before any of it, get the measurement right.** The 58% figure and the
byte numbers exist only as prose in two plan docs, a test comment and a
cfg comment — `forge/e2e/results/` holds no classification artefact, and
it is n=1 on one ~80 KB repository whose pushes do not deltify. On a
repository with real content the residue's size AND class mix are both
unknown. That measurement, with the artefact committed, is the next
step; it is cheap, and every choice above turns on it.
