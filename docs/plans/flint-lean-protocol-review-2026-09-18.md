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

## Where this stands (2026-09-21)

The verdict above is the review's, unedited. What has happened to each
finding since is in the rows themselves and in the two "Applied"
sections; this is the index.

- **Fixed, with a test that fails unfixed and a mutation-checked fix:**
  C1, C2, H1, H1b-H1f (pushed `843a668c`); H10, H2, H3, M7, H4, H5, H7
  (2026-09-19, uncommitted); H5's compose residual, M5's agent twin
  (L-120) and M5's gateway half (L-121), and `sync.rs`'s
  adds-before-deletes (2026-09-21, uncommitted).
- **Disposed — the mechanism is gone, not the symptom:** M2 (nothing
  reads the collector posture any more; the field is deleted), M6 (every
  write lands at a fresh handle and the doors judge the tracked version),
  L9/L-24 (one key per path was the mechanism).
- **Restated rather than fixed, because the claim was the defect:** H8,
  H9, M1, L1, L2, L3 — `SAFETY.md` §3.1 is the table of what is checked
  on which shape, and the three enforcers no run checked are retired.
- **Open:** H6 (documented in `AGENTS.md`; the writers drill's host legs
  are tranche 6), M3 and M4 (availability, a live holder deposed at 60 s),
  L5-L8, L10.
