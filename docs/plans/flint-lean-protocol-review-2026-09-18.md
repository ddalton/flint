# flint-lean protocol review — 2026-09-18, HEAD `0d5808c7`

The question was "is the lean protocol COMPLETE and CORRECT?", asked
after v1.56.0 with `lean/SAFETY.md` (S1-S6), `lean/formal/COVERAGE.md`
and a 117-run gate in place. Completeness here means every event the
environment can produce has a defined, safe outcome, and every promise is
checked by something that can see it. Correctness means the promises hold
in the shipped code, and the model that certifies them describes that
code.

Six reviewers, one per area (store outcomes and crash points; the ack and
the reader; the foreign-change lifecycle; lease, fence and ticket; claim
adequacy; the event alphabet), each required to return `file:line`, a
numbered scenario, a `known_status` against `lean/FINDINGS.md` and the
2026-09-12 review, and a HELD list (96 claims held in total, each with
the line that enforces it). Every HIGH/CRITICAL finding (capped at three
per area) was then handed to an independent refuter whose default verdict
was "refuted"; a completeness critic read all six outputs for what nobody
looked at. Everything was read-only: no cargo, no TLC. Workflow run
`wf_56c5207c-1b8`; 21 agents; 38 raw findings, 12 verified (11
CONFIRMED, 1 PLAUSIBLE, 0 refuted), 3 HIGH carried unverified, 7 critic
items. Mechanisms of every CRITICAL and HIGH were re-read by the author
of this record before it was written.

## Verdict

**The commit point still holds; the claim overstates what the gate
certifies; and the edges have two CRITICAL and eight HIGH routes, six of
them new mechanisms rather than re-runs of old ones.** No reviewer found
a way to land a manifest naming bytes that were never uploaded, to run
two commit sections at one epoch through the CAS, or to overwrite
locally-dirty work through the steady-state consume. The fences on the
CAS, the window and the handoff held (96 HELD claims, in the raw record).

What was found clusters into five mechanisms:

1. **A name rule with no owner.** The scan skips any file whose name
   ends in `.flint-sync-tmp` (the atomicity-7 fix); the gateway accepts
   such a name. An acked UI write under that name is materialised,
   cited once by the citation repair, then classified as a delete and
   its object collected. No race needed. (C1)
2. **The fence is a count, not a clock.** Inside the commit section the
   cell is read before the CAS and then only every 200 GC deletes; the
   DELETE itself carries no epoch. A holder deposed after its CAS landed
   still runs its deletes, and a peer that cited the same etag in the
   meantime (same-bytes re-upload or adoption) is left with a dangling
   citation. The model fences every `GCDelete` atomically, so no world
   can see it. The same stretch (claim to 200th delete) moves no token,
   so a live holder with a big manifest is deposed at 60 s. (C2, M4)
3. **Collector-off is not "a cost".** On MinIO and Ozone (the e2e rig
   and a target) the leaked object at a deleted path (a) supersedes the
   peer's tombstone, because the tombstone HEAD compares nothing, and
   (b) is a citation-repair candidate, because the baseline still holds
   it and the merge base does not. One `rm` on a two-writer workspace
   flaps forever: S4 and S6 are broken on those stores, not degraded.
   The leak also blocks every create door. And the conformance gate's
   "no verdict" arm leaves the collector ON. (H1, M2, M7)
4. **A verb that is judged once and applied later.** A declared removal
   or rename carries no etag: `If-Match` is checked at declaration, the
   unlink happens at the next barrier "if clean", and a version the
   agent published in between is deleted or moved with no copy and no
   record. A landed inbox CAS answered 409 is fatal to the gateway's
   append, so a landed PUT is untracked and the write it replaced is
   dropped as superseded. A restart releases a cell held at an epoch
   this incarnation never recorded without the rotation, so a deposed
   straggler's CAS lands. (H2, H3, H4)
5. **"Never a half-written tree" is false with two writers.** A fresh
   checkout adopts a peer's mid-barrier uploads path by path (S3-wins)
   and its first repair publishes the mix as a boundary; the untracked
   sweep does the same one path at a time for a lost writer. The
   directory-symlink swap re-opens atomicity-6 (`O_NOFOLLOW` and lstat
   guard the last component only). The consume's re-stat is followed by
   a CRC, a temp write and the rename, so an agent write in that window
   is still overwritten. (H5, H6, H7)

On the claim: `SAFETY.md` §3 says every invariant is checked
exhaustively per world. On the code's shape (`IMPL` in `gen-cfgs.sh`)
the gate checks the ten `BLINV` invariants only; the five ack invariants,
the two sync, the two rename and the two narrow invariants are certified
only in life-lease worlds the code has not had since v1.52.0, or opt-in
on one path. Three enforcers `SAFETY.md` names (`Inv_CitedVersionLives`,
`Inv_NoUncitedGC`, `Inv_BoundaryAtomic`) are in no cfg at all — they are
gated-lane invariants, and the code has no version id for the first to
be about. `Inv_NoResurrection` is a ghost written only under a mutation
constant, so its 17 strict runs are tautologies. (H8, H9, M1)

## Findings

Severity is this record's, after the refuter's verdict and a re-read of
the mechanism. "Verified" is the independent refuter's verdict; "author"
means the mechanism was re-read by the author of this record only.