- **The critic's unread areas** are still mostly unread — but `sync.rs`
  was read on 2026-09-21 and its apply order was a real defect, which is
  one data point for the critic's claim that this is where to look next.

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
| M2 | MEDIUM | `conformance::gate` returns `Unknown` when the probe could not write (`conformance.rs:113-114`); `flint_sync.rs:442-444` prints and proceeds; `cfg.conditional_delete_enforced` defaults `true` (`lib.rs:483`). A MinIO/Ozone bucket whose policy denies the probe key runs the collector ON: the refuted `GCUnconditional` shape. A 403 is rightly not a verdict; the fallback posture was decided by nobody. **DISPOSED 2026-09-21: there is no posture left to decide.** Under immutable handles nothing reads `conditional_delete_enforced` — both collect sites call `delete_many` unconditionally — so it was dead config and is removed. The probe's DELETE arm no longer changes behaviour, and the operator message promising "the file collector is OFF ... LEFT in the bucket" was false against a test that already pinned the opposite (`a_store_without_a_conditional_delete_collects_just_the_same`); it now says what is true. `Unknown` still proceeds unverified, which is right: a 403 is not a verdict and the verb reports its own error. | S2 `Inv_NoDangling` on a collector-off store; SAFETY.md §2 row 2 | author (critic item, read) | critic-2
| M3 | MEDIUM | Nothing moves the cell token between the claim (`lease.rs:269`) and the 200th delete (`barrier.rs:1499`); the 30 s heartbeat went with finding 4. A live holder whose section exceeds 60 s (a 264 MiB entries document took 27 s to load on the 0b rig) is deposed by a waiter; two such writers depose each other every floor and neither installs. The model deposes only `Quiet` (stalled/dead) holders. | design §4 "a live holder is never deposed"; availability | PLAUSIBLE | lease-and-fencing-3 |
| M4 | MEDIUM | `lease-4` is not moot: `renew_if_due` (`barrier.rs:849-852`) compares the node clock with the cell's Last-Modified; a node behind the store never renews inside a mass delete, and the waiter deposes a live holder mid-GC — the entry state of C2. | FINDINGS.md L-row lease-4 deferral reason | CONFIRMED; known-open | lease-and-fencing-4 |
| M5 | MEDIUM | A UI PUT of `build/log.txt` over a regular file `build` is acked (`path_ok` is syntactic); the consume refuses containment ("parent is not a directory", `barrier.rs:2352`), records it in the pod's log and DROPS the entry from the cell; the object sits uncited at an uncited key that the sweep skips. The agent-side twin (file→dir on a cadence barrier) cites both `a` and `a/x` in one generation. **2026-09-21: the consume half was already fixed** — a containment refusal writes a durable `consume-refused-containment` conflict record (`barrier.rs:441`), so what remains of that half is L6 (the record is pod-local). **The twin was real and is now fixed:** the scan sees the old path absent and the new one new, the two-scan guard withholds a FIRST absence, and the upload landed beside the citation it was meant to replace — `uploaded: ["a/x"]`, `deleted: []`, `first_absence: ["a"]`, and the installed document cited both. The CAS now judges the merged document's final shape and withholds MY side of any path/prefix clash (`upload-withheld-path-clash`, parked, published at the next barrier once the absence confirms). Both arms pinned by their own test — `a_published_file_replaced_by_a_directory_…` and `a_published_directory_replaced_by_a_file_…`, each failing when its arm is disabled, each asserting the next barrier converges and a fresh checkout materialises the result. No model can state this one: the core has no name alphabet. **The gateway half turned out worse than this row said** (L-121): asked the CORRECT way — delete `build`, then write `build/log.txt` — the write is still lost, because the consume takes entries before removals, so it is refused while the file is still there and dropped from the cell. The UI could not make a directory at all. An entry blocked by a path the cell is already removing is now deferred rather than dropped; one blocked by a REFUSED removal is still recorded and dropped, since nothing is coming to clear it. | S1 `Inv_HITLTracked`; "a boundary is a coherent point" | CONFIRMED | event-alphabet-7 |
| M6 | MEDIUM | On a collector-off store the leaked object blocks every create door for the path: `If-None-Match: *` → 412, no precondition → 428, rename-onto → `DestinationExists`, draft promote refused; inside the grace 409; an agent re-create preserves the garbage as a "foreign version" with an `upload-412-preserved` record naming a writer nobody was. **DISPOSED 2026-09-21: the mechanism is gone with the slot.** Every write lands at a fresh handle (`handle_key(path, flush)`), so no leaked object sits where a create door writes; the doors judge `If-Match`/`If-None-Match` against the TRACKED version — the cell's entry, else the document's citation (`workspace.rs::put_file` → `lookup` → `judge_preconditions`) — never against a HEAD of a shared key. The `upload-412-preserved` record it names no longer exists in the syncer, and the collector runs on every store (M2 above), so there is no leak to block anything. | SAFETY.md §2 "served to nobody", §4.7 "not a loss; a cost" | CONFIRMED | foreign-lifecycle-3, event-alphabet-8 |
| M7 | MEDIUM | `gateway_append`'s retry after its own landed CAS (`inbox.rs:207-216`) `retain`s away a NEWER entry for the same path that landed in between, and re-pushes the stale one; the next consume drops the newer acked write as superseded. Narrow window (SDK backoff vs a second client's full PUT). | S1 `Inv_HITLTracked` | PLAUSIBLE, unverified | store-outcomes-3 |
| L1 | LOW | SAFETY.md is stale at HEAD: §4.4 "neither run has been made yet" and §5.3 — commit `a42c3d40` ran both arms (`OrphanTrack=FALSE` VIOLATED as intended, 66M states; `TRUE` INCONCLUSIVE, disk guard at 158M); §3 says 116 runs / 28 strict, `check.sh` asserts 117 and COVERAGE.md 29; README line 16 says 116; the CI file's comment says 83. | claim currency | CONFIRMED; author | claim-adequacy-6 |
| L2 | LOW | SAFETY.md §4 names about two of the fifteen "not modelled" items the README names (atomic scan and two-scan rule; bare touch, min-interval, budget; an agent writing during sync; multi-gateway; window closing at the CAS vs after the GC; 412 arm parking; vanished base as a park; bytes coinciding with an unseen version; the grace escapes; 8 lost handoffs; the stronger ticket; the abort trace event; manifest as one object; whole-PUT vs compose; sockets/FIFOs/symlinks). | SAFETY.md preamble | CONFIRMED | claim-adequacy-7 |
| L3 | LOW | `Inv_NoFencedOkAck` and `Inv_NoDeposedPut` hold vacuously under the barrier lease (README says so; SAFETY.md S1/S5 list them as enforcers). §5.8 calls S2/S5 state-based; three of their rows are ghosts (`NoStaleOverride`, `NoStragglerInstall`, `NoDeposedPut`). | claim wording | CONFIRMED | claim-adequacy-5, -9 |
| L4 | LOW | A pointer CAS that lands with its response lost is reported `no_change: true` with `uploaded: n`; `note_boundary` is skipped and `installed_etag` is not journalled; the tree converges. `upload_one`/`upload_compose` treat a per-key 409 as fatal to the whole barrier. | AGENTS.md ack semantics; availability | CONFIRMED | store-outcomes-4, -5 |
| L5 | LOW | `boundary: "sentinel-deferred"` is stamped on a startup honour and on any floor-tick honour; `gauges.json` `last_boundary.source` is hard-coded `cadence` for every install. | AGENTS.md:152-155 | CONFIRMED | ack-and-reader-5 |
| L6 | LOW | The conflict COPY is durable in the bucket; the RECORD naming it is a pod-local file that rotates at 1 MiB and dies with the pod; nothing lists `conflicts/`. | S3 "with a record naming it" | CONFIRMED | foreign-lifecycle-4, event-alphabet-9(4) |
| L7 | LOW | Contract silences: chmod-only is invisible (`stat_changed` compares size and mtime, `scan.rs:36`); two names differing only in invalid UTF-8 collapse to one key (`to_string_lossy`, `scan.rs:74`) and one is never published. | AGENTS.md "with their mode bits", "Nothing is silent" | CONFIRMED | event-alphabet-9(1,2) |
| L8 | LOW | The adopted-own claim arm writes nothing, so a holder that re-adopts after losing all 8 handoff races starts its section on a token a waiter has been counting for a floor — the lease-2 rule applied to renew but not to claim. | availability; entry state of C2 | CONFIRMED | lease-and-fencing-5 |
| L9 | LOW | FINDINGS.md L-24 is carried OPEN on a reason that is false at HEAD (the model's 412 arm no longer parks; `Inv_NoStaleOverride` checks generation; `VerifyUploadedCitations` withholds the citation L-24 describes). What remains of it is C2. | ledger currency | CONFIRMED; **DONE 2026-09-21**: L-24 is closed in the ledger, with the reason — its mechanism was one key per path, and every write now lands at a handle nobody else writes; what can still take a peer's in-flight upload is the sweep, which is R4a/R4b with its own mutation worlds | claim-adequacy-10 |
| L10 | LOW | The chunk reaper's grace-vs-longest-publish rule (`manifest.rs:841-843`) and the fact that LeanSubtree models the manifest as ONE object are in neither §2 nor §4; the pointer is the CAS unit and the entries model stays faithful. | claim wording | CONFIRMED | claim-adequacy-8 |

## What the review did not reach (the critic)

- **`sync.rs`, `reader.rs`, the rescope arm of `checkout.rs`**: the three
  baseline writers OUTSIDE `barrier_inner`. These carry S3 rows 2-3 and
  S4 row 3 — the only SAFETY rows with neither a code-side HELD line nor
  a model run on the shipped shape (H8). Rescope holds the fence for its
  whole run and makes no cell writes, so M3 applies to it. One
  reviewer-hour; the single most valuable next check.
  **Two of the three read on 2026-09-21, and the critic was right.**
  `sync.rs` applied adds BEFORE remote deletions, so a remote generation
  that turned a file into a directory left the workspace with neither —
  the add refused for containment while the file was still there, then
  the file deleted. The passes act on disjoint path sets, so they were
  swapped, with a test. `reader.rs` (106 lines) read clean: its tick
  remembers the etags it read BEFORE the sync, so a document that moves
  during the sync is re-synced next tick rather than skipped, and a sync
  that fails leaves the memo unwritten. Nothing was found in it. The
  rescope arm of `checkout.rs` read clean too: the drop set is RECORDED
  in a durable intent rather than re-derived (a re-derivation loses the
  set the moment the uncite lands), the uncite is saved before a single
  file leaves the tree so a lost intent fails toward a re-uploadable
  local add rather than a published DELETE, the unlink demands the same
  containment the barrier does, a dirty path is KEPT rather than dropped
  so a replay cannot refuse forever, and the merge base stays the WHOLE
  manifest (C2's rule). Its door refuses an all-rejected scope instead of
  widening it to the tree — the bug `sync.rs` had shipped once.
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

## Applied 2026-09-19: H10, H2, H3, M7, H4, H5, H7

The same order: the finding re-verified at HEAD `9d157679` (a reading agent,
adversarial, which also widened three of them — H2's fence position, M7's
sweep route, H7's second window), the model world that fails as shipped
where the model can express it, the test against the unfixed tree, the fix,
then each fix line reverted alone.

| id | model | test (fails unfixed at the finding's assertion; each fix line reverted fails it again) | fix |
|---|---|---|---|
| H10 | `AckFromCarrier` (FALSE = shipped): each install whose barrier began with a declaration standing journals, with its CAS, the paths it published (`carryPaths`, and `carryDropped` for its outranked deletes); a restart keeps them; `AckDropped` adds a carried path whose deletion waits in the queue. `LeanBarrierLeaseAckAfterRestart` (the crash world's H10 route: one touch, one restart, no HITL) violates `Inv_AckImpliesCited` at 24; `…AckCarried` holds every sentinel invariant, `Inv_AckBoundaryCoherent` and `Inv_NoDeleteResurrected` (4,449,692 states, depth 32); `LeanProbeCarrierAck` shows the journal alone made an ack partial. **A first cut named the FIRST carrying install as the acked document; `…AckCarried` refuted it on `Inv_AckBoundaryCoherent` in 18 steps** (a restart between the CAS and step 7 made the re-run publish the agent's later write, and the ack named a document older than the tree). No document can satisfy both halves of the ack once a peer has deleted a declared path, so the honest answer is the latest document, `partial`. | `a_restart_between_the_install_and_the_ack_does_not_ack_a_peers_later_delete_as_the_declaration` (the model's route), `an_ack_write_that_failed_is_not_answered_later_with_a_peers_delete_as_the_declaration` (no restart), `the_floors_cadence_barrier_after_a_failed_honor_carries_the_whole_declaration` (the cadence barrier after a failed honor withheld the agent's declared delete) | `IntentJournal::carrier` (pending id, paths, dropped), written in the journal write after the CAS; `PendingSentinel::id`, fresh on every fold; `honor_publish` adds the carrier's drops and the carried paths with a queued deletion; `barrier_inner` treats a barrier that begins with a publish declaration standing as declared (the model's `DSet`) and stamps it `sentinel-deferred` (the model's `InstallSource`); `AGENTS.md`'s `partial` names the case |
| H2 | `FenceAfterLoad` (FALSE = shipped): the commit's one cell read is `VerifyCell`, a step of its own before the CAS, and the CAS carries no fence. `LeanBarrierLeaseStragglerLoadsSuccessor` violates `Inv_NoStragglerInstall` at 16 (claim, verify, stall; the successor deposes and rotates; the thawed holder CASes onto the rotated document); `…StragglerGCFenced` holds at its old count (276,442), so the arm preserves every other world. The restart that releases without rotating (the finding's route) is not modelled: `Claim` is one action. | `a_holder_deposed_during_its_commit_heads_does_not_install_onto_its_successors_document`, `a_restart_between_a_deposal_and_its_rotation_still_rotates`, `a_takeover_rotation_that_meets_a_409_retries_it` | the cell is read after each manifest load, before the CAS (the early read before the HEAD fan-out is gone: the publishing boundary stays at four cell requests); `release_stale_own` rotates when the cell's epoch is not the incarnation's; `rotate_for_takeover` retries a 409 |
| H3 | none: the model has no 409 | `a_ui_write_whose_inbox_append_meets_a_409_is_still_tracked`, `every_inbox_cell_cas_retries_a_409` (all nine loops, each exercised independently) | `inbox::lost_race` (412 or 409) in every loop on the cell |
| M7 | none (the sweep's append is not a separate model step) | `a_late_inbox_append_never_replaces_the_entry_of_a_newer_write` (the sweep's route: a UI write replaces the orphan between the sweep's listing and its append) | `gateway_append`: when the cell holds a different entry for the path, one HEAD; the entry naming the object at the key stays |
| H4 | `RemovalNamesGen` (FALSE = shipped) and `Inv_RemovalNamesItsVersion`; `gh.remJudged` stands for the cell's `Removal::etag` and is kept in the view (the removal worlds' state counts move, as they must). `LeanRemovalOverreaches` violates at 6 (remove, UI write, consume + apply); `LeanRemovalHolds` (360,007 states) and `LeanRemovalCrashHolds` hold with the arm on. | `a_declared_removal_never_deletes_a_ui_write_made_after_it`, `a_declared_removal_never_deletes_a_version_published_after_it_was_judged`, `a_rename_whose_source_changed_keeps_both_versions`, `a_declared_removal_of_a_ui_write_not_yet_integrated_waits_for_it` (the one deferral), control `a_declared_removal_of_the_version_it_named_is_performed`; gateway `verbs.rs` asserts the recorded etag for delete and rename | `Removal::etag` (the gateway's resolved version; the rename's source etag); `apply_removals` removes only that version — newer: `removal-refused-superseded`; still in the inbox: deferred |
| H5 | none: the model has no name alphabet | `the_upload_refuses_a_directory_swapped_for_a_symlink_after_the_scan` (atomicity-6's test, one directory up) | `safefs::open_beneath_nofollow`; `upload_one` takes the stamps, size and bytes from that descriptor; `file_crc` reads through it. **Residual CLOSED 2026-09-21**: `ComposeSpec` no longer carries a path at all — `local_path: &Path` became `local: Option<Arc<File>>`, and both backends read parts with `read_exact_at` on the caller's descriptor (`s3.rs::read_local`, `memory.rs::read_local`), so a `Local` part without a descriptor is refused before any part moves. The hole was wider than the original: the old reopen carried `O_NOFOLLOW` on the LAST component only and ran ONCE PER PART. Pinned by `the_compose_path_reads_the_descriptor_it_was_given_not_the_path_it_was_named` with a new `before_compose` hook that plants the swap in the window between the syncer's open and the store's read; unfixed it publishes `SECRET, outside the workspace` under `files/d/cfg.yaml`. Callers updated: lean's `upload_compose`, forge's `packio`, the CSI tier's flush. flint-store 37, lean/syncer 245, forge/syncer 175, csi tier 164 |
| H7 | none: the model's `Consume` is atomic | `the_consume_never_overwrites_an_agent_write_made_while_it_writes`, `an_agent_write_just_after_the_consume_is_published`; and in `sync`, which had both windows and judged dirt by one scan at its start: `sync_never_overwrites_an_agent_write_made_while_it_writes`, `an_agent_write_just_after_a_sync_applies_is_published`, `sync_never_deletes_an_agent_write_made_after_its_scan` — through a `#[cfg(test)]` window hook (nothing in either window calls the store) | `write_file_atomic_if`: the licence re-checked with the temp written, immediately before the rename (a write there takes the dirty arm — `consume_preserve_dirty`, or `sync-dirty`); the baseline records the written inode's fstat; `sync`'s remote delete re-checks a fresh stat. Residual: `sync`'s identical-bytes recovery arm reads, then stats |

Suites: syncer 253 and gateway 24 + 26 + 5 green on the Mac (the syncer's 250
before the `sync` fix also on the Linux box). The gate went
from 136 to 141 runs; the final module's deciders, gate, census and trace
replays are in `lean/formal/results/2026-09-19-review-final/` (gate 141/141
green, trace validation ok, census control identical to the 2026-09-18
baseline; the first attempt caught `gh.removalOverreach` missing from
`StrictGh` — fixed, re-run). H6 (the multi-writer checkout mix) is not a
contract patch: its structural fix — immutable object handles — is a design
of record with its model arm built and in the gate
(`docs/plans/flint-lean-immutable-objects-design.md`; the owner relaxed
fetch-by-bare-path everywhere and chose the model first). Until the code
tranches land, H6 stands as documented in `AGENTS.md`. H8/H9 and M1-M6 are
unchanged.

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
| M2 | ~~Decide the `Unknown` posture explicitly: collector OFF when the store is unverified (fail-closed, storage growth), or ON with a warning (today). Write it into SAFETY.md §2 either way.~~ **DONE 2026-09-21, the other way: the posture is gone.** Immutable handles removed the reason for it — the collector deletes handles nothing can cite again — so `conditional_delete_enforced` is deleted, the probe's DELETE arm only reports, and SAFETY.md §2 row 7 no longer claims the objects the collector gives way on. |
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

## 8. The flat `known` debt, repriced by reading the code (2026-09-21)

The debt carried out of 2026-09-20 was stated as "make `LeanSubtree`'s
`sc[s].known` per path", and costed as expensive: `known` is read at
twenty sites, a per-path field widens the state every one of the 166
worlds explores, and the fairness action would have to carry the update
too. That costing was wrong, because neither model's exemption is what
the code does.

The code's merge (`manifest.rs::merge`) has NO "mine" exemption at all.
It judges per path against the merge base and nothing else:

```rust
let changed = base.get(p).map(|b| b != &e.etag).unwrap_or(true);
if changed && !mine_upserts.contains_key(p) && !parked.contains(p) { foreign.push(..) }
if changed && mine_upserts.contains_key(p) { overridden.push(..) }
```

The exemption lives one level up, in the R7 surfacing loop
(`barrier.rs:1770`), and it is a test on the CURRENT baseline AT THAT
PATH — not a history, and not a set that spans paths:

```rust
let integrated = baseline.entries.get(path)
    .is_some_and(|be| be.key.as_deref() == Some(was.key.as_str()));
if integrated || parked.contains(path) { continue; }
```

So the faithful model of "mine" is `manifest[p] = sc[s].baseline[p]`, one
line, in both models:

- `LeanSubtree.ForeignEntry`: `manifest[p] \notin sc[s].known` becomes
  `manifest[p] # sc[s].baseline[p]`. No new field, no new constant, no
  cfg change, no state growth — `baseline` is already per path.
- `LeanCore.Foreign`: `Gen(doc[p]) \notin took[s][p]` becomes
  `doc[p] # w[s].baseline[p]`, and `took` stays where seven passes proved
  it necessary: `Inv_AckedNamed`, which asks what a tree EVER took in at
  a path, a question the merge never asks.

Both models then ask the same question and `LeanRefine` can substitute
`ForeignPerPath <- TRUE`, which is what retiring the debt means.

Three things must be checked before this is believed, and each is a box
run, not an argument:

1. The new test is STRICTER than flat `known` (more paths judged theirs),
   so `LeanCoreHoldsSmall` must be re-run: more foreign means more
   surfacing and more queueing, and `Inv_OneName` has not seen it.
2. It is also stricter than `took`, and `LeanSubtree`'s own comment at
   the CAS warns where that leads — "the path is queued into the inbox as
   a phantom conflict nobody else ever touched. TLC found this in shipped
   code." That counterexample was against removing the exemption
   ENTIRELY; the baseline test keeps its per-path core. If a world
   refutes it anyway, the finding is about the code, which does exactly
   this, and it is a real defect rather than a modelling artefact.
3. `LeanCoreForeignFlat` must still violate: it is the control that
   prices the flat rule, and a rewrite of the predicate it mutates can
   quietly stop firing.

### What the runs said (2026-09-21)

All three checks passed, and then the gate found something better than a
confirmation.

- `LeanCoreForeignFlat` still violates `Inv_AckedNamed` at depth 20, and
  `LeanCorePendingDropped` (L-119) at 19; the reachability probe still
  reaches the pending rule at depth 4.
- `LeanRefineQueue` holds, now mapping onto `ForeignPerPath <- TRUE`. It
  held at exactly its old 636,652 states while only R7's filter had
  moved; once the merge itself dropped its exemption and both models
  took the journal's merge base, the world settled at 606,916 — the
  difference is the foreign queueing the exemption used to suppress.
- `LeanCoreHoldsSmall` holds at every shape the day went through:
  47,550,294 states before it, 47,564,054 with R7's filter moved,
  47,472,288 once the merge itself dropped its exemption, and 48,995,156
  once the journal's own generation joined the writer's record. Depth 34
  throughout.
- **The gate stopped at `LeanSentinelRestart`**, a STRICT world, on
  `Inv_AckImpliesCited` at depth 18. The trace is the H1c window: A
  publishes p1, restarts between its CAS and step 7, and its next commit
  reads its OWN install as a peer's change — so the agent's delete of p1
  is outranked by "theirs" that is really A's, and the ack still says ok.

That counterexample is the session's real finding, and it is about the
MODEL, not the code. The model had the right rule and the wrong
mechanism. `MineIsNotForeign` is exactly this window's rule — its comment
names the crash between the CAS and step 7 — but it was implemented as
"is the generation in `sc[s].known`?", one flat set per writer holding
every generation it ever knew, anywhere. The code's answer is narrower
and lives somewhere else entirely: the intent journal's
`installed_etag`, written the instant the CAS returns and before the
deletes, and read at the merge — "if the bucket is still at the document
THIS workspace installed, that document IS the merge base, whatever the
persisted one says" (`barrier.rs::merge_onto`).

So the flat set was covering a mechanism the model did not have, and
covering far more besides — which is precisely what `LeanCoreForeignFlat`
prices. Asking per path took the cover away and the hole showed in eight
seconds.

The model now does what the code does, in three places rather than one:

- `MergeBase(s)` is the journal's rule, gated by `MineIsNotForeign` so
  the existing mutation world still refutes it.
- `ForeignEntry` has NO exemption — `manifest.rs::merge` has none either:
  changed against the merge base is theirs, full stop.
- R7's "never integrated" filter sits where `barrier.rs:1770` has it, on
  the CURRENT baseline at that path, and `ForeignPerPath` mutates THAT.

`LeanSentinelRestart` holds at 218,762 states, and
`LeanSentinelStaleMergeBase` — the rule's own mutation, which has always
described this route — still violates `Inv_AckImpliesCited` at depth 18.

One more thing had to move with it, and the refinement said so. Giving
the history model a merge base the core did not have broke
`LeanRefineQueue` at depth 22 — an action property, not an invariant:
a step of the history model that is no step of the core. The core has no
restarts, so the recovery looked like a history-model concern; it is not.
Step 7 rewrites `instBase` after the CAS in BOTH models, and a barrier
deposed in that window reads its own install as a peer's just the same.
The core keeps the document it installed (`w[s].inst`) and the pointer
generation it installed at (`w[s].seen`), so it could already say it:
`MergeBase(s) == IF w[s].seen = seq THEN w[s].inst ELSE w[s].instBase`,
read by `Foreign`, `MergeGone` and `Retired` exactly as the history model
reads its own. With that, the refinement passes again — and
`LeanCoreHoldsSmall` returns EXACTLY the state count it returned without
the rule, 47,472,288. That is not a null result: the core's own writers
never reach a state where the rule bites (its holder releases at step 7,
so there is no window), but the refinement evaluates the core's `Next` on
states MAPPED from the history model, which does reach them. A rule the
core's own worlds cannot see, and the refinement cannot do without.

And one ghost had to move with `contested`. `Inv_NoStaleOverride`'s
handles arm says "this commit cites its upload over a citation it never
integrated, and no record says so", but it was written as `foreign(p) /\
p \notin contested`. Once `contested` became R7's own filter, a path the
tree DID integrate satisfied both halves and the ghost fired for it:
`LeanImmutableQueueHolds`, a strict world, went red in forty-five
seconds. The ghost now carries the same filter as the record it is about,
so it never fires where a record is written and still fires for exactly
the unrecorded overrides — `LeanImmutableCasOverridesPeer` violates it as
before.

A fourth, and the sharpest of them. The gate then stopped at
`LeanBarrierLeaseLeakRestoreOnly` on `Inv_NoDeleteResurrected`: H1f's
rule restores the merge base to the generation a superseded deletion
retired, and the new merge base was THROWING THAT AWAY. The reason was a
field reused for two jobs. `instSeq` is set by a CAS, by a checkout AND
by a PULL-ONLY, because it names the document an ACK points at; the
code's journal records CASes only — `clear_intent_keys` runs on the
pull-only path and deliberately leaves `installed_etag` alone. So a
writer that had merely PULLED the current document was treated as having
installed it, and its restored base was discarded in favour of the
manifest. Both models now carry `jSeq`, written at a CAS and nowhere
else, and the merge base takes the document the POINTER holds — `theirs`,
exactly as `merge_onto` reads it, rather than a stored snapshot. With
that, `LeanBarrierLeaseLeakRestoreOnly` holds at 95,313 states,
`LeanImmutableQueueHolds` at 139,912, `LeanSentinelRestart` at 218,762,
`LeanRefineQueue` at 606,916, and `LeanSentinelStaleMergeBase` still
violates.

A fifth, and this one is an OPEN QUESTION ABOUT THE CODE. The gate then
ran an hour into `LeanImmutableRenameHolds` and stopped on `Inv_OneName`
at depth 18 — one handle cited under two names. The model's `KeepsAt`,
which answers R2's "does this install keep the handle at that other
path?", read a declared delete as removing the citation: its first
conjunct was `q \notin scanD`. But the merge only APPLIES a delete where
theirs is unchanged against the merge base — delete/modify resolves
conservative — and with the exemption gone more deletes are outranked.
So the install kept the handle at a path `KeepsAt` had written off, the
repair at the source re-cited it, and one handle had two names.
`KeepsAt` now mirrors `inst`, where `foreign(q)` is read BEFORE `q \in
scanD`.

**The code has the same shape and it is not yet known whether it is
reachable there.** `void_stale_repairs` (`barrier.rs`) computes "the
paths this install keeps that cite the handle" with `!deletes.contains(*q)`
— the same reading of a declared delete as an applied one, where
`manifest::merge` decides it by `theirs_unchanged`. An attempt to build
the route by hand did not reproduce it: the answered-record rule (L-117)
moves the handle out of a tree that holds it clean at the source, which
takes the repair away before the race can happen, and a tree DIRTY at the
source publishes an upload rather than a repair. Either the code is
protected by that rule and the model's route is one the code cannot
reach, or there is a third way in. Settling it needs the counterexample:
re-run `LeanImmutableRenameHolds` with the `KeepsAt` fix reverted, dump
the trace, and build the test from it. Until then this is a suspected
defect with a named line, not a finding.

**SETTLED 2026-09-22: it IS a defect, and it is FIXED — L-122.** The
counterexample came from r26 (BFS over the sandbox with conjunct 1
reverted): `Inv_OneName` at depth 18, 68,272,476 distinct / 254,212,763
generated — the shallowest route, which is what a test wants. DFID (r24)
was the wrong tool and timed out at level 13 after an hour, having gone
deep on a narrow spine; BFS found it in about 50 minutes.

The trace's configuration, and the test built from it: A re-cites `p1` at
the UI handle while B's merge base still holds `p1` at the seed, so B's
delete of `p1` — carried by the UI's rename of `p1` to `p3` — is
OUTRANKED, and B owes a repair at `p3` for that same handle. As shipped
the manifest ends up citing `files/p1.txt@ui-...` at BOTH names, with the
barrier report's own `outranked: ["p1.txt"]` proving the delete was
declined. `void_stale_repairs` now takes `baseline.inst_base` and a
declared delete only removes the citation where the merge would apply it.
251 tests green.

**Translating the trace, not transcribing it, was the load-bearing step.**
The first test written from the trace PASSED UNFIXED — worthless by the
standing rule. The harness said why: `removals_refused: 1`,
"removal-refused-superseded ... the newer version is kept". B still held
the seed bytes at `p1`, so the rename's removal was REFUSED and no delete
was ever declared; the fixture never reached the configuration. B has to
consume the UI write first. That is also a real MODEL/CODE DIVERGENCE
worth remembering: the model applies a removal over a tree copy that does
not match the version the removal names, where the code refuses it. The
model's route therefore cannot be replayed step for step — only its
configuration can.

**That run is now BUILT AND STAGED, 2026-09-22** (`~/r24.sh` on the box,
sandbox `~/lean-sandbox-keepsat/formal`, SANY-clean). It is a copy of the
gate tree differing from it by **exactly one line** — `KeepsAt`'s first
conjunct back to `q \notin sc[s].scanD`, the shape `barrier.rs`'s
`!deletes.contains(*q)` still has — under a cfg cut down to `TypeOK` and
`Inv_OneName` alone, so another invariant firing first cannot hand back
the wrong trace. It runs by DFID (`-dfid 26`, 2 workers) for r17's
reason: a must-violate world needs ONE route, and BFS must exhaust every
shallower level first — the FIXED world is ~58M distinct states at depth
18, which is where this counterexample lives. Deliberately NOT launched
while `LeanImmutableRenameHolds` itself is in flight; it is the first
thing to fire after it. The separate directory is also why this is safe:
the live run's module is never touched.

### PLANNED, AFTER THE GATE IS GREEN: restructure `KeepsAt` into the core's `CASE`

Decided 2026-09-22 after the third defect in this one rule. **The fix so
far is the third INSTANCE fix; this is the CLASS fix.**

The evidence is that `LeanCore` never had any of the three. Its `KeepsAt`
is an ORDERED CASE, so precedence must be stated; `LeanSubtree`'s is a
conjunction of three independent negations, so precedence is implicit and
each conjunct silently assumes the others did not already settle the
path. All three defects were precedence errors — an intention that
another mechanism had already cancelled. The core also SPLITS the
predicate so the repair arm can be gated without recursion:
`RepairHere` (non-recursive) and `RepairOwed == RepairHere /\ ~MovedElsewhere`.
`LeanSubtree` lacks that split, which is the whole reason conjunct 3
could not simply be gated on `RepairOwed`.

Drafted change (NOT applied — it is a real behaviour change at the edges,
which is exactly why it needs a green baseline to regress against):

```tla
RepairHere(s, q) ==                      \* the non-recursive half
  /\ q \notin (sc[s].scanU \cup sc[s].scanD \cup sc[s].parked \cup sc[s].surfaced)
  /\ sc[s].baseline[q] # sc[s].instBase[q]
  /\ HoldsGen(q, sc[s].baseline[q])
  /\ JudgedStands(s, q)
  /\ ~Entombed(s, q)
  /\ ~SupersededByUI(s, q)

KeepsAt(s, q, g) ==
  CASE q \in sc[s].upGone                  -> TRUE           \* withheld: moves nothing
    [] q \in sc[s].scanU \cap sc[s].upDone  -> sc[s].scanGen[q] = g
    [] RepairHere(s, q)                    -> sc[s].baseline[q] = g
    [] q \in sc[s].scanD                    -> ForeignEntry(s, q)
    [] OTHER                               -> TRUE

RepairOwed(s, p) == RepairHere(s, p) /\ ~MovedElsewhere(s, p)
```

Note the CASE is strictly more careful than the conjunction it replaces:
an upload SCANNED but neither withheld nor done falls through to OTHER
(it has moved nothing yet, so q keeps its citation), and a path in both
`scanU` and `scanD` is decided by the upload rather than by whichever
conjunct happened to fire.

**Verification this restructure owes, and it is not "the gate is green":**
a form change that closes a class must be shown still to CATCH that
class. All three known-bad worlds must still fire —
(1) conjunct 1's route: the `KeepsAtPreFix` sandbox, `Inv_OneName` depth
18 / 68,272,476 distinct (r26);
(2) conjunct 3's route: r23's world, `Inv_OneName` depth 21 /
182,220,773 distinct;
(3) conjunct 2's route: the core's two-path world, 2026-09-20.
Then the full gate, and `LeanRefine` specifically — this moves
`LeanSubtree` TOWARD `LeanCore`, which should help the refinement, but
"should" is the word that preceded `LeanRefineQueue` rc=13 last time.

**A census of the same shape, 2026-09-22 — the confusion is ISOLATED to
that one line.** Every site in `barrier.rs` that reads `classified.deletes`
(or the `deletes` it is passed as), classified by whether it treats a
DECLARED delete as an APPLIED one. The control arm is real: two sites
consult the applied view and carry a comment saying why, so a site that
conflated them out of ignorance would have looked different from these.

| site | reads | verdict |
| --- | --- | --- |
| `:1774` | `deletes.iter().filter(\|p\| merged.entries.contains_key(p))` — the MERGED document | **correct**, and commented: "an outranked delete is one the merged document still cites" |
| `:1881-1889` | branches on `installed.entries.contains_key(path)` → `outranked` vs `retired` | **correct**, and commented: "delete/modify resolved foreign-wins" |
| `:1397` (`repair_candidates`) | the declared set | **sound both ways**: if the delete is outranked theirs wins and the tree no longer holds that path, so no repair is owed; if it applies the path leaves the document. Errs conservative (repairs LESS) |
| `:2899` (`gone` in `merge_onto`) | the declared set | **sound both ways**: a path this install declared deleted is one its tree already lacks, so it needs no foreign-deletion queue entry either way |
| **`:2815`** (`cited_at` in `void_stale_repairs`) | the declared set, to decide whether ANOTHER path still cites the handle | **THE SUSPECTED DEFECT.** Errs permissive — it lets the repair proceed — which is the direction that can produce two names for one handle |

Reasoned by inspection, not by model or test: `:1397` and `:2899` are
argued sound, not proven so. What the census establishes is scope — the
fix is one line plus one parameter, not a pass over every delete site.
`baseline.inst_base` (path → etag, the merge base `manifest::merge`
compares against) is already in scope at the call site: it is handed to
`merge_onto` two statements later, `barrier.rs:1643`.

**AND THE CENSUS WAS SCOPED TOO NARROWLY — the class is `KeepsAt`-wide,
2026-09-22.** `LeanImmutableRenameHolds` (r23) ran to a COMPLETE state
graph of depth 21 and violated **`Inv_OneName` at depth 21**, 182,220,773
distinct / 718,826,927 generated. `Inv_RenameNoHole` never fired — that
correction IS verified, and the run walked past r21's 108,153,165-state
violation point — but the world does NOT hold. `KeepsAt` has three
conjuncts, each asking "does this install move `q` off the handle?", and
each has carried the same declared-vs-applied defect:

| conjunct | reads | status |
| --- | --- | --- |
| 1, deletes | `q \notin scanD` | fixed 2026-09-21 (`\/ ForeignEntry(s, q)`) |
| 2, uploads | `q \in scanU \ upGone` | fixed 2026-09-20 (`upGone`; core two-path world, `Inv_OneName` depth 21) |
| 3, the owed repair | `baseline[q] # instBase[q] /\ baseline[q] # g` | **STILL READS THE INTENT — this is what r23 fired on** |

Read off the violating state, not inferred: writer B, handle `g = 1` at
`p1` — `scanD = {}` (conjunct 1 true); `scanU = {p1}`, `upGone = {p1}`,
so `scanU \ upGone = {}` (conjunct 2 true); `surfaced = {}`,
`baseline[p1] = 2`, `instBase[p1] = 1`, `g = 1` (conjunct 3 FALSE).
So `KeepsAt(B, p1, 1)` is false, `MovedElsewhere(B, p3)` is false, the
repair at `p3` proceeds, and the final `manifest = (p1 :> 1 @@ p2 :> 1 @@
p3 :> 1)` cites one handle under two names. Yet B moves `p1` nowhere:
`RepairOwed(B, p1)` requires `p1 \notin scanU` and `p1 \in scanU`, so no
repair happens; and the upload at `p1` was withheld (`p1 \in upGone`).
Conjunct 3 asserted an intent the other two mechanisms had already
cancelled.

**The code's conjunct-2 behaviour was VERIFIED CORRECT — and a first
reading of `Aliased`'s comment as a bug report was MY misreading, kept
here because the ambiguity is worth knowing about.** The comment says
"the code reads the same thing, a withheld upload leaving `upserts`
before `void_stale_repairs` walks it". "Leaving" there means DEPARTING,
not REMAINING — it names the mechanism by which the code is right, and
`LeanCore.tla:552` says so without the ambiguity: "the code's
`void_stale_repairs` reads `upserts`, which a withheld upload has already
left". The verification below stands and confirms it; only the reading of
the comment was wrong. There are exactly TWO park sites, `barrier.rs:1583`
and `:1696`, and each is immediately preceded by an `upserts.remove` —
`:1552` (in HEAD since `79e7dac9`, predating the model finding) and
`:1691` (uncommitted, the path-clash arm of this workstream). A census of
`parked.insert` against `upserts.remove` finds no third site and no park
that leaves the path in the map. The one real window — `void_stale_repairs`
runs at `:1632`, BEFORE clash detection at `:1690`, so attempt 1 does walk
an upsert about to be withheld — is closed by the clash arm's
`attempt -= 1; continue`, which re-enters the loop and re-judges against
the corrected map. Whether the conjunct-3 route maps onto a code route at
all is NOT settled by this and must not be assumed either way: it needs
the refinement mapping, not assertion.

**AND THE RENAME WORLD IS STILL RED, ON A DIFFERENT INVARIANT.** With
`KeepsAt` mirroring `inst`, `LeanImmutableRenameHolds` no longer violates
`Inv_OneName` — it runs past depth 18, where that counterexample lived,
and stops at depth 19 on `Inv_RenameNoHole` after 108,153,165 distinct
states and 87 minutes. A performed rename ends with NEITHER name cited
and no entry left in the cell to cite the destination. The direction is
the one the change predicts: `MovedElsewhere` is now TRUE in more cases,
so more adoptions at a rename's destination are DECLINED, and a decline
that is not PENDING (L-119's rule needs a removal still WAITING at the
source) drops the entry. The counterexample, read: the UI renames p2 to
p3 and then WRITES p3 again by hand, so the rename's own version is
superseded at the destination before any barrier cites it; a writer
consumes the replacing entry (so it leaves the cell), its agent deletes
p3, and that delete is published. Neither name is cited and nothing in
the cell names the destination.

That is not a hole, and the invariant already knew it: its last disjunct
excuses exactly "the copy was overwritten by a later write to the new
name before any barrier cited it". But that disjunct is written
`~ImmutableObjects`, on the assumption — stated in its comment — that
under handles the entry clause covers the case, because a later write
REPLACES the entry. It does, while the entry waits; once a writer has
consumed it, it is gone. So the rule now carries the same exemption for
handles, spelled the way handles spell it: the destination's version is
in `gh.hitlRetired`, the stamp for "this acked version was retired
KNOWINGLY" — by a later UI write at the same name, or by a tree that
integrated it and then edited or deleted it. A destination LOST, which is
the harm this invariant is for, is stamped by nobody, so
`LeanRenameNoDestinationGuard` must still violate it — and it does.
`LeanRemovalHolds` and `LeanRemovalCrashHolds`, the other two worlds that
check the rule, hold. Both refinement worlds hold on this model:
`LeanRefineQueue` at exactly its 606,916 states and `LeanRefineProbe` at
exactly its 1,965,383 — neither reaches what the `KeepsAt` correction
changed, one having a single path and the other no UI write and no
removal.

**The big rename world itself is UNVERIFIED.** It was re-running when the
day ended and was stopped at depth 18 — 215 million states generated,
58.4 million distinct, no violation — one depth short of the 19 where the
previous run failed. So the exemption is argued, controlled by three
smaller worlds, and not yet proved on the world that produced the
counterexample. That run is the first thing to do next.

SO: THE GATE IS NOT GREEN. Four of the five corrections are verified by
their own worlds; the fifth is half-verified — it fixed what it was for
and left this behind. Everything in this section is the state of the work
at the end of 2026-09-21, not a finished result, and nothing here should
be read as "the model says the protocol is sound" until all 182 runs are
clean.

Five corrections from one change, every one found by a world rather than
by reading, and four of them in mechanisms nobody had asked about. That
is the argument for keeping the worlds, and for not trusting a model that
is green because a stronger rule elsewhere is covering for it.

## The constants census: what can be reduced, and what is load-bearing (2026-09-23)

The rename world would not complete. The question asked was whether its
constants can be cut "without affecting load-bearing components". A
constant is load-bearing here if REMOVING IT MAKES A KNOWN DEFECT
UNREACHABLE — which is a question with an experiment, not a judgement
call. The two known-bad runs on this exact world are the oracle:

- **r23** — same cfg, `KeepsAt` conjunct-3 defect present: `Inv_OneName`
  violated at **depth 21**, 182,220,773 distinct.
- **r26** — the same defect, BFS: `Inv_OneName` at **depth 18**,
  68,272,476 distinct.

Both counterexample traces were read state by state for what they
actually SPEND.

| constant | budget | spent by the known-bad | verdict |
|---|---|---|---|
| `MaxBarriers` | 2 | **2** (both traces) | LOAD-BEARING |
| `MaxHitl` | 1 | **1** (both) | at its floor |
| `MaxRemovals` | 1 | **1** (both) | at its floor |
| `MaxGen` | 3 | **3** (r23 mints gens 1, 2 and 3) | LOAD-BEARING |
| `Writers` | TwoWriters | both A and B | LOAD-BEARING |
| `FreePaths` | `{p3}` | the rename's destination | LOAD-BEARING |
| `MaxCrashes/Restarts/Syncs/Touches/Narrows/SameBytes` | 0 | — | already 0 |
| `MaxSeq` | 6 | **2** (r23), **3** (r26) | SLACK — but see below |
| `Paths` | `{p1,p2,p3}` | **p2 never moves** | the only candidate |

## `MaxSeq` is slack that cannot be spent

`MaxSeq` is the manifest CAS budget — how many times the pointer's
sequence token may advance. Five guards spell `manSeq < MaxSeq` and in
this cfg **three are dead**: `ClaimB` (1848) needs `~BarrierLease`,
`HitlWrite` (1980) needs `~ImmutableObjects`, `CitePassStep` (3596) needs
`GatedCitation`. Only the barrier's fused commit (3041) and a deposal's
rotation fence (3283) can charge it.

And commits are already throttled by something TIGHTER: `MaxBarriers`.
`gh.barriers < MaxBarriers` gates `Consume`, `LoadInbox` and `FastPath`,
and the module says so itself at 1489 — *"every claim follows a scan, so
the barrier budget bounds it"*. `manSeq` starts at 1, so two barriers put
the ceiling near 3, which is exactly where r26 lands.

**A guard that never fires does not change behaviour when you move its
threshold.** Lowering `MaxSeq` from 6 to 4 should remove no states at
all. It is not a lever; it is slack the barrier budget never lets the run
spend. (Unproven corner: `EpochBound = MaxBarriers + 2 = 4` caps claims,
so a deposal-heavy path could in principle reach ~7. Settling it costs
one small run at `MaxSeq = 4` compared per-depth against r27's progress
lines — but it would only confirm the no-op.)

## Why this world is the suite's long pole

It is **the only multi-path world in the suite that cannot use
symmetry.** `PathSym == Permutations(Paths)` (4846) is sound only where
the paths START interchangeable — the module's own note requires
`FreePaths` to be `{}` or all of `Paths`. 86 cfgs declare
`SYMMETRY PathSym`; every one of them sets `FreePaths = {}`. This world
needs an UNPUBLISHED destination for the rename, so `FreePaths = {p3}`,
so p3 is not interchangeable with p1 and p2, so the whole group is
unsound and the cfg correctly declares none. The rename/removal family
(~20 cfgs) all share that shape.

That points at the one reduction that would cost NOTHING in claim
strength: the sound group here is `Permutations({p1, p2})` — the
PUBLISHED paths, which do start identical (`manifest`, `objects` and
`versions` all hold gen 1 for both at `Init`). Order 2, so up to a 2x
cut, with no loss of coverage at all. It needs a module change (a
`SymPaths` CONSTANT, since a cfg cannot define an operator and the
module cannot name a model value), which touches all 198 cfgs and is
NOT a thing to do while a run is in flight.

### The one reduction available, and its known-bad control (r29, 2026-09-23)

Dropping p2 is an ORACLE RELAXATION, so it was tested against the UNFIXED
spec BEFORE being adopted — a smaller world that no longer contains the
defect would report "No error has been found", and that verdict would be
worthless ([[feedback_an_oracle_relaxation_needs_a_known_bad_run]]).

**r29** — `KeepsAtPreFix.cfg` (spec md5 `eafc60f0a62e88a9ad038941b5e6694c`,
the conjunct-3 defect PRESENT) at `Paths = {p1, p3}`, one line changed and
a `diff` guard refusing any cfg that differs beyond it:

```
Invariant Inv_OneName is violated
13,643,317 states generated, 4,055,236 distinct
```

It is the SAME counterexample, not a different route to the same
invariant:

| | r26 (3 paths) | r29 (2 paths) |
| --- | --- | --- |
| counterexample length | 17 states | 17 states |
| final `manifest` | `p1:>2, p2:>1, p3:>2` | `p1:>2, p3:>2` |
| `versions` | gen 2 under BOTH names | gen 2 under BOTH names |
| budgets | barriers 2, hitl 1, removals 1, nextGen 3 | identical |
| distinct | 68,272,476 | **4,055,236 — 16.8x smaller** |

**WHAT THIS LICENSES, AND WHAT IT DOES NOT.** It licenses: the 2-path
world still contains THIS defect, of this class, at the same length. It
does NOT license "p2 is irrelevant". p2 is not ignorable in general even
though its generation never moves — set-valued state reads every path
regardless, e.g. `prevScan == {q \in Paths : manifest[q] # 0}`. So the
2-path world is a NARROWER CLAIM, not a free win, and anything needing a
second PUBLISHED path (a delete of p2 racing a rename of p1 -> p3) stops
being checked. Adopt it as a gate member with that recorded, not silently.

### Two corrections made the same day

1. **"3 paths is load-bearing" was an assertion I had not checked.** It
   went into a durable note on 2026-09-22. Reading both counterexamples
   state by state shows p2 NEVER MOVES — `manifest`, `objects` and
   `versions` hold it at generation 1 across all 20 states of r23 and all
   17 of r26. `MaxBarriers` and the two writers ARE load-bearing; the
   third path was not, and I had lumped them together.
2. **The "disk wall" was arithmetic, not a machine.** I projected
   `267 GB / 7.25 h = 37 GB/h` as a steady rate and concluded the run
   would die at level 28 around 02:30. Wrong: that is a RAMP-UP AVERAGE.
   Disk tracks the QUEUE, not elapsed time, and TLC reclaims dequeued
   level files. MEASURED on r28: 246G -> 240G over four minutes while the
   queue GREW. At ~1.6 KB/queued state the projected peak (~238M states
   near level 30) is ~380 GB against 246 used and 475 free. **The binding
   constraint is TIME alone (~30-40 h), not space.**