| id | sev | mechanism | promise broken | verified | raw ids |
|---|---|---|---|---|---|
| C1 | CRITICAL | A gateway PUT to a path ending `.flint-sync-tmp` is acked and materialised (`path_ok` `lean/gateway/src/workspace.rs:421`; consume `barrier.rs:395`), skipped by the walk (`scan.rs:64`), cited once by the repair (`barrier.rs:1193-1201`, the path is in `first_absence`, not excluded), classified as a delete next barrier (`scan.rs:148`), collected (`barrier.rs:1585`). The agent-side twin: an agent file with that suffix is never published and nothing records it. | S1 `Inv_HITLTracked`, `Inv_HITLDurable`; AGENTS.md "Nothing is silent" | CONFIRMED; author | event-alphabet-1, event-alphabet-9(3) |
| C2 | CRITICAL | A holder deposed AFTER its pointer CAS landed (a >60 s stall inside step 6: `verify_not_deposed` at `barrier.rs:1310`/`1470` only; `renew_if_due` at `:1500` every 200 deletes; `delete_if_match` `:1585` carries no epoch) runs its deletes; a peer that cited the same etag (same-bytes upload or adoption, re-verified at `:1324-1340` while the object was still there) is left citing nothing. `lease::release` returns Ok on a foreign holder (`lease.rs:566`), so nothing records it. The model's `GCDelete` guards `~Fenced(s)` atomically (`LeanSubtree.tla:1806`). | S2 `Inv_NoDangling`; S1 row 2 | CONFIRMED (raised to CRITICAL by the refuter); PLAUSIBLE by the lease reviewer; author | claim-adequacy-1, lease-and-fencing-2 |
| H1 | HIGH | Collector-off store (`conformance.rs:120`, MinIO L-102 / Ozone L-27): A's published delete leaks e0 at the key (`barrier.rs:1560-1573`); A's baseline keeps p (`:1650` removes only `report.deleted`). B queues a tombstone; B's consume HEADs the key, gets the leak, and calls the tombstone superseded with NO etag comparison (`barrier.rs:432-437`). B's repair filter (`:1193-1201`: baseline e0, `inst_base` none) re-cites p@e0; the CAS installs it. A's next delete is outranked, then applies, then leaks again: the path flaps on every barrier pair, forever. | S4 "a deletion is never resurrected"; S6 `Inv_QuiescentConverged`; A's `ok` ack for the delete | CONFIRMED twice (two reviewers, two refuters); author | ack-and-reader-2, foreign-lifecycle-1 |
| H1b | HIGH | Found by the model the same evening, after H1's invariant reached the untracked sweep's world (`untracked.rs`, finding 10, ships on): the sweep tracks an object at a CITED key whose etag the manifest does not cite — a live writer's in-flight upload qualifies once its grace elapses (`untracked.rs:59`, a long step 3 suffices). The uploader commits that generation; the entry stays (the window clear removes only what a barrier consumed, `barrier.rs` step 7); the uploader's agent deletes the file and the delete is published; the other writer's consume adopts the entry (`barrier.rs` consume: entry, live object, clean path), or had adopted it while it was pending, and its citation repair (`:1193`) re-cites the deleted generation. Second harm: a leaked generation the receiving tree never integrated supersedes its tombstone (`:432`, "any different object"), and the clean stale copy stays forever. | S4 "a deletion is never resurrected"; S6 convergence | CONFIRMED by the model (21 steps) and a test that fails unfixed; author | — |
| H1c | HIGH | Found once H1's third contest was tightened: the merge base (`Baseline::inst_base`) is rewritten at step 7 (`barrier.rs:1645`), after the CAS and the GC; a container restart in that window leaves it a generation behind the document the workspace installed (the intent journal records the etag, `state.rs:173`, and only the "bucket still at my document" case recovers). A path that install cited reads as a repair still owed; after a peer's knowing delete of it (collector-off leaves the object) the repair re-cites it. | S4 | CONFIRMED by the model (19 steps, `LeanBarrierLeaseCollectorOff`) and a test that fails unfixed; author | — |
| H1d, H1e | HIGH | Found with every other arm on: a UI write adopted while still PENDING, cited by another writer and then deleted by it knowingly (H1e), or cited by a writer that restarted before its window clear so the entry outlived the citation and was adopted after it (H1d). The adopter's merge sees "baseline ≠ merge base, object at the key" (`barrier.rs` repair candidates) — the same view a delete that merely raced the write leaves, where modify rightly wins — and nothing in the bucket tells the two apart. | S4 | CONFIRMED by the model (19 and 21 steps) and tests that fail unfixed; author | — |
| H1f | MEDIUM | Found by the final run of the four-barrier orphan world with every other arm on — the one route that is not a resurrection: a queued deletion superseded by a re-cite (`barrier.rs` tombstone pass, \"superseded\") is settled, and the pull it defers to is the next install's diff from a merge base that moved past the path when the deletion was queued; a second delete before that install leaves nothing to diff, and the clean copy of the retired generation stays forever — uncited, a repair candidate at every barrier, invisible to every safety invariant. | S6 convergence | CONFIRMED by the model (30 steps, `Inv_TreesConverged`) and a test that fails unfixed; author | — |
| H2 | HIGH | B deposes A (`lease.rs:269`, acquire lands) and dies before `rotate_for_takeover` (`:272`). B's container restarts: `release_stale_own` (`lease.rs:593-612`, called from `bin/flint_sync.rs:544`) matches `holder_id && !released` with no epoch test and calls `release`; no rotation. The next claimant takes the released cell with `rotate = false` (`:238-240`). A, alive and slow, reaches its CAS on an unmoved pointer: it lands. The orphaned-own arm of `claim_step` (`:233-235`) was written for this state and is never reached. | S5 `Inv_NoStragglerInstall`, `Inv_CommitExclusive` | CONFIRMED; author | lease-and-fencing-1 |
| H3 | HIGH | Every inbox-cell CAS helper (`inbox.rs:216` and eight siblings) matches `PreconditionFailed` only; S3's 409 `ConditionalRequestConflict` (`s3.rs:337` → `StoreError::Conflict`, observed on this key in the storm leg, README:1150) is returned as an error. On the gateway's append after a LANDED `put_whole` (`workspace.rs:753-756`, `:671`): the client gets 502, the object is untracked, the entry it replaced is dropped as `superseded` at the next consume (`barrier.rs:246-252`) with no record. The landed bytes are unwritable (409) for 600 s and unreadable through the gateway until the sweep; at an UNCITED key the sweep never tracks them (`untracked.rs:56`). The lease and the manifest CAS treat the same 409 as a lost race (`lease.rs:550`, `manifest.rs:500`). | S1 `Inv_HITLDurable`, `Inv_HITLTracked`; the gateway's "nothing was written" | CONFIRMED (refuter: CRITICAL) | store-outcomes-1, store-outcomes-2, store-outcomes-5 |
| H4 | HIGH | A declared removal/rename carries no etag (`inbox.rs:71-87`): `If-Match` is judged once at declaration (`workspace.rs:977-985`); removals are not window-gated (`inbox.rs:240-258`); `apply_removals` at the NEXT barrier unlinks "if clean" (`barrier.rs:644-669`). A version the agent uploaded and had acked at seq N in between is deleted (or, for a rename, the OLD bytes land under the new name and the new version is deleted). No copy, no record; the ack says `deleted: 1`. | S3 "both versions survive"; S1 (the acked boundary is undone by a request that named e0) | CONFIRMED | foreign-lifecycle-2 |
| H5 | HIGH (security) | `mv d d.bak; ln -s /proc/self d` between the scan and the upload: `symlink_metadata` (`barrier.rs:1909`) and `read_nofollow`'s `O_NOFOLLOW` (`:2089`) guard the LAST component only; `d/environ` reports a regular file and the syncer's environment is PUT and cited. The atomicity-6 test swaps the final component only (`tests.rs:8927-8957`). No `resolve_contained` on the upload path. | AGENTS.md "symlinks are never published"; atomicity-6 re-opened | CONFIRMED; re-adjudicated | event-alphabet-2 |
| H6 | HIGH | Multi-writer checkout (`sole_writer` defaults false): a peer's lease-free uploads land path by path; a fresh checkout 412s on the cited etag and adopts the CURRENT object (`checkout.rs:552`, `:570-582`, no inbox read, no window check); the baseline carries the peer's etags and `inst_base` the manifest's, so the first barrier's repair (`barrier.rs:1193-1203`) cites the mix as a boundary. If the peer's pod is gone, half its change is published and half is lost. The untracked sweep (`untracked.rs:56-86`) publishes a lost writer's partial set the same way, one path at a time. | AGENTS.md "a boundary is a coherent point ... never a half-written tree"; S6 converges to a state no barrier declared | CONFIRMED | ack-and-reader-1, event-alphabet-5 |
| H7 | HIGH | The consume's post-fetch re-stat (`barrier.rs:271`, the atomicity-3 fix) is followed by `crc64_nvme` over the body (`:317`), `contained_path` (`:359`) and `write_file_atomic` (`:374`), which writes the whole body to a temp and then renames over the path. An agent write inside that window (tens to hundreds of ms for a large file) is overwritten with no record. The model's `Consume` is atomic. | AGENTS.md "never overwrites a file you modified"; S3 | author only (carried unverified; order confirmed by reading) | event-alphabet-6 |
| H8 | HIGH (claim) | The strict gate runs carrying the full `IMPL` set are six (`QueueHolds`, `ImplHolds`, `CollectorOff`, `InboxSnapshot`, `OrphanTracked`, `ImplThreeWriters`) and all check `BLINV` (ten invariants). `Inv_AckImpliesCited`, `Inv_AckBoundaryCoherent`, `Inv_NoFencedOkAck`, `Inv_NoNonceOrphan`, `Inv_BoundaryNamesItsClock`, `Inv_SyncNeverDestroysDirty`, `Inv_NoForeignLost`, `Inv_RenameAtomic`, `Inv_RenameNoHole`, `Inv_NarrowNeverDeletes`, `Inv_NarrowNeverRecites` are certified only with `BarrierLease = FALSE`, `WriterQueue = FALSE`, `EmptyInstall = FALSE` (COVERAGE.md's zeros), or opt-in on one path (`SentinelImpl1`, laptop). `SAFETY.md` §3 "every invariant above, exhaustively, per world" and §4 do not say so. | S1 rows 1-5, S3 rows 2-3, S4 rows 2-3: no check on the shipped shape | CONFIRMED (three reviewers) | ack-and-reader-3, claim-adequacy-3, event-alphabet-4 |
| H9 | HIGH (claim) | `Inv_CitedVersionLives`, `Inv_NoUncitedGC` (S2 rows 2-3) and `Inv_BoundaryAtomic` (S5 row 5) appear in no cfg, no gate line, no COVERAGE row: they read `versions`/`citeDone`, gated-lane state frozen at Init in every world. `LeanEntry` carries no version id and every reader fetches by etag (`lib.rs:838`), so "on a versioned bucket the exact cited version is still stored" is not what the code does. | SAFETY.md §1 enforcers, §3 "every invariant in §1 has at least one refutation" | CONFIRMED (three reviewers) | claim-adequacy-2, ack-and-reader-4, event-alphabet-3 |
| H10 | HIGH (claim) | Found by the crash world at `OrphanTrack = TRUE` on the final module (`lean/formal/results/2026-09-18-crash1-orphantrack/Crash1OrphanTrue.log`, depth 25 after 148,870,390 distinct states): an agent's declared write is published (seq 2); the writer RESTARTS after its step 7 and before the ack is written; a peer deletes the path (seq 3); the restarted incarnation re-runs the pending, honours it with a PULL-ONLY install of the peer's document and acks ok — a boundary that no longer cites the declared content while the tree still holds it (the peer's delete waits in this writer's queue). `AckHonest` does not see it: a pull-only merges nothing, so it drops no citation. The honest answer is the boundary the pre-restart barrier installed (seq 2), which the new incarnation no longer knows; the ack should name that, or refuse. No sweep action is in the trace: the route is independent of `OrphanTrack`, and the FALSE arm stops earlier on `Inv_HITLTracked` (21), which is why neither arm had shown it. | S5 / D1: an ok ack names a boundary that carries the declaration | CONFIRMED in the model (IMPL shape, `AckHonest = TRUE`, `AckFromInstall = TRUE`); the code path (`sentinel.rs` re-run after a restart honoured by a pull-only) not yet traced or tested | — |
| M1 | MEDIUM (claim) | `Inv_NoResurrection == ~gh.resurrected` is written only as `RematerializeOnRestart /\ res` (`LeanSubtree.tla:1218`); the constant is TRUE only in the `LeanRematerialize` mutation. In all 17 strict worlds the invariant is a tautology. The code's rule (`checkout.rs:7-11`, `:783-795`, never re-materialise over a live tree) is in no model action. | S4 row 1 presented as machine-checked | CONFIRMED | claim-adequacy-4 |
| M2 | MEDIUM | `conformance::gate` returns `Unknown` when the probe could not write (`conformance.rs:113-114`); `flint_sync.rs:442-444` prints and proceeds; `cfg.conditional_delete_enforced` defaults `true` (`lib.rs:483`). A MinIO/Ozone bucket whose policy denies the probe key runs the collector ON: the refuted `GCUnconditional` shape. A 403 is rightly not a verdict; the fallback posture was decided by nobody. | S2 `Inv_NoDangling` on a collector-off store; SAFETY.md §2 row 2 | author (critic item, read) | critic-2 |
| M3 | MEDIUM | Nothing moves the cell token between the claim (`lease.rs:269`) and the 200th delete (`barrier.rs:1499`); the 30 s heartbeat went with finding 4. A live holder whose section exceeds 60 s (a 264 MiB entries document took 27 s to load on the 0b rig) is deposed by a waiter; two such writers depose each other every floor and neither installs. The model deposes only `Quiet` (stalled/dead) holders. | design §4 "a live holder is never deposed"; availability | PLAUSIBLE | lease-and-fencing-3 |
| M4 | MEDIUM | `lease-4` is not moot: `renew_if_due` (`barrier.rs:849-852`) compares the node clock with the cell's Last-Modified; a node behind the store never renews inside a mass delete, and the waiter deposes a live holder mid-GC — the entry state of C2. | FINDINGS.md L-row lease-4 deferral reason | CONFIRMED; known-open | lease-and-fencing-4 |
| M5 | MEDIUM | A UI PUT of `build/log.txt` over a regular file `build` is acked (`path_ok` is syntactic); the consume refuses containment ("parent is not a directory", `barrier.rs:2352`), records it in the pod's log and DROPS the entry from the cell; the object sits uncited at an uncited key that the sweep skips. The agent-side twin (file→dir on a cadence barrier) cites both `a` and `a/x` in one generation. | S1 `Inv_HITLTracked`; "a boundary is a coherent point" | CONFIRMED | event-alphabet-7 |
| M6 | MEDIUM | On a collector-off store the leaked object blocks every create door for the path: `If-None-Match: *` → 412, no precondition → 428, rename-onto → `DestinationExists`, draft promote refused; inside the grace 409; an agent re-create preserves the garbage as a "foreign version" with an `upload-412-preserved` record naming a writer nobody was. | SAFETY.md §2 "served to nobody", §4.7 "not a loss; a cost" | CONFIRMED | foreign-lifecycle-3, event-alphabet-8 |
| M7 | MEDIUM | `gateway_append`'s retry after its own landed CAS (`inbox.rs:207-216`) `retain`s away a NEWER entry for the same path that landed in between, and re-pushes the stale one; the next consume drops the newer acked write as superseded. Narrow window (SDK backoff vs a second client's full PUT). | S1 `Inv_HITLTracked` | PLAUSIBLE, unverified | store-outcomes-3 |
| L1 | LOW | SAFETY.md is stale at HEAD: §4.4 "neither run has been made yet" and §5.3 — commit `a42c3d40` ran both arms (`OrphanTrack=FALSE` VIOLATED as intended, 66M states; `TRUE` INCONCLUSIVE, disk guard at 158M); §3 says 116 runs / 28 strict, `check.sh` asserts 117 and COVERAGE.md 29; README line 16 says 116; the CI file's comment says 83. | claim currency | CONFIRMED; author | claim-adequacy-6 |
| L2 | LOW | SAFETY.md §4 names about two of the fifteen "not modelled" items the README names (atomic scan and two-scan rule; bare touch, min-interval, budget; an agent writing during sync; multi-gateway; window closing at the CAS vs after the GC; 412 arm parking; vanished base as a park; bytes coinciding with an unseen version; the grace escapes; 8 lost handoffs; the stronger ticket; the abort trace event; manifest as one object; whole-PUT vs compose; sockets/FIFOs/symlinks). | SAFETY.md preamble | CONFIRMED | claim-adequacy-7 |
| L3 | LOW | `Inv_NoFencedOkAck` and `Inv_NoDeposedPut` hold vacuously under the barrier lease (README says so; SAFETY.md S1/S5 list them as enforcers). §5.8 calls S2/S5 state-based; three of their rows are ghosts (`NoStaleOverride`, `NoStragglerInstall`, `NoDeposedPut`). | claim wording | CONFIRMED | claim-adequacy-5, -9 |
| L4 | LOW | A pointer CAS that lands with its response lost is reported `no_change: true` with `uploaded: n`; `note_boundary` is skipped and `installed_etag` is not journalled; the tree converges. `upload_one`/`upload_compose` treat a per-key 409 as fatal to the whole barrier. | AGENTS.md ack semantics; availability | CONFIRMED | store-outcomes-4, -5 |
| L5 | LOW | `boundary: "sentinel-deferred"` is stamped on a startup honour and on any floor-tick honour; `gauges.json` `last_boundary.source` is hard-coded `cadence` for every install. | AGENTS.md:152-155 | CONFIRMED | ack-and-reader-5 |
| L6 | LOW | The conflict COPY is durable in the bucket; the RECORD naming it is a pod-local file that rotates at 1 MiB and dies with the pod; nothing lists `conflicts/`. | S3 "with a record naming it" | CONFIRMED | foreign-lifecycle-4, event-alphabet-9(4) |
| L7 | LOW | Contract silences: chmod-only is invisible (`stat_changed` compares size and mtime, `scan.rs:36`); two names differing only in invalid UTF-8 collapse to one key (`to_string_lossy`, `scan.rs:74`) and one is never published. | AGENTS.md "with their mode bits", "Nothing is silent" | CONFIRMED | event-alphabet-9(1,2) |
| L8 | LOW | The adopted-own claim arm writes nothing, so a holder that re-adopts after losing all 8 handoff races starts its section on a token a waiter has been counting for a floor — the lease-2 rule applied to renew but not to claim. | availability; entry state of C2 | CONFIRMED | lease-and-fencing-5 |
| L9 | LOW | FINDINGS.md L-24 is carried OPEN on a reason that is false at HEAD (the model's 412 arm no longer parks; `Inv_NoStaleOverride` checks generation; `VerifyUploadedCitations` withholds the citation L-24 describes). What remains of it is C2. | ledger currency | CONFIRMED; known-open | claim-adequacy-10 |
| L10 | LOW | The chunk reaper's grace-vs-longest-publish rule (`manifest.rs:841-843`) and the fact that LeanSubtree models the manifest as ONE object are in neither §2 nor §4; the pointer is the CAS unit and the entries model stays faithful. | claim wording | CONFIRMED | claim-adequacy-8 |

## What the review did not reach (the critic)

- **`sync.rs`, `reader.rs`, the rescope arm of `checkout.rs`**: the three
  baseline writers OUTSIDE `barrier_inner`. These carry S3 rows 2-3 and
  S4 row 3 — the only SAFETY rows with neither a code-side HELD line nor
  a model run on the shipped shape (H8). Rescope holds the fence for its
  whole run and makes no cell writes, so M3 applies to it. One
  reviewer-hour; the single most valuable next check.
- **`conformance.rs` / `crates/flint-store/src/probe.rs`**: read only for
  M2; the probe's cleanup after a landed-but-lost PUT and the caching of
  the verdict across a pod replacement are unjudged.
- **`control.rs`, `uds.rs`, `verbs.rs`, `bin/flint_sync.rs`** (the
  `.flint/` pre-flight, `capabilities.json`, the torn-body rule, the UDS
  door, startup ordering): in no reviewer's output.
- **The chunked manifest** (`cas_write_chunked`, `sweep_chunks`,
  `LeanChunkGC.tla`) under a deposed straggler, and the multipart/compose
  path: deferred by three reviewers each.
- **`crates/flint-store`** beyond the status table: `readonly.rs`,
  `rawread.rs`, `layout.rs`, `gate.rs`, the SDK retry classifier, the
  versioned-bucket arms.
- **The unit battery census** (SAFETY.md §3 "each finding pinned by a
  test whose control fails it"): not taken.
- **TLC witnesses** for H1 and C2: neither has a model world (the
  tombstone HEAD is etag-less in the model too; `GCDelete` is atomically
  fenced). Both need a cfg before a fix is trusted.

## Applied 2026-09-18 (same day): C1, C2, H1

Model first, then the test written against the unfixed tree, then the fix,
then each fix line reverted alone to see its test fail.

| id | model | test (fails unfixed at the finding's assertion; fails again with the fix line reverted) | fix |
|---|---|---|---|
| C1 | none: the model has no name alphabet | `a_consumed_entry_named_like_a_consume_temp_is_surfaced_never_collected`; gateway `path_hygiene_the_size_cap_and_a_missing_file` gained the two suffix names | `path_ok` refuses a segment ending in the suffix; `resolve_contained` refuses it (surfaced, never materialised); `AGENTS.md` names it |
| C2 | `GCFencePerDelete` (FALSE = shipped: no fence in step 6) and the collector's "still referenced" read of the writer's OWN install (`instSnap`) instead of the live manifest. `LeanBarrierLeaseStragglerGC` violates `Inv_NoDangling` in 17 steps; `…GCFenced` holds (276,622 states, depth 30); `LeanProbeStragglerGCFenced` shows the thawed straggler fenced AT its GC | `a_holder_deposed_after_its_cas_landed_does_not_collect_what_the_successor_re_cited` — the unfixed run's message shows A's barrier returning `Ok` with `deleted: ["x.txt"]` after its deposal | before every delete, renew when the last cell WRITE is older than `renew_within_secs` (a new `LeanConfig` field, 20 s; `Syncer.cell_written_at`); `renew_if_due`/`fence_on_cell` and the every-200 cadence removed, which also retires `lease-4`'s store-clock freshness test |
| H1 | `TombstoneNamesRetired` (FALSE = shipped) with `sc[s].fqRetired`, and the new `Inv_NoDeleteResurrected` (stated over the manifest and one history variable). A delete is recorded only if UNCONTESTED at publication, and three legitimate routes taught what "contested" means: another writer dirty at the path or uploading it (the C2 control world); the publisher's own agent re-creating the file between the scan and the CAS (same world); and another writer holding a generation of the path it integrated but has not cited yet — a consumed UI write whose citation repair is the "modify" of delete/modify, so modify wins and the agent's delete is lost, the Crash1 stance (the collector-off breadth world, on the gate's first run, through a restart between the CAS and step 7 and again with an honestly old base). Two arms were tried for that third route and withdrawn with their reasons in the formal README: a repair that only re-cites over the citation it expected (refuted by `Inv_HITLTracked`: it also blocks the repair that keeps a consumed UI write tracked against a peer that never saw it), and a restart that recovers the merge base from the journal (nothing left for it to catch once the contest rule was right). `LeanBarrierLeaseLeakResurrects` violates in 18 steps; `…LeakHolds` holds (91,097 states, depth 42); `LeanProbeTombstoneOverLeak` shows the tombstone applied over the leak. The six IMPL strict worlds carry the invariant | `a_peers_delete_on_a_collector_off_store_reaches_the_other_tree_and_is_not_resurrected` | `ForeignChange.retired` carries the merge base's etag for a gone path; the tombstone HEAD supersedes only on a DIFFERENT etag |
| H1b | `OrphanEntryCited` (FALSE = shipped) with `gh.orphanAt` (the citation the sweep judged against, real state) and `sc[s].judged` (a provisional adoption's); `LeakSupersedesNothing` (FALSE = shipped) in `tSuper`; `Inv_TreesConverged` (a peer's published delete reaches every live tree, stated at quiescence over the trees; a queued deletion the next consume would apply, and an owed repair, excepted — H1f narrowed the first: not a deletion the key's leak would supersede again). `LeanBarrierLeaseOrphanResurrects` violates `Inv_NoDeleteResurrected` (22 steps when found; 23 on the final module) — with the tombstone OFF: H1e's tombstone closes H1b's resurrection on its own (with it on, the world HELD in the first 2026-09-19 gate), so the arm's remaining content is the sweep's policy, pinned by the code's tests; keeping it or dropping the two rules as redundant is OPEN — `…OrphanTracked` holds; the four-barrier `…OrphanStaleCopy` violates `Inv_TreesConverged` and `…OrphanConverges` holds — at three barriers the pair was VACUOUS (identical state counts) because the consume that applies the tombstone is the fourth, and `LeanProbeLeakApplied` now guards that. `LeanBarrierLeaseLeakResurrects` needs `LeakSupersedesNothing=FALSE` now (the leak rule subsumes H1's for the leak; after H1e and H1f, `ManifestTombstones=FALSE` and `SupersedeRestoresBase=FALSE` too), and `…LeakRetiredOnly` shows H1's rule alone still closes the resurrection | `a_sweep_entry_that_outlived_the_citation_it_was_judged_against_does_not_resurrect_a_delete` (crash shape), `a_sweep_adoption_is_void_once_the_citation_it_was_judged_against_moves` (no crash), `a_voided_sweep_adoption_of_a_path_the_merge_base_never_had_is_removed`; five fix lines, each reverted alone fails exactly one assertion (the consume-time drop's mutation survived until a "never materialised" assertion pinned it: with the repair-time rule in place it is a cleanliness rule, not a safety one) | `InboxEntry::cited` (the sweep sets it; the consume drops an entry whose citation moved); `BaselineEntry::judged` (set at adoption; `void_stale_repairs` withholds the repair on every CAS attempt and returns the path for the tombstone queue where the manifest cites nothing); the tombstone pass HEADs, and a different object supersedes only if the manifest cites it or a surviving entry names it (one manifest GET per consume that needs it) |
| H1c, H1d, H1e | `ManifestTombstones` (FALSE = shipped): `CASInstall` records in `gh.tomb[p]` (real bucket state) what each delete retired, cleared when the path is cited again; `RepairOwed` requires `~Entombed`, and `MergeGone` queues an entombed path as a tombstone for the tree. `LeanBarrierLeaseCollectorOffNoTombstone` violates (21 steps); `…CollectorOff`, `…ImplHolds`, `…InboxSnapshot` hold again. Two arms written on the way were DROPPED as redundant once this one was in, each found by its known-bad world refusing to go red: `BaseAtCas` (the merge base persisted at the CAS, for H1c) and a consume-time "pull" (an entry the manifest already cites becomes the merge base, for H1d) | `a_pending_adoption_cited_by_its_own_writer_that_restarted_is_not_re_cited_over_a_knowing_delete` (H1c; the restart lands at the barrier's own step-6 delete, the only bucket operation between the CAS and step 7), `a_pending_adoption_of_an_entry_that_outlived_its_citers_restart_is_not_re_cited` (H1d), `a_pending_adoption_cited_and_then_knowingly_deleted_is_not_re_cited` (H1e) — one check reverted fails all three | `LeanManifest::tombstones` (path → retired etag + seq; `merge` writes one per applied delete, clears it on a re-cite, evicts past `TOMBSTONE_KEEP_SEQS`); inline in the single-object layout, a content-addressed `chunk::TombstoneBody` named by `Pointer::tombstones` in the chunked one, fetched with the chunks and kept by `sweep_chunks`; `void_stale_repairs` voids a repair the tombstone names and returns the path for the tree's tombstone queue |
| H1f | `SupersedeRestoresBase` (FALSE = shipped): `Consume` restores `instBase[p]` to `fqRetired[p]` for a superseded deletion; `gh.baseRestored` probes it. `LeanBarrierLeaseSupersedeDropsBase` (the `…OrphanConverges` world with the arm off) violates `Inv_TreesConverged` (30 steps), `…OrphanConverges` holds (10,475,529 distinct states, depth 43), `LeanProbeBaseRestored` fires (22 steps). The arm turned `…OrphanStaleCopy` green — the shipped leak rule now FLAPS (re-queued at every install, superseded again) instead of settling, and the invariant's pending-work exception excused it — so `Inv_TreesConverged` counts a queued deletion as pending only if the next consume would apply it (`DeletionPending`, sharing `ObjectSupersedes` with `Consume`). The restore also closes H1's resurrection half alone (`…LeakRestoreOnly` holds; `…LeakResurrects` now turns all three rules off), but not its convergence half: `…LeakFlaps` (the restore alone, under `Inv_TreesConverged`) violates (19) and `…LeakRuleConverges` (one arm from it, the leak rule on) holds; `…LeakSkippedGeneration` (one arm from `…LeakHolds`) shows the retired-etag rule alone cannot converge a leak of a generation the tree never installed (24) | `a_queued_delete_superseded_by_a_re_cite_that_is_deleted_again_before_the_install_still_leaves_the_tree` (A's second delete runs inside B's own upload, between B's consume and its install); the restore reverted fails it at the claim | the tombstone pass's superseded arm writes `baseline.inst_base[path] = retired` (the baseline entry's etag for a queue file written before `retired`) |

Suites: syncer 233/233, gateway 24 + 26 + 5, both green. Clippy's lints are
the pre-existing baseline (none at a changed line). The gate went from 117
to 136 runs over the day (H1b–H1f, found by the model the same evening, are
in the two tables above and the formal README's "Review 2026-09-18,
evening" section); the C2/H1 logs and the before/after census are in
`lean/formal/results/2026-09-18-review-c2-h1/`, the evening's worlds in
`lean/formal/results/2026-09-18-review-h1b/` (its last run is the one that
found H1f), the final module's deciding worlds, gate and trace replays in
`lean/formal/results/2026-09-18-review-h1f/`. The crash world at both
`OrphanTrack` arms ran after the gate (`results/2026-09-18-crash1-orphantrack/`):
FALSE violates `Inv_HITLTracked` at 21 as it should; TRUE ran 52 minutes to
depth 25 (148,870,390 distinct states) without violating it and stopped
there on `Inv_AckImpliesCited` — H10 above, new and open. SAFETY §4.4's
TRUE half is therefore still not a hold. `SAFETY.md` §2, §3, §4.4, §4.11-13 and §5 were
corrected the same day (L1, H8, H9 in part); the IMPL-shape worlds for the
eleven uncertified invariants and the restatement of the three dead
enforcers are §5 rows 9-10, open.

## Dispositions (proposed; C1, C2, H1 applied above)

| id | fix |
|---|---|
| C1 | Refuse the suffix in `path_ok` and in `check_contained` for consume and checkout (surface, do not materialise, like the control namespace); or skip only temp names whose stem is being materialised now. Gateway test + a syncer test that a consumed entry with the suffix is never classified as a delete. |
| C2 | Fence step 6 on TIME: re-read the cell before the first DELETE and whenever `QUIET_SPACING_SECS` has elapsed since the last cell read; a failed read is a fence. Model: make `GCDelete`'s fence an observation (unfenced until the writer reads the cell) and add a world with `AllowStall`, `MaxSameBytes=1` and `IMPL`; `Inv_NoDangling` must fail on the shipped cadence and hold with the time fence. Test: depose A between its CAS and its first delete with B's same-bytes upload in place. |
| H1 | The tombstone HEAD must compare the etag to the baseline's: equal ⇒ the leak, apply the tombstone (remove the file, drop the baseline entry); different ⇒ superseded. And a leaked path must leave the repair candidate set (drop it from `inst_base`-vs-baseline consideration, or drop the baseline entry at leak time and re-offer the DELETE by a separate leaked set). Model: `TombstoneHeadsKey` needs an etag; add the collector-off two-writer delete world and expect `Inv_NoResurrection` (made a real state predicate) to fail as shipped. |
| H2 | `release_stale_own` must take the rotation when the cell's epoch is not the incarnation's recorded one (the orphaned-own rule), or route through `claim_step`'s arm and release from there. Test: acquire-then-die-before-rotate, restart, assert the straggler's CAS 412s. |
| H3 | Every `inbox.rs` CAS loop matches `Conflict` alongside `PreconditionFailed` and re-reads/retries (as `manifest.rs:500` and `workspace.rs:1382` do). Then M7 must not widen: the retry must not replace a newer entry for the same path. Give `put_file` a recognizer for its own landed PUT (the `gateway-<uuid>` flush stamp) so a lost response re-tracks rather than 409s for 600 s. |
| H4 | A `Removal` carries the etag it was judged against; `apply_removals` unlinks only if the baseline's etag equals it, else refuses with a record (the tombstone rule). Rename: copy If-Match that etag, and refuse when the destination copy would carry stale bytes. |
| H5 | Resolve the upload path with `openat2(RESOLVE_NO_SYMLINKS \| RESOLVE_BENEATH)` (Linux ≥ 5.6), or walk components with `O_NOFOLLOW\|O_DIRECTORY` `openat` from the root fd; refuse and record on any link. Test: the directory-swap variant of atomicity-6. |
| H6 | Either the checkout refuses S3-wins adoption when the object's flush stamp names a live peer (a barrier in flight), or the contract states that a multi-writer checkout may deliver a tree that is not any boundary. The sweep should track a lost writer's set as ONE unit (group by `flush_uuid`), or the contract says it does not. |
| H7 | Re-stat immediately before the rename (after the temp write), or hold the path's dirty-check and the rename under one `flock` on the parent. The residual window then is the rename itself. |
| H8, H9, M1, L1-L3 | SAFETY.md §3/§4: say which rows are certified on the shipped shape (name the six cfgs), which only on the life-lease shape, and which by nothing; remove the three gated enforcers or replace them with the state-based reader claim the code actually keeps (etag-resolved reads); restate `Inv_NoResurrection` over state (a live tree's local set is never re-fetched at restart) with a mutation that fails it; update the counts and §4.4/§5.3 to `a42c3d40`. Move the eleven uncertified invariants into `IMPL` worlds (one-path budgets fit a laptop for the sentinel; the rename/narrow/sync worlds need `IMPL` variants of `LeanRemovalHolds`, `LeanNarrowHolds`, `LeanSyncHolds`). |
| M2 | Decide the `Unknown` posture explicitly: collector OFF when the store is unverified (fail-closed, storage growth), or ON with a warning (today). Write it into SAFETY.md §2 either way. |
| M3, M4, L8 | A wall-clock renew inside the commit section (before the CAS when the section is older than half the deposal threshold, and in the GC loop by time) using a local monotonic clock; then `lease-4` closes. |

## The representation question

Asked alongside: "apart from the TLA model is there a more concise and
proper way to represent the lean protocol?" The answer is in the session
record and summarised here because the review's claim findings (H8, H9,
M1, L3) are consequences of it.

`LeanSubtree.tla` is a history of the protocol, not a specification of
it: 3,588 lines; 79 constants of which 8 are budgets and the rest are
arms (`TRUE` = shipped, `FALSE` = a refuted design or a mutation), so
every action is a tree of `IF` on constants (`BarrierLease` alone appears
59 times); 137 ghost fields and 37 per-writer fields; 352 lines of gated
mode the product dropped on 2026-09-13; 218 lines of the life lease the
code lost in v1.52.0; 15 of the 21 invariants are ghost stamps, which is
why one of them is a tautology in every strict world (M1) and three are
dead (H9). It is the right shape for what it does — a mutation harness
and a trace-validation target — and the wrong shape for reading, review,
or an inductive proof.

Two artefacts would fix that, in this order:

1. **A one-page protocol of record**, `lean/PROTOCOL.md`, in the style of
   Raft's Figure 2: the state (bucket: cell, pointer + entries, objects,
   inbox with removals and the window; writer: tree, baseline, merge
   base, queue, intent journal, pending sentinel), then every rule as a
   precondition and an effect (the seven barrier steps, claim/enqueue/
   handoff/deposal, the four gateway verbs, consume, sync, checkout,
   the sweep, the conformance gate's three postures), then S1-S6 as
   predicates over that state. Today the steps live in `barrier.rs`
   comments, the lease loop in a 2026-09-13 assessment, the contract in
   `AGENTS.md` and the properties in `SAFETY.md`; nobody can read the
   protocol in one sitting, which is how a name rule ends up with no
   owner (C1) and a fence ends up as a count (C2).
2. **`LeanCore.tla`**: the shipped shape only — no arms, no life lease,
   no gated lane, about a dozen actions and no ghost where a state
   predicate exists — and a TLC-checked refinement `LeanSubtree(IMPL) ⇒
   LeanCore!Spec` through an `INSTANCE` mapping, so the big module keeps
   its job and the small one becomes what the invariants are stated
   over. That is the only form on which SAFETY.md §5.8's inductive
   invariant (Apalache, then TLAPS) is tractable. PlusCal is the natural
   notation for the writer loop (`pc` is hand-encoded today). Quint is
   the readable alternative (typed, shorter, Apalache backend, transpiles
   to TLA+) at the price of a second toolchain the trace harness does not
   target.
