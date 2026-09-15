# lean findings ledger

Every defect found in lean: the checkout/publish syncer (`lean/syncer`, `lean/sidecar` before 2026-09-12), the
gateway (`lean/gateway`), their store and its double (`crates/flint-store`), and code defects the TLA+ model
(`lean/formal`) found. Compiled 2026-09-15 for W5 of `docs/plans/flint-lean-well-understood-plan.md`: the
finding rate per release and per campaign is computed from Table 1 in [Rate](#rate).

**Counts:** Table 1, 141 product defects (9 OPEN, 1 unreleased, 34 fixed before any published build
carried them). Table 2, 21 rig defects. Table 3, 15 model-only bug rows (21 bugs counting the three rows that each hold
three), and 6 abstractions that hid code defects (Table 3b).

**Columns.**
- **id**: the project's own name where one exists (a protocol-review id such as `inbox-1`, a plan-review id
  `C1`-`C6`, a writer-lease finding `F1`-`F8` or `finding 10`-`13`); otherwise `L-<n>`. An `L-<n>` in
  parentheses is the ledger's own number for a named row.
- **found**: the date the defect was found, or the fix commit's date when the finding date is not recorded.
- **found by**: `model` (TLC), `live drill` (a cluster: kind or EC2, deployed path), `host leg` (a process
  or rig against a real or fake store, outside a cluster), `test` (the battery or a guard test),
  `code reading`, `review/audit` (a named review or audit campaign). "(unverified)" means the source states
  the defect but not how it was found.
- **class**: `data loss` (acked or published bytes lost, deleted or silently wrong), `contract` (the agent,
  operator or packaging contract says or does something false; includes doc-only contract corrections
  marked "(doc)"), `convergence` (a tree never receives a change it should), `availability` (a wedge,
  hang, crash loop or OOM), `performance`, `security`.
- **fix commit**: short hash; `OPEN`; or "(unverified)" when the fix was not traced to a hash.
- **shipped in**: the first release tag containing the fix. "never shipped broken" marks a defect found and
  fixed during that release's development.
- **pinned by**: the test, cfg or drill leg that fails on the defect, as the source names it.

Citations: `CHANGELOG.md:<line>`; "review doc" is `docs/plans/flint-lean-protocol-review-2026-09-12.md`;
"assessment" is `docs/plans/flint-lean-writer-lease-and-gated-assessment.md`; "plan" or "boundary-verbs plan"
is `docs/plans/flint-lean-boundary-verbs-plan.md`; "README" is `lean/formal/README.md`. Line numbers are as
of HEAD `86d731b3` plus the working tree of 2026-09-15.

## Table 1 — product defects

| id | found | found by | class | summary | fix commit | shipped in | pinned by |
|----|-------|----------|-------|---------|------------|------------|-----------|
| L-1 | 2026-09-15 | model | data loss | A UI write that re-created a path another writer had deleted could be lost: the writer-local queue applied a peer's deletion after the inbox's adoptions, the window clear dropped the inbox entry, and the acked bytes stayed cited by no manifest (since 1.52.0). Tranche 7, the first run that modelled the writer-local queue (lean/formal/README.md:883). CHANGELOG.md:17 | uncommitted (working tree: `lean/syncer/src/barrier.rs`) | unreleased | `a_ui_write_over_a_peers_delete_survives_the_queued_tombstone`; cfg `LeanBarrierLeaseQueueTombstoneOverHitl` (`Inv_HITLTracked`, 19 steps) |
| L-2 | 2026-09-14 | model | contract | A publish ack said `ok` for a boundary that did not carry the agent's change, route (1): the agent deleted a file another writer had changed that had not reached its tree; delete/modify resolves foreign-wins but the ack named a seq still citing the file. Now `partial` with the path in `report.dropped`. TLC box 2026-09-14 (`results/2026-09-14-sentinel-box/`, lean/formal/README.md:757). CHANGELOG.md:251 | 231cff00 | v1.54.0 | `a_publish_whose_delete_lost_to_a_peers_edit_is_partial` and three siblings; cfg `LeanBarrierLeaseSentinelOutrankedOk` (must-fail, `Inv_AckImpliesCited`) |
| L-3 | 2026-09-14 | code reading (unverified) | contract | Publish ack said `ok` over an upload that published nothing (large file drifted mid-transfer, or its assembly was swept). CHANGELOG.md:259 | 231cff00 | v1.54.0 | syncer test using `MemoryStore::inject_compose_swept` |
| L-4 | 2026-09-14 | code reading (unverified) | data loss | The drain attested a boundary missing such a file (routes of L-2/L-3); an attested drain lets the node remove the tree holding the only copy. Drain now refuses; the binary's retry publishes the path. CHANGELOG.md:262 | 231cff00 | v1.54.0 | syncer drain test (one of four) |
| L-5 | 2026-09-14 | live drill | availability | A fence handoff that raced the queue left the cell held: every enqueue moves the cell token, so the handoff could 412 twice (read as "deposed") or get S3 409 ConditionalRequestConflict (read as failure); 0-6 handoffs lost per run in every build since v1.52.0, queue waited out the 60 s deposal. Contention drill on real S3. CHANGELOG.md:370 | 7f6f5e71 | v1.53.0 | `a_handoff_that_races_the_queue_still_hands_the_cell_on` |
| L-6 | 2026-09-14 | live drill | performance | The chunk reaper HEADed every unreferenced chunk it would keep, one at a time, inside the fence: 700-920 ms of a 1.0-1.3 s commit hold, growing with the last hour's publishes. CHANGELOG.md:357 | 8f72afe3 | v1.53.0 | `the_chunk_reaper_does_not_head_what_the_listing_shows_inside_the_grace` |
| L-7 | 2026-09-14 | live drill | performance | A pull-only boundary (manifest moved, nothing of its own to publish) still claimed the fence, opened/cleared the HITL window and handed off: 65 of 191 claims in the writers drill, 18 requests where 4 GETs suffice. CHANGELOG.md:336 | 6d8e49c9 | v1.53.0 | `a_pull_only_boundary_takes_no_fence_and_writes_nothing`, `a_pull_only_boundary_inside_a_peers_commit_section_converges` |
| L-8 | 2026-09-14 | live drill | performance | The fence queue head polled at 1 s: a released, reserved cell stood idle a median 614 ms before the head claimed it; claim wait 7.2 s p50. CHANGELOG.md:343 | b58a374e | v1.53.0 | `only_the_queue_head_is_told_to_poll_fast` |
| L-9 | 2026-09-14 | code reading (unverified) | contract | flint-store shared-prefix warning said "one prefix has exactly one writer", false for a lean workspace since v1.52.0 (a log message; it detects two products on one prefix). CHANGELOG.md:383 | 095cc18a | v1.53.0 (flint-store 0.1.2) | none named |
| F1 / finding 1 (L-10) | 2026-09-13 | model | data loss | Multi-writer (1): the garbage collector HEADed an object, recognized its etag and deleted it unconditionally, so a peer's upload of the same path landing between the two was deleted and then cited. Delete now carries `If-Match` (`ObjectStore::delete_if_match`). CHANGELOG.md:497 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:552 | b50c2faf | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `a_peer_upload_between_the_gc_head_and_its_delete_is_not_deleted`; cfg `LeanBarrierLeaseGCUnconditional`; host leg H1 |
| F2 (L-11) | 2026-09-13 | model | data loss | Multi-writer (2): an upload that found its bytes already at the key cited that etag with no lease held; the peer's collector could remove it first. Observed citations are re-read inside the commit section, withheld when gone (`adopt-withheld`). CHANGELOG.md:511 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:553 | b50c2faf | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `an_adopted_upload_deleted_by_the_peer_before_the_claim_is_not_cited`; cfg `LeanBarrierLeaseAdoptBlind`; host leg H2 |
| F3 (L-12) | 2026-09-13 | model | convergence | Multi-writer (3): `sync` advanced its merge base to the manifest for a path whose remote truth came from an older inbox entry, skipping a change the tree never received. CHANGELOG.md:517 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:554 | b50c2faf | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `a_sync_does_not_advance_its_base_past_a_change_the_inbox_hid`; cfgs `LeanBarrierLeaseSyncOverlayStale` / `LeanBarrierLeaseSyncOverlayHolds`; host leg H3 |
| F5 (L-13) | 2026-09-13 | model | convergence | Multi-writer (4): a writer's merge queued the peer's changes as `merge-preserved` entries in the SHARED inbox, where the peer's consume found its own bytes and dropped them, so the writer needing them never converged. Now a writer-local `foreign-queue.json`. CHANGELOG.md:520 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:555 | b50c2faf | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `a_peers_change_reaches_the_writer_whose_merge_queued_it_even_if_the_peer_consumes_first` (queue not modelled) |
| F6 (L-14) | 2026-09-13 | model | convergence | Multi-writer (5): a peer's DELETE never reached the other writer's tree. The queue now carries deletions (`consume-foreign-delete-vs-dirty`). CHANGELOG.md:526 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:556 | b50c2faf | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `a_peers_delete_reaches_the_other_writers_tree` |
| F7 (L-15) | 2026-09-13 | model | performance | Multi-writer (6): two idle writers traded empty manifest generations (and fence claims) every tick; a barrier whose merge adds nothing now installs nothing. CHANGELOG.md:529 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:557 | b50c2faf | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `two_idle_writers_do_not_trade_empty_generations` (seq 5 -> 13 in 8 idle barriers; model has no empty-install rule) |
| F8 (L-16) | 2026-09-13 | model | data loss | Gateway: a UI write overwrote a writer's upload its commit had not cited yet (PUT conditional on the key's CURRENT etag); the uploading writer's commit re-cited its own generation over it, leaving the UI's acked write uncited and the manifest citing bytes the key no longer held. `put_file`/`promote_draft` overwrite only a tracked version, else 409 `concurrent-write`. CHANGELOG.md:544 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:558 (the seventh place, found by the full formal gate after the first six were fixed) | 75bdff42 | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `a_ui_write_over_an_uncited_upload_is_never_silently_lost`; gateway `a_blind_write_over_an_uncited_upload_is_refused_until_it_is_cited`; model `HitlOverwritesTrackedOnly`, cfg `LeanBarrierLeaseHitlOverUncited` (`Inv_HITLDurable`) |
| finding 11 (L-17) | 2026-09-13 | host leg | availability | An upload whose base a peer's GC removed wedged the writer on S3 forever (`put_whole: 404 NoSuchKey`): S3 answers If-Match on a missing key with 404, the "vanished base is a create" rule sat behind the 412 arm only. Writers drill host leg H2 on real S3. CHANGELOG.md:558 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:568 | ed7de78e | v1.52.0 (whether a published build could reach it: unverified) | vanished-base test against both 404 and 412 answers, both upload paths, mutation-checked |
| finding 12 (L-18) | 2026-09-13 | live drill | data loss | Gateway: a UI write refused after its PUT (409 `barrier-window-open` on the inbox append) destroyed the tracked version it replaced (a peer's acked cited upload, or the UI's own earlier acked write). Append now waits for the window and runs as its own task. Writers drill leg A2 (six writers + UI, real S3), twice in five minutes. CHANGELOG.md:573 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:611 (runcv deployed storm) | 79e7dac9 | v1.52.0 (whether a published build could reach it: unverified) | three tests in `lean/gateway/tests/verbs.rs` (hook store opens the window at PUT) |
| finding 13 (L-19) | 2026-09-13 | live drill | data loss | A writer's upload of bytes identical to a version a peer just deleted was collected (same MD5 etag) and then cited; a fresh checkout refused the workspace. Commit section now re-reads every citation it adds. Writers drill leg A3 (churn, real S3). CHANGELOG.md:595 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:639 (runcv deployed storm, `churn/p14.txt`); modelled after the fact as `LeanBarrierLeaseSameBytesUnverified` (`Inv_NoDangling`, 18 steps) | 79e7dac9 | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `a_peer_upload_of_identical_bytes_before_the_gc_delete_is_not_deleted` |
| finding 13, second route (L-20) | 2026-09-13 | model | data loss | Second same-bytes route: a peer's upload of new bytes landed If-Match the unchanged etag over the rewrite and committed, then the rewriting writer's commit cited its own version over it, leaving the peer's committed edit uncited. CHANGELOG.md:611 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:664 | 79e7dac9 (fix); df2d5555 (model found it) | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `a_peer_put_over_an_identical_bytes_upload_is_not_cited_as_the_old_version`; cfg `LeanBarrierLeaseSameBytesOverride` (`Inv_NoStaleOverride`); `LeanBarrierLeaseSameBytesDeep` holds (382.7M states) |
| atomicity-5 / inbox-4 / barrier-7 (L-21) | 2026-09-12 | review/audit | convergence | A restart between the manifest CAS and step 7 (queueing the peers' changes) left the merge base at the installed document, so the peers' edits/deletes never reached this tree. Now journalled in `intent.json`. The 2026-09-12 protocol review's deferred barrier-7 item, reachable only with a second manifest writer. CHANGELOG.md:622 | 8792c5f7 | v1.52.0 | `a_restart_between_the_cas_and_step_7_still_delivers_a_peers_change` and `..._delete` |
| L-22 | 2026-09-13 | model | contract | `.flint/remote.seq` `integrated_seq` advanced to the installed seq while queued peer changes still waited for the next consume, so the ticker said "no news" about a tree that lacked them. Sentinel world depth 19 (`Inv_AckBoundaryCoherent`), i4i.2xlarge TLC box 2026-09-13; lean/formal/README.md:739. CHANGELOG.md:639 | 8792c5f7 | v1.52.0 (never shipped broken: introduced by the per-barrier fence in the same release) | `remote_seq_reports_news_while_a_peers_change_waits_in_the_queue`; cfg `LeanBarrierLeaseSentinel` |
| L-23 | 2026-09-13 | code reading (unverified) | availability | Opt-in `uploadPartParallelism` (v1.51.0) had no byte bound: every part and whole body read whole into memory, peak RSS `min(large objects, fanout) x parts x part_size`, 16 GiB at 8 wide. Now `ByteGate` / `uploadInflightMb` 256. CHANGELOG.md:652 | b50c2faf | v1.52.0 | fake-S3 RSS measurement (no named test) |
| L-24 | 2026-09-13 | test (unverified) | convergence | OPEN (known limitation v1.52.0): a writer that supersedes a peer's not-yet-cited upload leaves that peer's commit citing a generation the key no longer holds until the superseding writer commits (bytes preserved). Not repeated in the v1.53.0/v1.54.0 limitations, but the assessment (docs/plans/flint-lean-writer-lease-and-gated-assessment.md:560) calls it still open and the model hides it (its 412 arm parks; `Inv_NoDangling` checks existence, not generation). CHANGELOG.md:707 | OPEN | — | none |
| finding 10 (L-25) | 2026-09-13 | model (unverified) | convergence | OPEN (known limitation v1.52.0-v1.54.0): a writer lost for good between an upload and its commit leaves an uncited object that the manifest, a fresh checkout and the live trees disagree about until a writer rewrites the path. No acked write lost. CHANGELOG.md:702 docs/plans/flint-lean-writer-lease-and-gated-assessment.md:595 | OPEN | — | `a_writer_killed_after_its_upload_does_not_leave_the_trees_diverged` (`#[ignore]`, fails today); host leg H5 reproduced it |
| L-26 | 2026-09-13 | live drill | availability | OPEN (known limitation v1.52.0-v1.54.0; introduced by the L-18 fix): a gateway UI write can wait up to the window deadline (180 s) behind a barrier that died mid-window. CHANGELOG.md:710 | OPEN | — | none |
| L-27 | 2026-09-13 | host leg | data loss | OPEN (known limitation v1.52.0-v1.54.0): on Ozone 2.2.x a second writer's GC can delete the first writer's upload (Ozone ignores `If-Match` on DeleteObject, HDDS-14907); the syncer does not refuse the configuration. `probe-conditional` on Ozone 2.2.1 s3g plus an AWS CLI control (assessment:581). CHANGELOG.md:698 | OPEN | — | `probe::probe_conditional_delete` detects it (does not refuse) |
| L-28 | 2026-09-12 | test | contract | Gateway 0.2.0: an overwrite of a cited file read 409 `moved` until the syncer re-cited it (`get_file` preferred the citation over the tracked inbox entry), for every reader for up to a barrier, and forever with no syncer running. A battery leg had documented the 409 as expected. CHANGELOG.md:975 | ebb42b30 | v1.51.0 (gateway 0.2.1) | one API test with both mutations as controls; the battery leg now asserts the bytes |
| L-29 | 2026-09-12 | model | data loss | A consumed HITL write could be orphaned by a pod replacement during the upload phase: consumed inbox entries were dropped at the window-open CAS ("durably in the baseline"), but a pod replacement's emptyDir takes the baseline; the acked write was never cited and nothing tracked it. Found while modelling rename (phase E). CHANGELOG.md:989 | be90f785 | v1.51.0 | `a_consumed_hitl_write_survives_pod_replacement_before_the_cas` (old rule as mutation) |
| L-30 | 2026-09-12 | host leg | performance | Small-file read ceiling: every fetch future was polled by ONE `buffer_unordered` task on the main thread (~3,300 files/s plateau from fanout 128 to 512); with drivers added, musl malloc's process-wide lock burned 3x CPU per file. i4i.xlarge vs real S3. CHANGELOG.md:1017 | 087d5c43 | v1.51.0 | `lean/e2e/perf/results/fanout-ceiling-2026-09-12.md` (measurement, no test named) |
| L-31 | 2026-09-12 | host leg | performance | Four syscalls per materialised file that bought nothing (20,006 ENOENT `unlinkat` per 20,000-file checkout, redundant `create_dir_all`, six `stat`s per file). CHANGELOG.md:1034 | a58a4843 | v1.51.0 | a test that fails with either retry arm deleted |
| L-32 | 2026-09-12 | host leg | contract | Parent-directory race: two parallel siblings creating one missing parent; the loser's `EEXIST` was reported as a containment REFUSAL and the checkout completed ("19999 materialized, 0 present") with one file missing. 5 of 12 runs. CHANGELOG.md:1137 | 087d5c43 (test introduced there) | v1.51.0 | `siblings_racing_to_create_one_parent_do_not_refuse_each_other` |
| L-33 | 2026-09-12 | code reading (unverified) | data loss | A fresh fetch was not verified against the manifest's CRC-64 (only the resume path was), so a wrong body under the cited etag (bit-rot, broken gateway, wrong cache object) was written, cited in the baseline and read as the file; Ozone returns no checksum. CHANGELOG.md:1068 | d1539fb9 | v1.51.0 | three mutation controls |
| L-34 | 2026-09-12 | code reading (unverified) | data loss | The three citation repairs took the manifest CRC-64 from a HEAD, which Ozone never returns, so on Ozone no adopted file carried a CRC and the L-33 check was empty exactly where the backend attests nothing; the gated lane's repair had NO test (a wrong CRC there passed the whole battery). CHANGELOG.md:1082 | 328bbf5d | v1.51.0 | `AttestsNoChecksum` double; six mutation controls |
| L-35 | 2026-09-12 | live drill | performance | Raw read path (off by default): the SDK's bare credential chain is IMDS on EC2 and was asked per request; "the one defect the tests could not" find. Fixed by a single-flight cache before shipping. CHANGELOG.md:1120 | 9bbf021b (unverified: fix may be a later commit) | v1.51.0 (never shipped broken: the raw read path is new in this release) | ten tests against an in-process server (none named for this defect) |
| L-36 | 2026-09-12 | live drill | availability | Default read window 512 MiB in flight: the deployed lean worker (1Gi plugin-wide limit) was OOM-killed checking out 6 x 1 GiB, three times; host arms ran with no cgroup and never saw it. Deployed-door drill. CHANGELOG.md:807 | 131b575f | v1.51.0 | `VmHWM` measurement (no test named) |
| L-37 | 2026-09-13 | code reading (unverified) | contract | `flint-lean-gateway` 0.2.2 required `flint-lean ^0.2.0`, unsatisfiable since the 0.3.0 bump. CHANGELOG.md:802 | 3fb4f2d2 | v1.51.0 (gateway 0.2.3) | none |
| atomicity-1 | 2026-09-12 | review/audit | availability | CRITICAL: the manifest cited the scanned size while the upload carried the file as it stood at read time; a file growing during a tick was cited short and every fresh checkout of an entry over 16 MiB failed its CRC fold, successor pod exiting 1 in a loop (bytes intact). Live-tested on runcu. review doc:90 | 4fc8bee6 | v1.51.0 | `the_manifest_cites_the_uploaded_length_not_the_scanned_one` |
| ack-1 / inbox-3 | 2026-09-12 | review/audit | availability | HIGH: an invalid `sync` scope was a permanent unacked error returned ahead of the publish honor and the cadence barrier, stopping every boundary for the workspace's life. review doc:91 | 4fc8bee6 | v1.51.0 | `an_invalid_sync_scope_is_acked_refused_and_never_wedges_publish`, `a_refused_sync_pending_does_not_stop_the_cadence_barrier` |
| atomicity-2 | 2026-09-12 | review/audit | data loss | HIGH: the compose 412 recognizer ignored `prior_uuids`; a crash mid-compose (> 64 MiB) plus an edit parked the path forever. review doc:92 | 4fc8bee6 | v1.51.0 | `a_crashed_compose_is_adopted_after_an_edit_not_parked_forever` |
| atomicity-3 / inbox-2 | 2026-09-12 | review/audit | data loss | HIGH: the consume overwrote an agent write landing between the dirty stat and the rename (during its fetch), with no record. review doc:93 | 4fc8bee6 | v1.51.0 | `a_consume_never_overwrites_a_write_that_landed_during_its_fetch` |
| atomicity-4 | 2026-09-12 | review/audit | data loss | HIGH: a same-size rewrite inside the scan's second was invisible forever (the make/rsync trap). review doc:94 | 4fc8bee6 | v1.51.0 | `a_same_size_rewrite_within_the_scan_second_is_still_published` |
| inbox-1 | 2026-09-12 | review/audit | data loss | HIGH: a path parked on a foreign version never un-parked; acks said `ok` with `parked: n`, and the drain attested so the node removed the tree with the agent's only copy. review doc:95 | 4fc8bee6 | v1.51.0 | `a_parked_path_is_preserved_and_published_over_not_abandoned`, `a_drain_never_attests_a_boundary_with_parked_paths` |
| atomicity-6 | 2026-09-12 | review/audit | security | HIGH (security): a file swapped for a symlink between the scan and the upload published the link's target, `/proc/self/environ` included. review doc:96 | 4fc8bee6 | v1.51.0 | `the_upload_refuses_a_symlink_swapped_in_after_the_scan` |
| gated-1 | 2026-09-12 | review/audit | data loss | HIGH (gated mode, removed v1.52.0): a consume-dirty inbox entry was never dropped after the citation that superseded it and was consumed again over the agent's published edit. review doc:97 | 4fc8bee6 | v1.51.0 | `a_consume_dirty_entry_leaves_the_cell_at_the_citation_that_supersedes_it` (gated code removed in 26b75637) |
| gated-2 | 2026-09-12 | review/audit | data loss | HIGH (gated mode): a withheld tombstone survived a same-stat return; the citation dropped the file and GC deleted its object. review doc:98 | 4fc8bee6 | v1.51.0 | `a_same_stat_recreated_file_survives_its_withheld_tombstone` (gated code removed in 26b75637) |
| ack-2 / gated-4 | 2026-09-12 | review/audit | contract | MEDIUM (doc): `sentinel-deferred` was stamped on any floor-tick honour; the contract said budget only. review doc:99 | 4fc8bee6 (doc) | v1.51.0 | none (doc) |
| inbox-5 | 2026-09-12 | review/audit | availability | MEDIUM (plausible): a 412 whose object was gone failed every barrier forever. Fixed for the 412 answer only; S3's 404 answer stayed broken until L-17. review doc:100 | 4fc8bee6 | v1.51.0 | `a_412_on_a_vanished_object_recreates_instead_of_failing_forever` |
| inbox-7 | 2026-09-12 | review/audit | contract | MEDIUM: an unscoped `sync` coalesced with a scoped one was honoured as the scoped one (silently narrowed). review doc:101 | 4fc8bee6 | v1.51.0 | `an_unscoped_sync_touch_widens_a_coalesced_scope_to_the_whole_tree` |
| inbox-8 | 2026-09-12 | review/audit | security | MEDIUM: containment refused `.flint/` but not `.flint-sync/`; a planted citation could be materialised into the state directory. review doc:102 | 4fc8bee6 | v1.51.0 | `containment_refuses_the_state_directory_too` |
| gated-3 / lease-6 | 2026-09-12 | review/audit | contract | MEDIUM: a restarted claimant waiting behind a live holder left the marker `live`; a raw touch was never consumed or refused. review doc:103 | 4fc8bee6 | v1.51.0 | `a_waiting_claimant_refuses_a_raw_touch_and_flips_the_marker` |
| lease-1 | 2026-09-12 | review/audit | contract | MEDIUM: a fence whose local settle failed (ENOSPC on `.flint/`) swallowed `Fenced`: an immortal leaseless syncer behind a `live` marker. review doc:104 | 4fc8bee6 | v1.51.0 | `a_fence_whose_settle_fails_is_still_a_fence` |
| lease-2 | 2026-09-12 | review/audit | availability | MEDIUM (plausible): adopting a lost-renew token wrote nothing, so the token stood still for a takeover threshold and a waiting challenger could count a live holder dead. review doc:105 | 4fc8bee6 | v1.51.0 | `adopting_a_lost_renew_token_moves_the_cell` |
| lease-3 / audit #7 | 2026-09-12 | review/audit | contract | MEDIUM: self-recognition by `holder_id` alone skipped the takeover rotation after a lost acquire response (or a rotation that failed after the acquire). review doc:106 | 4fc8bee6 | v1.51.0 | `a_lost_acquire_response_still_rotates` |
| ack-3 / atomicity-8 / inbox-6 | 2026-09-12 | review/audit | contract | LOW: publish acks never carried `report.conflicts`. review doc:108 | 4fc8bee6 | v1.51.0 | `a_publish_ack_carries_the_boundarys_conflict_records` |
| ack-4 | 2026-09-12 | review/audit | contract | LOW (doc): `boundary: "drain"` was emitted and undocumented. review doc:109 | 4fc8bee6 (doc) | v1.51.0 | none (doc) |
| ack-5 / gated-5 | 2026-09-12 | review/audit | contract | LOW (doc): `cadence` mode documented "(no verbs)"; the code honoured them. review doc:110 | 4fc8bee6 (doc) | v1.51.0 | none (doc) |
| ack-6 | 2026-09-12 | review/audit | contract | LOW (doc): `conflicts.jsonl` rotates at 1 MiB, the contract said "every conflict ever". review doc:111 | 4fc8bee6 (doc) | v1.51.0 | none (doc) |
| ack-7 | 2026-09-12 | review/audit | contract | LOW: `conflicts_since` under-reports when the log rotates mid-sync. Deferred (noted in `sentinel.rs`). review doc:112 | OPEN (deferred) | — | none |
| ack-8 | 2026-09-12 | review/audit | contract | LOW (doc): empty vectors omitted from acks; the doc's example showed `[]`. review doc:113 | 4fc8bee6 (doc) | v1.51.0 | none (doc) |
| ack-9 | 2026-09-12 | review/audit | contract | LOW (plausible, doc): an agent timestamping its touch with `date` can never match its solitary ack by the mtime rule on a coarse clock. review doc:114 | 4fc8bee6 (doc) | v1.51.0 | none (doc) |
| ack-10 / U37 | 2026-09-12 | review/audit | contract | LOW: a touch consumed into the staging file but not folded is clobbered by the next consume after a transient `save_pending` failure. Deferred (needs ENOSPC injection). review doc:115 | OPEN (deferred) | — | none |
| ack-11 | 2026-09-12 | review/audit | contract | LOW (doc): the contract did not require a fresh nonce per touch. review doc:116 | 4fc8bee6 (doc) | v1.51.0 | none (doc) |
| atomicity-7 | 2026-09-12 | review/audit | contract | LOW: a crash-orphaned `*.flint-sync-tmp` was published. review doc:117 | 4fc8bee6 | v1.51.0 | `an_orphaned_consume_temp_is_never_published` |
| atomicity-9 / csi-6 | 2026-09-12 | review/audit | availability | LOW: the operator's stale-MPU sweep aborted uploads whose `Initiated` the store omits (operator `reconcile.rs`, outside the syncer). review doc:118 | 4fc8bee6 | v1.51.0 | none (one-line `unwrap_or(false)`) |
| gated-6 | 2026-09-12 | review/audit | contract | LOW (doc): gated applied the inbox at every floor tick, not "only at a boundary". review doc:119 | 4fc8bee6 (doc) | v1.51.0 | none (doc) |
| gated-7 | 2026-09-12 | review/audit | contract | LOW: a fenced marker carried no `reason`. review doc:120 | 4fc8bee6 | v1.51.0 | `a_fenced_marker_says_why` |
| gated-8 | 2026-09-12 | review/audit | contract | LOW (doc): gated parks are `stage-412-parked`, the doc said `upload-412-parked`. review doc:121 | 4fc8bee6 (doc) | v1.51.0 | none (doc) |
| inbox-9 | 2026-09-12 | review/audit | data loss | LOW: rescope-widen has no dirty check (only with the syncer stopped). Deferred: unreachable while the syncer holds the flock. review doc:122 | OPEN (deferred) | — | none |
| inbox-10 | 2026-09-12 | review/audit | contract | LOW: the 65th scope entry was dropped silently. Folded into `refused-scope`. review doc:123 | 4fc8bee6 | v1.51.0 | covered by the ack-1 tests (unverified) |
| lease-4 / arbitration-5 | 2026-09-12 | review/audit | availability | LOW (plausible): a node clock behind the store's suppresses in-barrier renewal. Deferred; in-barrier renewal no longer exists since the per-barrier fence (b50c2faf) — whether moot is unverified. review doc:124 | OPEN (deferred; possibly moot, unverified) | — | none |
| lease-5 / audit #6 | 2026-09-12 | review/audit | availability | LOW: a legacy single-object manifest is poisoned after the pointer install. Deferred: mixed-version rollouts with pre-2026-09-04 binaries only (nothing is deployed). review doc:125 | OPEN (deferred) | — | none |
| L-38 | 2026-09-10 | review/audit | data loss | An UNREADABLE path was published as a deletion and the object deleted: `confirm_absences` asked `symlink_metadata(..).is_err()`, so `EACCES`, `EIO`, `EMFILE`, `ELOOP` read as "the agent deleted it". Found by an adversarial investigation into splitting `scan`. CHANGELOG.md:1328 | e4c5a2e9 | v1.50.0 | none named in CHANGELOG (unverified) |
| L-39 | 2026-09-10 | review/audit | convergence | A full disk lost a foreign write: a write failure while consuming an inbox entry was recorded as a containment refusal and the entry dropped; the foreign bytes stayed in the bucket and the workspace never took them. Scoped-read audit. CHANGELOG.md:1338 | 3148992d | v1.50.0 | none named (unverified) |
| L-40 | 2026-09-10 | review/audit | security | A typo'd scope became the whole tree: `Scope::new` silently dropped malformed/too-long/over-cap entries, so an all-bad scope normalised to EMPTY, read everywhere as "no restriction". Scoped-read audit. CHANGELOG.md:1338 | 3148992d | v1.50.0 | none named (unverified) |
| L-41 | 2026-09-11 | host leg | performance | Every fetched byte was copied an extra time (`AggregatedBytes::into_bytes` on multi-frame bodies) and the ranged arm's inline blocking `pwrite` froze the fan-out; whole-object arm too. Found by measuring against an in-memory S3. 477-500 to 654-676 MiB/s on real S3. CHANGELOG.md:1349 | 2b1c801d | v1.50.0 | measurement (no test named) |
| L-42 | 2026-09-11 | host leg | performance | The publish read every file TWICE. CHANGELOG.md:1363 | 5cfb6fe7 | v1.50.0 | publish drill 1.55-1.63x (runcs) |
| L-43 | 2026-09-10 | host leg | performance | A large object was ONE stream in both directions. CHANGELOG.md:1363 | c9a94dab | v1.50.0 | publish drill (runcs) |
| L-44 | 2026-09-11 | code reading (unverified) | performance | The conflict log was an unbounded append that `load_conflicts` parsed WHOLE twice per sync honor, per status read and per scrape; cost grew forever on the read path. CHANGELOG.md:1366 | 9fa44535 | v1.50.0 | none named (unverified) |
| L-45 | 2026-09-11 | code reading (unverified) | contract | Lean operator: a parse failure of the holder's echo (`from_str(e).ok()`) reported as "an older syncer"; three env vars left from the v1.45.0 webhook removal read as operative and configured nothing. CHANGELOG.md:1374 | 81aa76b7 | v1.50.0 | none named (unverified) |
| L-46 | 2026-09-10 | code reading (unverified) | contract | Lean PodMonitor selected nothing since v1.45.0 (a tenant label that left with the webhook); indistinguishable from no workspaces. Unit-tested only, not verified live (commit body). CHANGELOG.md:1388 | 3452a5e9 | v1.50.0 | unit test only |
| L-47 | 2026-09-11 | host leg | availability | `checkout` took the publish lease: a checkout meeting a live publisher waited for it (a live holder always renews), and on a quiet cell a READER DEPOSED A LIVE PUBLISHER sixty seconds in; four disjoint readers ran 0.41x of one. Measured i4i.2xlarge. CHANGELOG.md:1243 | 3b9201ea | v1.50.0 | `a_checkout_issues_no_write_to_the_bucket` |
| L-48 | 2026-09-10 | code reading (unverified) | data loss | Lean gateway: the HITL door took any `PUT`; two browsers that each read v1 both got 200 and the second silently won, the loser's bytes unrecoverable from any manifest. Now 428/412/400. CHANGELOG.md:1193 | 4f3f11ea | v1.50.0 | "a real store proved the fix" (commit subject; test unverified) |
| L-49 | 2026-09-11 | test | availability | The raised `FLINT_SYNC_FANOUT` default (128) reached UPLOADS, which have no byte gate: 4x bodies in memory, and the lease fence window widened from 512 to 2048 files. Caught by the suite before shipping ("a_deposed_writer_stops_at_the_next_..."). CHANGELOG.md:1268 | fcba3efd | v1.50.0 (never shipped broken) | `a_deposed_writer_stops_at_the_next_...` (name truncated in commit body) |
| L-50 | 2026-09-11 | host leg | performance | Small-file checkout was fanout-bound at 32 (1,277 files/s), and the whole-object arm still wrote inline. CHANGELOG.md:1268 | 6984fe00 | v1.50.0 | measurement (no test named) |
| L-51 | 2026-09-09 | review/audit | contract | Release: `flint-s3-worker-lean` was `FROM flint-sync:1.45.0`, shipping a sync binary four releases old while its own payload was current. Found by diffing each image's source tree against its newest tag. CHANGELOG.md:1479 | 91a5bfdc | v1.49.0 | none (`release.sh check` cannot see it; staleness gate proposed) |
| L-52 | 2026-09-09 | review/audit | contract | Release: the standalone `flint-sync:1.45.0` image predated four `lean/sidecar` commits (including `2a213b01`) that were already shipping inside `flint-forge-syncer:1.48.0`. CHANGELOG.md:1488 | 91a5bfdc | v1.49.0 | none |
| L-53 | 2026-09-05 | live drill | contract | A reader adopted a foreign write into a published mirror: checkout's etag-pinned refusal was guarded by `if pinned` (gated only), so under forge's export manifests the S3-wins adopt copied bytes no manifest cites into the tree and reported success. Composition drill C4. Manifest/pointer now carry `sole_writer`. CHANGELOG.md:3070 | 78c4445e | v1.46.0 | S3-wins arm guarded by its own test (names unverified) |
| L-54 | 2026-09-03 | review/audit | data loss | Integrity audit hole 1: the cadence barrier's between-chunk renewal never fenced a deposed writer (returned Ok on exactly the deposed condition), so a straggler taken over mid-barrier completed every remaining data PUT over the cited generation. `docs/plans/flint-lean-integrity-audit-2026-09-03.md`. CHANGELOG.md:3844 | 78ba901c | v1.45.0 | none named in CHANGELOG (unverified) |
| L-55 | 2026-09-03 | review/audit | availability | Integrity audit hole 2: a renew whose own previous response was lost read the resulting 412 as a deposal and self-fenced (exit 0; permanent under CSI). CHANGELOG.md:3850 | 78ba901c | v1.45.0 | none named (unverified) |
| L-56 | 2026-09-03 | review/audit | data loss | Integrity audit hole 3: the SIGTERM drain did not attest completion and released the lease as a clean handoff on failure. Now `.flint-sync/drained.json`, retries for `FLINT_SYNC_DRAIN_BUDGET_SECS`, lease left UNRELEASED on failure. CHANGELOG.md:3853 | 78ba901c | v1.45.0 | drill legs S20-S22 (s3-csi side) |
| L-57 | 2026-09-03 | review/audit | data loss | Integrity audit hole 4: state and control files were not fsynced and the tree not `syncfs`ed before a baseline vouching for materialised files, so a power loss could leave a baseline describing zero-length files that the next scan publishes. CHANGELOG.md:3857 | 78ba901c | v1.45.0 | none named (unverified) |
| L-58 | 2026-09-03 | review/audit | security | Integrity audit hole 5: refuse-foreign was enforced only by the operator's verdict; the syncer did not read the claim cell and refuse a prefix claimed by another project. The first S22 run also showed a refusal exiting 1 restarted forever as "checkout in progress"; now exit 78 (`EXIT_REFUSED`). CHANGELOG.md:3862 | 78ba901c | v1.45.0 | drill leg S22 |
| L-59 | 2026-09-04 | code reading (unverified) | contract | Lean operator: the `StagedWorkRecovered` message said to run `flint-sync recover-staged` "in a pod on this workspace"; under CSI that binary exists only in the worker pod a tenant cannot exec into. CHANGELOG.md:3766 | unverified (in v1.45.0) | v1.45.0 | a unit test that fails if it stops naming a reachable recipe |
| L-60 | 2026-09-03 | review/audit | data loss | Chunk reaper judged the grace from the LISTING's timestamp, not the store's state at DELETE time, so it could delete a chunk an in-flight publish (whose adoption refreshes the chunk's age) was about to name; the implementation did not match `LeanChunkGC.tla`. Found by the parallel integrity audit at 3e1560ab. Source: commit body only (not in CHANGELOG). | cdabc10c | v1.45.0 | none named (unverified) |
| L-61 | 2026-09-03 | review/audit | performance | `sweep_chunks` had NO production caller (only tests), so with chunking on by default every publish left superseded chunks in the bucket forever. Source: commit body only. | cdabc10c claimed it; 8fa07929 ("actually wire the reaper — cdabc10c claimed to and did not") | v1.45.0 | none named (unverified) |
| L-62 | 2026-09-03 | code reading (unverified) | contract | The fix for L-61 did not wire the reaper either ("cdabc10c claimed to and did not"). Source: commit subject only. | 8fa07929 | v1.45.0 (never shipped broken) | none named |
| L-63 | 2026-09-03 | code reading (unverified) | contract | Lean lease: a credential refusal was logged as an ordinary renew failure and left no trace. Source: commit subject only. | b75a67fc | v1.45.0 | unverified |
| L-64 | 2026-09-03 | code reading (unverified) | contract | Lean serve: the retry arms still printed the generic line for a credential refusal. Source: commit subject only. | dbcfa6c2 | v1.45.0 | unverified |
| L-65 | 2026-09-03 | code reading (unverified) | contract | Lean operator: the DR signature told operators the sidecar was gone, which it never observed. Source: commit subject only. | d3ef7868 | v1.45.0 | unverified |
| L-66 | 2026-08-27 | test | contract | `spec.fetchInflightMb` (the 1.41.0 headline byte bound) was unreachable: the webhook never stamped `FLINT_SYNC_FETCH_INFLIGHT_MB` and no CR field carried it. The guard test `every_knob_the_sidecar_reads_is_stamped_by_the_webhook` was red at v1.41.0 but not run (lean work ran only the lean battery). 1.41.0 was tagged, never published. CHANGELOG.md:4342 | 41a9fedb | v1.41.1 | `every_knob_the_sidecar_reads_is_stamped_by_the_webhook` |
| L-67 | 2026-08-27 | host leg | availability | Checkout window unbounded by bytes: `fanout` bounded count, not size, and each whole object sat in RAM, so peak RSS was `fanout x largest object` in a sidecar with no memory limit (916 MiB on 32 x 32 MiB). Local latency rig. CHANGELOG.md:4398 | 73e34dfc | v1.41.0 | measurement (no test named) |
| L-68 | 2026-08-27 | code reading (unverified) | availability | No read timeout on the S3 client: one pooled connection to a reclaimed peer hung a fan-out slot and the whole checkout waited; the agent-start marker never landed. CHANGELOG.md:4425 | 73e34dfc | v1.41.0 | none named |
| L-69 | 2026-08-27 | code reading (unverified) | performance | `GET /status` downloaded and parsed a manifest of tens of MB to return three scalars; `Bytes::from(body.clone())` memcpy'd every published body; the gated lane rewrote the whole O(files) baseline every tick. CHANGELOG.md:4416, :4431 | 73e34dfc | v1.41.0 | none named |
| L-70 | 2026-08-27 | code reading (unverified) | contract | A takeover rotation dropped the manifest's boundary-source OBJECT STAMP while the document kept it (the forbidden GET/HEAD divergence); invisible until `/status` switched to HEAD. CHANGELOG.md:4437 | 73e34dfc (unverified) | v1.41.0 | none named |
| L-71 | 2026-08-26 | review/audit | data loss | Gated: the reaper deleted the successor's CITED version (its `is_current` guard protects one version; a gated successor stages -> cites -> stages, so a thawing straggler deleted it). The plan asserted the opposite in four places. CHANGELOG.md:4611 | a79b1381 | v1.39.0 | unverified (TLC gate 65/65, bucket drill 28/28) |
| L-72 | 2026-08-26 | review/audit | data loss | Gated: the upload lane reclaimed superseded versions with no reference to the installed manifest; one transient store error after a successful CAS left a stage naming cited versions and the next lane pass reaped them (two sites, supersede and cancel). Coverage audit (llvm-cov over the battery; "four decision arms no fixture had ever run"). Shipped in `flint-sync:1.38.0`. CHANGELOG.md:4624 | f2080edd | v1.39.0 | `pending_reclaims` tests (names unverified) |
| L-73 | 2026-08-26 | code reading (unverified) | availability | A restarted claimant owing an ack it could never honor sat in `claim` without answering, blocking its agent on a sentinel forever. CHANGELOG.md:4642 | a79b1381 | v1.39.0 | unverified |
| L-74 | 2026-08-26 | code reading (unverified) | contract | A stale ack retired a fresh boundary: `ack_matches` compared too loosely. CHANGELOG.md:4646 | 66459812 | v1.39.0 | unverified |
| L-75 | 2026-08-26 | code reading (unverified) | availability | A FIFO at a sentinel path wedged the poll arm (now `O_NONBLOCK` + `O_NOFOLLOW`, regular file only). CHANGELOG.md:4649 | 66459812 | v1.39.0 | unverified |
| L-76 | 2026-08-26 | code reading (unverified) | performance | `recover-staged` ignored the durable orphan summary and listed every version prefix-wide each time; `/status` paid for a HEAD it already had. CHANGELOG.md:4652 | 1452c768 / 66459812 (unverified split) | v1.39.0 | unverified |
| L-77 | 2026-08-26 | code reading (unverified) | availability | `stagedBacklogCapObjects` counted paths only, so one hot file rewritten every tick never fired the cap, while `drain_need_secs` sized termination grace from that cap. CHANGELOG.md:4659 | unverified (in v1.39.0) | v1.39.0 | unverified |
| L-78 | 2026-08-26 | live drill | availability | The lean chart could not install: it exec'd `flint-lean-operator` from an image that did not contain it (CrashLoopBackOff), and the injected sidecar image had no production recipe; `release.sh` did not gate the lean chart. CHANGELOG.md:4743 | 489640ce | v1.38.0 | `release.sh` lean gate |
| L-79 | 2026-08-26 | live drill | availability | The sidecar could not reach a TLS S3 endpoint: the certless base image had no `ca-certificates` for `rustls-native-certs`; every drill missed it because MinIO is plain HTTP. Found installing the chart on a three-node AWS cluster (unverified attribution). CHANGELOG.md:4750 | 489640ce (unverified) | v1.38.0 | none |
| L-80 | 2026-08-26 | code reading (unverified) | availability | A long barrier starved the lease renewal (one `select!`), so a deposed straggler could not learn it was deposed and a HEALTHY sidecar could outrun the 60 s takeover window and lose the lease to a standby. CHANGELOG.md:4756 | ee66cfb9 | v1.38.0 | unverified |
| L-81 | 2026-08-26 | live drill | availability | Gateway ignored `pinned_reads`: 409 for every staged-but-uncited file for the whole withholding window (the human read path dark when gated did its job). B1-B25 bucket drill (kind + MinIO). CHANGELOG.md:4764; plan §10.1g:1210 | 0cb4ece3 | v1.38.0 | regression test observed red (battery 94 -> 101) |
| L-82 | 2026-08-26 | live drill | contract | No boundary recorded which clock installed it outside gated mode (`boundary_source` stamped only by the gated citation). B1-B25 drill. CHANGELOG.md:4768; plan:1226 | 0cb4ece3 | v1.38.0 | regression test; drill leg B11 |
| L-83 | 2026-08-26 | live drill | contract | Fixing L-82 produced a disagreement: the drain rewrote its ack to `drain` but left the manifest stamped `sentinel`/`sentinel-deferred`; leg B11a caught the gated half. CHANGELOG.md:4768; plan:1236 | 0cb4ece3 | v1.38.0 (never shipped broken) | B11a |
| L-84 | 2026-08-26 | live drill | contract | `flint-sync status` could never report a pending sentinel: it spelled the record `pending-publish.json` against the written `publish.pending.json`. B1-B25 drill. CHANGELOG.md:4771 | 0cb4ece3 | v1.38.0 | regression test |
| L-85 | 2026-08-26 | live drill | data loss | A resumed checkout adopted a stale generation: it skipped a present file and stamped the baseline with the cited etag over old content, a divergence nothing reconciles. B1-B25 drill. CHANGELOG.md:4773 | 0cb4ece3 | v1.38.0 | regression test |
| L-86 | 2026-08-26 | code reading (unverified) | contract | Webhook mount injection collided silently: a pod with its own mount at the workspace path failed admission with the API server's "must be unique", naming neither flint nor the knob. (Webhook removed in v1.45.0.) CHANGELOG.md:4777 | unverified (in v1.38.0) | v1.38.0 | unverified |
| L-87 | 2026-08-26 | live drill | contract | Lean operator: the reconcile's fast pass recomputed the spec verdict and wrote `BoundaryModeAccepted=True`, CLEARING a bucket-side refusal ~2 min after the posture pass raised it (a customer's destroyer lifecycle rule stayed armed under green). Kind drill B26-B38 on its first run. Source: commit body only. | 85096813 | v1.38.0 (never shipped broken) | drill legs B26-B38 |
| L-88 | 2026-08-26 | live drill | contract | Lean operator: printer columns `CITED-SEQ`/`LAG`/`STAGED` were up to 30 min stale (reconcile requeued at 1800 s); "the worst defect of the tranche". Kind drill. Plan §10.1f:1141 | unverified (pre-v1.38.0) | v1.38.0 (never shipped broken) | drill legs B26-B38 |
| L-89 | 2026-08-25 | model | data loss | Gated citation deleted an acked user write: a UI write landing on a path the lane had already staged was invisible to the citation's base re-validation; the citation cited the staged version and the version reaper deleted the user's (current) version; the inbox entry 412'd and was dropped. Formal tranche product 2. Plan §10.1c:766; lean/formal/README.md:245 ("a live defect on its first strict run, in shipped code": committed, before the first lean release) | unverified (pre-v1.38.0) | v1.38.0 (never shipped broken) | model rule `CiteDropsInflightHitl` (no positive reachability probe until §10.1e) |
| L-90 | 2026-08-25 | code reading (unverified) | contract | A sentinel ack claimed a boundary that withheld the agent's delete (absence must survive two scans; the ack landed first with `ok`, `report.deleted 0`). Plan §10.1d:848 | 8836f1ce | v1.38.0 (never shipped broken) | unverified |
| L-91 | 2026-08-25 | code reading | availability | The D12 heartbeat renewal arm exited on a fence without settling, stranding an agent behind a `live` capability marker (it is usually the arm that learns of deposal first). Plan §10.1d:870; "found by reading for the model, not by running it" (lean/formal/README.md:367) | 77333f5e | v1.38.0 (never shipped broken) | unverified |
| L-92 | 2026-08-25 | model | contract | A restart between the manifest CAS and step 7 ate the agent's delete: the merge base lagged the install, own entries read as foreign, delete/modify resolved conservatively, ack `ok` with `report.deleted 0`. Found by the model twice by two routes on a strict run. Plan §10.1d:882; lean/formal/README.md:351 (pinned in the model as `MineIsNotForeign`) | 7cb4e1bc | v1.38.0 (never shipped broken) | unverified |
| C1 | 2026-08-25 | review/audit | data loss | preStop drain asked "did any settled ack carry a seq?": a SYNC ack always does, so a pending `.flint/sync` at SIGTERM cancelled the drain's own boundary (routine on pure spot). Plan review (ultracode, 56 findings, 6 survived). Plan §10.1e:979 | 2331d554 | v1.38.0 (never shipped broken) | fixture/mutation observed red |
| C2 | 2026-08-25 | review/audit | data loss | HITL exemption / "within one floor" was prose only: gated never runs the repair pass, so a consumed HITL write went clean-vs-baseline and the manifest cited its predecessor forever, invisible to pinned readers and DR checkout. | 2331d554 | v1.38.0 (never shipped broken) | fixture/mutation observed red |
| C3 | 2026-08-25 | review/audit | data loss | A withheld tombstone outliving its file amputated a re-created path at the citation, and a sibling sync deleted its copy. | 2331d554 | v1.38.0 (never shipped broken) | fixture/mutation observed red |
| C3-inversion | 2026-08-25 | model | data loss | The first C3 fix made upserts win over a standing tombstone, citing a file the agent DELETED (create-then-delete). TLC found it two hours after it was written. Plan §10.1e:998 | 2331d554 | v1.38.0 (never shipped broken) | the sentinel x citation-lane TLC world |
| C4 | 2026-08-25 | review/audit | security | Containment was implemented on the TARGET and the write went through an unvalidated temp sibling; two new writers (`.flint/remote.seq` every tick) had no containment: an arbitrary-file-write primitive with the bucket credentials. | 2331d554 | v1.38.0 (never shipped broken) | fixture/mutation observed red |
| C5 | 2026-08-25 | review/audit | data loss | `(pinned_reads, version_id: None)` took checkout's S3-wins arm, so enabling gated on an existing workspace adopted uncited mid-change bytes and did not self-correct. | 2331d554 | v1.38.0 (never shipped broken) | fixture/mutation observed red |
| C6 | 2026-08-25 | review/audit | contract | A gated honor's citation could drop a declared path while the ack said `ok` with no field to express it; now `partial` + `report.dropped`. | 2331d554 | v1.38.0 (never shipped broken) | fixture/mutation observed red |
| L-93 | 2026-08-25 | test | availability | Phase-3 tranche: the versioning conformance probe could wedge gated mode permanently (a crash or a discarded failed cleanup DELETE left its `If-None-Match: *` object; every later probe 412'd). Plan §10.1b:710 | unverified (pre-v1.38.0) | v1.38.0 (never shipped broken) | unverified |
| L-94 | 2026-08-25 | test | performance | Phase-3 tranche: the preStop drain ran the fused barrier in gated mode, re-uploading staged bytes at SIGTERM and leaving the final boundary without the `drain` stamp or `pinned_reads`. Plan §10.1b:719 | unverified (pre-v1.38.0) | v1.38.0 (never shipped broken) | mutation-isolated test |
| L-95 | 2026-08-25 | test | contract | Phase-3 tranche: standalone `checkout` never wrote the capability marker D11 requires. Plan §10.1b:727 | unverified (pre-v1.38.0) | v1.38.0 (never shipped broken) | fixture corrected |
| L-96 | 2026-08-25 | code reading (unverified) | contract | Phase-3 tranche: `CitationSource::Recovered` was defined and never constructed (`recover-staged` did not exist). Plan §10.1b:735 | unverified (pre-v1.38.0) | v1.38.0 (never shipped broken) | none |
| L-97 | 2026-08-26 | review/audit | data loss | D9's `orphans.json` was never written in any mode, so a pure-spot replacement destroyed the only record naming uncited work. Plan §10.1f:1074 | unverified (baefbcaa?) | v1.38.0 (never shipped broken) | unverified |
| L-98 | 2026-08-26 | review/audit | contract | Two config fields the binary never read (`stagedBacklogCapObjects/Bytes`, `noncurrentRetentionDays` had no `env_u64`). Now a webhook<->binary contract test. Plan §10.1f:1085 | unverified (e985bb35?) | v1.38.0 (never shipped broken) | webhook<->binary contract test |
| L-99 | 2026-08-26 | review/audit | availability | Lean operator DR-signature condition read `released == true`, so it fired only after a CLEAN shutdown, never for a dead holder that actually strands work. Plan §10.1f:1094 | unverified (pre-v1.38.0) | v1.38.0 (never shipped broken) | unverified |

## Table 2 — defects in test rigs that would have faked results

| found | summary | fix |
|-------|---------|-----|
| 2026-09-15 | Release tooling: `release.sh`'s lean CRD comparison skipped SILENTLY when `crdgen` failed, so a stale lean CRD would have passed the release check. CHANGELOG.md:223 | one check over share, lean and forge CRDs refuses on a stale/missing file and on a `crdgen` failure (v1.54.0) |
| 2026-09-13 | `MemoryStore` (the `crates/flint-store` double) answered If-Match on a missing key with 412 (Ozone's answer) where S3 answers 404 NoSuchKey, so the vanished-base rule was green in tests and never ran on S3 (finding 11). CHANGELOG.md:563 | double answers 404 by default, 412 behind `with_if_match_missing_as_412`; the test runs both (ed7de78e, v1.52.0) |
| 2026-09-13 | Finding 1's regression test used DIFFERENT peer bytes; no test wrote identical bytes, so finding 13 passed the battery (the double's etag was already a content hash). assessment:651 | `a_peer_upload_of_identical_bytes_before_the_gc_delete_is_not_deleted` (79e7dac9, v1.52.0) |
| 2026-09-13 | `lean/e2e/run-writers.sh` (cluster form of L1-L8) written and not run; run-chaos.sh C2/C3 re-derived and not run at v1.52.0. CHANGELOG.md:485 (a coverage gap, not a false pass) | superseded by the writers-live drill (unverified whether run-writers.sh itself ran) |
| 2026-09-12 | Door drill: two arms' guards failed the run "on the rig itself" (CHANGELOG.md:826); the specific rig defects are not named there. | (unverified; see `lean/e2e/perf/results/door-drill-2026-09-12.md`) |
| 2026-09-12 | CI: lean and forge had 378 tests nothing ran, and the formal models had none; the hub formal gate could never run on Linux and the lean gate was dragging it along. Source: commit subjects only. | ece04610, 4d03b120 (v1.51.0) |
| 2026-09-12 | The gated lane's CRC citation repair had NO test: citing a wrong CRC there passed the whole battery. CHANGELOG.md:1098 | six mutation controls on an `AttestsNoChecksum` double (328bbf5d, v1.51.0) |
| 2026-09-11 | runcs publish drill: TWO of the author's controls were vacuous. Source: commit subject only. | 5b6df8e7 (v1.50.0) |
| 2026-09-10 | Perf drill whose own control failed it, "and what that was hiding"; the seed wrote to a FIXED key space so a reused bucket parked it; a comment instead of a guard on the seed. Source: commit subjects only. | d4396baa, 661a123e, 883beccd (v1.50.0) |
| 2026-09-04 | Two lean protocol suites (`lean/e2e/run-verbs.sh` B1-B25, `run-chaos.sh` C1-C12) were wrongly bannered retired: a suite nobody runs because it says not to. CHANGELOG.md:3772 | banner corrected in place (v1.45.0) |
| 2026-09-03 | Every lean harness still read the manifest key the pointer layout retired. Source: commit subject only. | c137f3af (v1.45.0) |
| 2026-08-27 | The `flint-store` lib-test target had not compiled since f2080edd (three `self.bump(...)` in free functions): every test in that crate silently unrunnable, because lean work never built it. CHANGELOG.md:4446 | e0b6dcc7 (v1.41.0) |
| 2026-08-27 | The guard test `every_knob_the_sidecar_reads_is_stamped_by_the_webhook` was red at v1.41.0 and never run (lean work ran only the lean battery), so L-66 shipped in a tag. CHANGELOG.md:4363 | run with the hub suite (41a9fedb, v1.41.1) |
| 2026-08-26 | Coverage audit: four decision arms no fixture had ever run (llvm-cov over the battery), where L-72 lived. Source: f2080edd subject/body. | f2080edd (v1.39.0) |
| 2026-08-26 | Drill rig: `mc ls --versions --json` emits no `isLatest` field (the rig derived it); `jq -e` over a filtering stream exits 4 when the last row does not match; the drill did not assert what it claimed or count its own legs and depended on a best-effort signal. Source: commit bodies of 75845740, bdedf7c4 (details unverified). | 75845740, bdedf7c4 (v1.39.0) |
| 2026-08-26 | Phase 4/5/6: three tests that could not fail, caught by the mutation pass: an orphan-summary oracle compared second-granular ETags; a condition-timestamp test compared two `now_rfc3339()` from one second; the door-coalescing test ran the order that a wrong implementation also passes. boundary-verbs plan:1126 | counts VERSIONS; explicit timestamp; runs both orders (pre-v1.38.0) |
| 2026-08-26 | B1-B25 drill harness: a failing leg skipped its own cleanup and its `run` loop deposed the NEXT leg's syncer (a product-shaped symptom); B8's in-pod request counter was silently empty (`minio/mc` has no grep/awk/sed), so every budget assertion would have passed on nothing; fixtures rewriting to the same length in the same second tested the scan residual, not the leg; a one-shot verb blocked in `claim` forever and hung the drill. boundary-verbs plan:1275-1296 | leg resets all pods; counting on the host; fixtures vary length; one-shot timeout, exit 124 = blocked (pre-v1.38.0) |
| 2026-08-26 | B12's first run failed its own anti-vacuity guard: the straggler froze outside the upload loop, fenced cooperatively and wrote nothing, so containment was untested. boundary-verbs plan:1259 | freeze lands inside the upload loop (pre-v1.38.0) |
| 2026-08-25 | The first hand-written Rust test for D4 (scoped sync) was vacuous: it used a HITL inbox entry as the out-of-scope change and passed with the hazard reintroduced. lean/formal/README.md:211 | (fix commit unverified) |
| 2026-09-13 | TLC gate printed only the last 40 lines of a failing run, so the violated invariant's name was lost with the spot instance. lean/formal/README.md:747 | the 2026-09-14/15 box runs kept the full log |
| open | `flint-s3-worker` tees the syncer's stderr in raw 4096-byte reads while its own `eprintln!` lines come from another thread, so a trace line can be spliced (most likely during pod deletion, leg A4); `extract_traces.py` counts it `malformed`. lean/e2e/writers-live/EVIDENCE.md:93 | OPEN (fix belongs in the worker: tee whole lines) |

## Table 3 — model-only bugs (the model was wrong, the code was right)

| found | summary | fix |
|-------|---------|-----|
| 2026-09-15 | Trace validation, first run: three of five syncer traces rejected (at the commit's CAS, an upload superseding a foreign version, a declared barrier's scan). Each was the model lagging the code: 3 bugs. lean/formal/README.md:964 | constants `CommitLoadsCurrent`, `Upload412Preserves`, `DeclaredConfirmsAbsence` |
| 2026-09-15 | The first box run of the one-path sentinel world on the IMPL shape omitted `VerifyUploadedCitations` and stopped on `Inv_NoStaleOverride` in 16 steps: a cfg error. README:911 | cfg corrected |
| 2026-09-13 | `Inv_AckBoundaryCoherent` third refinement: VIOLATED at depth 19 because the stamp fires when an ok ack's document is ahead of the tree by a queued peer change (the ack is honest). It did expose a real contract bug (L-22). README:739 | refinement landed in tranche 7 (2026-09-15, README:871); known-bad `LeanBarrierLeaseQueueDropped` |
| 2026-09-13 | `Inv_AckBoundaryCoherent` second refinement: a citation repair rightly declined because the key moved. README:716 | `repairMoved` exemption |
| 2026-09-13 | `Inv_AckBoundaryCoherent` first refinement under two writers: compared the baseline with the LIVE manifest, which a peer may move between honor and ack. README:702 | `AckedDoc(s)` / `instSnap` |
| 2026-09-14 | `Inv_HITLTracked` false positive under same-bytes aliasing: a UI write's generation re-created by the agent after its published delete. README:801 | — (first repair tried) |
| 2026-09-14 | `Inv_HITLTracked` first repair (accept a live tree holding those bytes) failed again with the pod replaced after upload (that trace is finding 10, not a UI-write loss). README:805 | `gh.hitlRetired`; relaxation re-run on a known-bad world (`EarlyInboxDrop = TRUE`) still violates |
| 2026-09-13 | Under the barrier lease the model's ungated adopt arm adopted ANY recognised generation and cited the walk's bytes, which is how the first four strict runs dangled before the real race was reached. README:557 | arm made to follow `upload_one` (adopt only on CRC match) |
| 2026-08-25 | Gated-citation tranche: three of the four defects it reported first were in the model: `dels` guarded the UNCITE with the GC's guard; `Consume` could interleave between the citation and the ack (single-threaded in code); the adopt-own arm staged any recognised generation. 3 bugs. README:435 | model corrected |
| 2026-08-25 | `BoundaryBroken`'s conflict exemption written as a plain conjunct excused exactly the case it exists to catch (vacuous direction). README:443 | narrowed to a disjunct |
| 2026-08-25 | The ack promise took three tries: snapshot equality (at-least semantics), missing `pendDirty`, missing the tree-comparison clause; each wrong try was a real, correct behaviour. 3 bugs. README:326; boundary-verbs plan §10.1d ("the invariant was wrong three times before the code was wrong once") | `pendMint`, `pendDirty`, tree clause in `BoundaryBroken` |
| 2026-08-25 | Scoped-sync (D4) cfg: `MaxBarriers=1` with no stall arm made the hazard UNREACHABLE, so the mutation ran green (vacuous). README:203 | `AllowStall` + second barrier |
| 2026-08-25 | `CiteDropsInflightHitl` (product 2's rule, L-89) never had a positive reachability probe: no gated world had budget for the four mints (vacuous). README:450 | `ProbeDeclaredDrop` |
| 2026-09-03 | Chunk-GC module first reported HOLDING because it had no CRASH action and so could not produce an orphan (vacuous; written before the reaper existed). README:58 | crash action added |
| 2026-08-26 | "Three claims the model was making without evidence". Source: commit subject 9877a7c1 only; the claims are not itemised here (unverified). | 9877a7c1 |

### Table 3b — model abstractions that hid real code defects (model wrong AND code wrong)

| hid | abstraction | source |
|-----|-------------|--------|
| F1 | `GCDelete` was one atomic step; the shipped GC is HEAD then unconditional DELETE | README:541 |
| finding 13 | every write minted a unique generation; real etags are content hashes ("the abstraction was the bug, again") | README:646 |
| finding 12 | `HitlWrite` (gateway PUT + inbox append) was one atomic step | CHANGELOG.md:588; assessment:628 |
| L-24 (412 supersede) | the model's 412 arm parks, and `Inv_NoDangling` checks existence, not generation | assessment:560 |
| L-1, F5, F6 | `foreignQ` joined the shared inbox; the writer-local queue was not modelled until tranche 7 | README:595, :859 |
| F7 | no empty-install rule until tranche 7 | CHANGELOG.md:309 |

## Rate

Computed from Table 1 by script (141 rows; each id unique). Normalisation: "shipped in" is the release whose tag first contains the fix (`git tag --contains <hash> | sort -V | head -1`, cross-checked against the CHANGELOG section; for 1.40.0/1.41.0, which share a date, the CHANGELOG's lean-scoped 1.41.0 is used); a row whose fix is OPEN counts under OPEN. "found by" strips "(unverified)".

### Per shipped-in release

"pre" counts rows marked "never shipped broken": the defect was found and fixed during that release's development and no published build carried it. OPEN rows have no release.

| release | rows | of which pre | ids |
|---|---|---|---|
| OPEN | 9 | 0 | L-24, finding 10 (L-25), L-26, L-27, ack-7, ack-10 / U37, inbox-9, lease-4 / arbitration-5, lease-5 / audit #6 |
| unreleased | 1 | 0 | L-1 |
| v1.54.0 | 3 | 0 | L-2, L-3, L-4 |
| v1.53.0 | 5 | 0 | L-5, L-6, L-7, L-8, L-9 |
| v1.52.0 | 14 | 10 | F1 / finding 1 (L-10), F2 (L-11), F3 (L-12), F5 (L-13), F6 (L-14), F7 (L-15), F8 (L-16), finding 11 (L-17), finding 12 (L-18), finding 13 (L-19), finding 13, second route (L-20), atomicity-5 / inbox-4 / barrier-7 (L-21), L-22, L-23 |
| v1.51.0 | 40 | 1 | L-28, L-29, L-30, L-31, L-32, L-33, L-34, L-35, L-36, L-37, atomicity-1, ack-1 / inbox-3, atomicity-2, atomicity-3 / inbox-2, atomicity-4, inbox-1, atomicity-6, gated-1, gated-2, ack-2 / gated-4, inbox-5, inbox-7, inbox-8, gated-3 / lease-6, lease-1, lease-2, lease-3 / audit #7, ack-3 / atomicity-8 / inbox-6, ack-4, ack-5 / gated-5, ack-6, ack-8, ack-9, ack-11, atomicity-7, atomicity-9 / csi-6, gated-6, gated-7, gated-8, inbox-10 |
| v1.50.0 | 13 | 1 | L-38, L-39, L-40, L-41, L-42, L-43, L-44, L-45, L-46, L-47, L-48, L-49, L-50 |
| v1.49.0 | 2 | 0 | L-51, L-52 |
| v1.46.0 | 1 | 0 | L-53 |
| v1.45.0 | 12 | 1 | L-54, L-55, L-56, L-57, L-58, L-59, L-60, L-61, L-62, L-63, L-64, L-65 |
| v1.41.1 | 1 | 0 | L-66 |
| v1.41.0 | 4 | 0 | L-67, L-68, L-69, L-70 |
| v1.39.0 | 7 | 0 | L-71, L-72, L-73, L-74, L-75, L-76, L-77 |
| v1.38.0 | 29 | 21 | L-78, L-79, L-80, L-81, L-82, L-83, L-84, L-85, L-86, L-87, L-88, L-89, L-90, L-91, L-92, C1, C2, C3, C3-inversion, C4, C5, C6, L-93, L-94, L-95, L-96, L-97, L-98, L-99 |
| **total** | **141** | **34** | |

### Per class

| class | rows | of which OPEN | ids |
|---|---|---|---|
| contract | 51 | 2 | L-2, L-3, L-9, L-22, L-28, L-32, L-37, ack-2 / gated-4, inbox-7, gated-3 / lease-6, lease-1, lease-3 / audit #7, ack-3 / atomicity-8 / inbox-6, ack-4, ack-5 / gated-5, ack-6, ack-7, ack-8, ack-9, ack-10 / U37, ack-11, atomicity-7, gated-6, gated-7, gated-8, inbox-10, L-45, L-46, L-51, L-52, L-53, L-59, L-62, L-63, L-64, L-65, L-66, L-70, L-74, L-82, L-83, L-84, L-86, L-87, L-88, L-90, L-92, C6, L-95, L-96, L-98 |
| data loss | 35 | 2 | L-1, L-4, F1 / finding 1 (L-10), F2 (L-11), F8 (L-16), finding 12 (L-18), finding 13 (L-19), finding 13, second route (L-20), L-27, L-29, L-33, L-34, atomicity-2, atomicity-3 / inbox-2, atomicity-4, inbox-1, gated-1, gated-2, inbox-9, L-38, L-48, L-54, L-56, L-57, L-60, L-71, L-72, L-85, L-89, C1, C2, C3, C3-inversion, C5, L-97 |
| availability | 27 | 3 | L-5, finding 11 (L-17), L-23, L-26, L-36, atomicity-1, ack-1 / inbox-3, inbox-5, lease-2, atomicity-9 / csi-6, lease-4 / arbitration-5, lease-5 / audit #6, L-47, L-49, L-55, L-67, L-68, L-73, L-75, L-77, L-78, L-79, L-80, L-81, L-91, L-93, L-99 |
| performance | 16 | 0 | L-6, L-7, L-8, F7 (L-15), L-30, L-31, L-35, L-41, L-42, L-43, L-44, L-50, L-61, L-69, L-76, L-94 |
| convergence | 7 | 2 | F3 (L-12), F5 (L-13), F6 (L-14), atomicity-5 / inbox-4 / barrier-7 (L-21), L-24, finding 10 (L-25), L-39 |
| security | 5 | 0 | atomicity-6, inbox-8, L-40, L-58, C4 |

### Per found by

| found by | rows | of which "(unverified)" | ids |
|---|---|---|---|
| review/audit | 59 | 0 | atomicity-5 / inbox-4 / barrier-7 (L-21), atomicity-1, ack-1 / inbox-3, atomicity-2, atomicity-3 / inbox-2, atomicity-4, inbox-1, atomicity-6, gated-1, gated-2, ack-2 / gated-4, inbox-5, inbox-7, inbox-8, gated-3 / lease-6, lease-1, lease-2, lease-3 / audit #7, ack-3 / atomicity-8 / inbox-6, ack-4, ack-5 / gated-5, ack-6, ack-7, ack-8, ack-9, ack-10 / U37, ack-11, atomicity-7, atomicity-9 / csi-6, gated-6, gated-7, gated-8, inbox-9, inbox-10, lease-4 / arbitration-5, lease-5 / audit #6, L-38, L-39, L-40, L-51, L-52, L-54, L-55, L-56, L-57, L-58, L-60, L-61, L-71, L-72, C1, C2, C3, C4, C5, C6, L-97, L-98, L-99 |
| code reading | 29 | 28 | L-3, L-4, L-9, L-23, L-33, L-34, L-37, L-44, L-45, L-46, L-48, L-59, L-62, L-63, L-64, L-65, L-68, L-69, L-70, L-73, L-74, L-75, L-76, L-77, L-80, L-86, L-90, L-91, L-96 |
| live drill | 19 | 0 | L-5, L-6, L-7, L-8, finding 12 (L-18), finding 13 (L-19), L-26, L-35, L-36, L-53, L-78, L-79, L-81, L-82, L-83, L-84, L-85, L-87, L-88 |
| model | 16 | 1 | L-1, L-2, F1 / finding 1 (L-10), F2 (L-11), F3 (L-12), F5 (L-13), F6 (L-14), F7 (L-15), F8 (L-16), finding 13, second route (L-20), L-22, finding 10 (L-25), L-29, L-89, L-92, C3-inversion |
| host leg | 11 | 0 | finding 11 (L-17), L-27, L-30, L-31, L-32, L-41, L-42, L-43, L-47, L-50, L-67 |
| test | 7 | 1 | L-24, L-28, L-49, L-66, L-93, L-94, L-95 |

### Found by, per release (row counts)

| release | model | live drill | host leg | test | code reading | review/audit | total |
|---|---|---|---|---|---|---|---|
| OPEN | 1 | 1 | 1 | 1 | 0 | 5 | 9 |
| unreleased | 1 | 0 | 0 | 0 | 0 | 0 | 1 |
| v1.54.0 | 1 | 0 | 0 | 0 | 2 | 0 | 3 |
| v1.53.0 | 0 | 4 | 0 | 0 | 1 | 0 | 5 |
| v1.52.0 | 9 | 2 | 1 | 0 | 1 | 1 | 14 |
| v1.51.0 | 1 | 2 | 3 | 1 | 3 | 30 | 40 |
| v1.50.0 | 0 | 0 | 5 | 1 | 4 | 3 | 13 |
| v1.49.0 | 0 | 0 | 0 | 0 | 0 | 2 | 2 |
| v1.46.0 | 0 | 1 | 0 | 0 | 0 | 0 | 1 |
| v1.45.0 | 0 | 0 | 0 | 0 | 5 | 7 | 12 |
| v1.41.1 | 0 | 0 | 0 | 1 | 0 | 0 | 1 |
| v1.41.0 | 0 | 0 | 1 | 0 | 3 | 0 | 4 |
| v1.39.0 | 0 | 0 | 0 | 0 | 5 | 2 | 7 |
| v1.38.0 | 3 | 9 | 0 | 3 | 5 | 9 | 29 |

### Class, per release (row counts)

| release | data loss | contract | convergence | availability | performance | security | total |
|---|---|---|---|---|---|---|---|
| OPEN | 2 | 2 | 2 | 3 | 0 | 0 | 9 |
| unreleased | 1 | 0 | 0 | 0 | 0 | 0 | 1 |
| v1.54.0 | 1 | 2 | 0 | 0 | 0 | 0 | 3 |
| v1.53.0 | 0 | 1 | 0 | 1 | 3 | 0 | 5 |
| v1.52.0 | 6 | 1 | 4 | 2 | 1 | 0 | 14 |
| v1.51.0 | 9 | 20 | 0 | 6 | 3 | 2 | 40 |
| v1.50.0 | 2 | 2 | 1 | 2 | 5 | 1 | 13 |
| v1.49.0 | 0 | 2 | 0 | 0 | 0 | 0 | 2 |
| v1.46.0 | 0 | 1 | 0 | 0 | 0 | 0 | 1 |
| v1.45.0 | 4 | 5 | 0 | 1 | 1 | 1 | 12 |
| v1.41.1 | 0 | 1 | 0 | 0 | 0 | 0 | 1 |
| v1.41.0 | 0 | 1 | 0 | 2 | 1 | 0 | 4 |
| v1.39.0 | 2 | 1 | 0 | 3 | 1 | 0 | 7 |
| v1.38.0 | 8 | 12 | 0 | 7 | 1 | 1 | 29 |

### Per campaign

Campaign is not a column of Table 1; this mapping assigns each row to the campaign that found it. Rows with no named campaign are listed last.

| started | campaign | rows | ids |
|---|---|---|---|
| 2026-08-25 | boundary-verbs plan review (ultracode: 56 findings, 6 survived adversarial verification) | 6 | C1, C2, C3, C4, C5, C6 |
| 2026-08-25 | formal tranches 2-3 (products 1 and 2) and plan §10.1c-d | 5 | L-89, L-90, L-91, L-92, C3-inversion |
| 2026-08-25 | Phase-3 completion tranche (plan §10.1b) | 4 | L-93, L-94, L-95, L-96 |
| 2026-08-26 | Phase 4/5/6 tranche (plan §10.1f) | 3 | L-97, L-98, L-99 |
| 2026-08-26 | B1-B25 bucket drill, kind + MinIO (plan §10.1g) | 5 | L-81, L-82, L-83, L-84, L-85 |
| 2026-08-26 | kind boundary drill B26-B38 | 2 | L-87, L-88 |
| 2026-08-26 | v1.38.0 packaging and the AWS install | 2 | L-78, L-79 |
| 2026-08-26 | coverage audit, llvm-cov over the battery (L-71 attribution unverified) | 2 | L-71, L-72 |
| 2026-08-27 | read-path release on the local latency rig | 5 | L-66, L-67, L-68, L-69, L-70 |
| 2026-09-03 | integrity audit (docs/plans/flint-lean-integrity-audit-2026-09-03.md) | 7 | L-54, L-55, L-56, L-57, L-58, L-60, L-61 |
| 2026-09-05 | composition drill (C4) | 1 | L-53 |
| 2026-09-09 | published-images audit (source tree vs newest tag) | 2 | L-51, L-52 |
| 2026-09-10 | scoped-read audit and the adversarial scan investigation | 3 | L-38, L-39, L-40 |
| 2026-09-10 | read and publish path measurement rigs (in-memory S3, i4i, runcs) | 6 | L-41, L-42, L-43, L-47, L-49, L-50 |
| 2026-09-12 | fanout-ceiling rig, i4i.xlarge vs real S3 | 3 | L-30, L-31, L-32 |
| 2026-09-12 | raw read path on a cluster | 1 | L-35 |
| 2026-09-12 | deployed door drill (door-drill-2026-09-12; the cluster name is unverified) | 1 | L-36 |
| 2026-09-12 | rename modelling, phase E | 1 | L-29 |
| 2026-09-12 | protocol review (docs/plans/flint-lean-protocol-review-2026-09-12.md) | 36 | atomicity-1, ack-1 / inbox-3, atomicity-2, atomicity-3 / inbox-2, atomicity-4, inbox-1, atomicity-6, gated-1, gated-2, ack-2 / gated-4, inbox-5, inbox-7, inbox-8, gated-3 / lease-6, lease-1, lease-2, lease-3 / audit #7, atomicity-5 / inbox-4 / barrier-7 (L-21), ack-3 / atomicity-8 / inbox-6, ack-4, ack-5 / gated-5, ack-6, ack-7, ack-8, ack-9, ack-10 / U37, ack-11, atomicity-7, atomicity-9 / csi-6, gated-6, gated-7, gated-8, inbox-9, inbox-10, lease-4 / arbitration-5, lease-5 / audit #6 |
| 2026-09-13 | two-writer model tranche plus the tests that checked it (assessment §10.1) | 8 | F1 / finding 1 (L-10), F2 (L-11), F3 (L-12), F5 (L-13), F6 (L-14), F7 (L-15), F8 (L-16), L-24 |
| 2026-09-13 | writers live drill on real S3 (runcv): deployed legs A2/A3, host legs H2/H5, the Ozone probe | 6 | finding 10 (L-25), finding 11 (L-17), finding 12 (L-18), finding 13 (L-19), L-26, L-27 |
| 2026-09-13 | TLC runs, the i4i box and the laptop (sentinel world, same-bytes world) | 5 | finding 13, second route (L-20), L-2, L-3, L-4, L-22 |
| 2026-09-14 | contention drill, 19 runs on real S3 | 4 | L-5, L-6, L-7, L-8 |
| 2026-09-15 | tranche 7: the writer-local queue modelled | 1 | L-1 |
| — | no named campaign (mostly "code reading (unverified)") | 22 | L-9, L-23, L-28, L-33, L-34, L-37, L-44, L-45, L-46, L-48, L-59, L-62, L-63, L-64, L-65, L-73, L-74, L-75, L-76, L-77, L-80, L-86 |
| | **total** | **141** | |

## How this was compiled

Sources, in order: `CHANGELOG.md` (every release section from [Unreleased] to 1.38.0, the first lean release); `docs/plans/flint-lean-writer-lease-and-gated-assessment.md` §10.1; `lean/formal/README.md`; `lean/e2e/writers-live/EVIDENCE.md`; `git log` over `lean/syncer lean/gateway crates/flint-store` (73 commits) and, because lean lived under `lean/sidecar` before 2026-09-12 (5da7e9ad), a date-bounded `git log` over the whole tree for lean subjects. Two documents outside the list were read because the sources cite them for ids: `docs/plans/flint-lean-protocol-review-2026-09-12.md` (the review's id table, lines 90-125) and `docs/plans/flint-lean-boundary-verbs-plan.md` §10.1b-g (the pre-1.38.0 findings), plus `docs/plans/flint-lean-writers-live-drill-plan.md` §0 for the F-number to defect mapping. No cargo, TLC or build was run; `git tag --contains` was the only tag query.

### Census control: CHANGELOG lean "Fixed" bullets, counted independently

A script walked `CHANGELOG.md` and counted every top-level bullet under a `### Fixed*` or `### Known*` heading whose first 200 characters name `lean`, `flint-lean`, `flint-sync` or `sidecar` as a word, plus every such bullet in the lean-scoped releases (1.38.0, 1.39.0, 1.41.0, 1.41.1). It found **49 Fixed bullets** and **18 Known-limitation bullets**. Reconciled by hand:

| release | census Fixed | became Table 1 rows | elsewhere |
|---|---|---|---|
| Unreleased | 3 | 1 bullet, 1 row (L-1) | 2 adjacent (chart notes, docs guide) |
| 1.54.0 | 2 | 1 bullet, 3 rows (L-2, L-3, L-4) | 1 adjacent (s3-csi ro bind) |
| 1.53.0 | 0 | — | the release's 5 lean defects are under Changed (L-5..L-9) |
| 1.52.0 | 0 | — | 14 rows from Changed (F1..F8, findings 11-13, L-21, L-22, L-23) |
| 1.51.0 | 7 | 6 bullets, 6 rows (L-28, L-29, L-32, L-33, L-34, L-35) | 1 bullet is a feature filed under Fixed (`probe-conditional`/`probe-versions`, no defect); Added/Changed give the 36 review ids, L-30, L-31, L-36, L-37 |
| 1.50.0 | 7 | 7 bullets, 9 rows (L-38..L-46; two bullets hold two defects each) | Added/Changed give L-47..L-50 |
| 1.49.0 | 2 | 2 bullets, 2 rows (L-51, L-52) | |
| 1.46.0 | 5 | 0 | all 5 are false positives (forge and s3-csi bullets that mention a lean workspace); the heuristic MISSED the one lean bullet (:3072, under "Fixed — flint lean"), placed as L-53 |
| 1.45.0 | 5 | 1 bullet, 5 rows (L-54..L-58) | 4 adjacent (s3-csi); L-59 is from Changed; L-60..L-65 from commits only |
| 1.41.1 | 1 | 1 row (L-66) | |
| 1.41.0 | 2 | 1 row (L-70) | 1 to Table 2 (flint-store test target); L-67..L-69 from Changed |
| 1.39.0 | 7 | 7 bullets, 6 rows (L-71..L-76; L-76 holds the `recover-staged` and `/status` bullets) | L-77 from Changed |
| 1.38.0 | 8 | 8 bullets, 9 rows (L-78..L-86; L-82/L-83 split one bullet) | L-87..L-99, C1-C6, C3-inversion from commits and the plan |
| **total** | **49** | **35 bullets, 43 rows**; plus L-53 from the missed 1.46.0 bullet = **44 rows** | **14 bullets not rows**: 7 adjacent, 1 feature, 5 false positives, 1 Table 2 |

Arithmetic check, by bullet (placed + not placed = census): Unreleased 1+2=3; 1.54.0 1+1=2; 1.51.0 6+1=7; 1.50.0 7+0=7; 1.49.0 2+0=2; 1.46.0 0+5=5; 1.45.0 1+4=5; 1.41.1 1+0=1; 1.41.0 1+1=2; 1.39.0 7+0=7; 1.38.0 8+0=8. Sum 35+14 = 49. Rows from the placed bullets: 1+3+6+9+2+0+5+1+1+6+9 = 43.

Known limitations: 18 bullets are 4 distinct OPEN rows (L-24, L-25, L-26, L-27) repeated across 1.52.0-1.54.0, 1 Table 3 row (the sentinel invariant, repeated 3 times), and 5 not counted (the gateway binary's read-only door, the 1.46.0 cross-product prefix, the three 1.38.0 scope limits). The other 5 OPEN rows (ack-7, ack-10, inbox-9, lease-4, lease-5) are the protocol review's deferrals.

Row total: 44 (Fixed bullets) + 4 (Known limitations) + 67 (other CHANGELOG sections: 36 review ids, 1.52.0's 14 less L-21 already a review id, 1.53.0's 5, 1.51.0's 4, 1.50.0's 4, 1.45.0's 1, 1.41.0's 3, 1.39.0's 1) + 26 (commits and plans only: L-60..L-65, L-87..L-99, C1-C6, C3-inversion) = **141**.

### Mentioned but not placed

- **Protocol review severity count.** The review's table has 8 HIGH rows (ack-1, atomicity-2, -3, -4, -6, inbox-1, gated-1, gated-2) and 9 MEDIUM rows; the CHANGELOG and 4fc8bee6 say "seven HIGH and eight MEDIUM". Not reconciled (unverified whether atomicity-6, "HIGH (security)", is counted apart, and whether the deferred atomicity-5 and doc-only ack-2 are the missing MEDIUMs).
- **F4, and findings 4-9 of the syncer numbering.** The drill plan maps F1-F3 and F5-F8; no source read names F4 or findings 4-9. `lean/formal/README.md` uses a second, tranche-local numbering (its "finding 4" is F8, its "finding 5" is finding 13).
- **The deployed door drill "found five things deploying found"** (2bba72aa). Only the OOM (L-36) is a lean syncer defect in the CHANGELOG; the other four are not placed (they may be passthrough or rig defects; unverified).
- **B1-B25 "five defects, and one the drill's own guard caught"** (plan §10.1g title). Five are L-81..L-85; the sixth may be the B8 empty counter (Table 2) or L-83 (unverified).
- **1452c768**: "the citation's crash matrix, the recovery it never used, and eleven doc claims that stopped being true". The eleven doc claims are not itemised; "the recovery it never used" may be part of L-76 (unverified).
- **75845740**: "the recovery narrowing that told a workspace its data was gone" reads as a product defect (a recovery message) and is not placed (unverified).
- **9877a7c1**: "three claims the model was making without evidence" is one Table 3 row, not three.
- **Plan review, 56 findings**: 50 did not survive adversarial verification or were not routed; not defects.
- **`run-writers.sh` never run** at v1.52.0 is a coverage gap, recorded in Table 2 but not a faked result.

### Known weaknesses of this ledger

- 28 of the 29 "code reading" rows are "(unverified)": the CHANGELOG states the defect and the fix, not how it was found. The found-by rates are therefore firm for model, live drill, host leg and review/audit, and a floor for everything else.
- 34 rows are marked "never shipped broken" (21 under v1.38.0, 10 under v1.52.0, 1 each under v1.51.0, v1.50.0 and v1.45.0): found and fixed during that release's development. A rate of defects that reached a published build should subtract the "pre" column. The other rows are not all proven to have shipped either; that was not traced per row.
- The protocol review contributes 36 of v1.51.0's 40 rows; 9 of them are doc-only contract corrections (marked "(doc)"), and 5 are deferred and OPEN. A rate of code defects should drop the "(doc)" rows.
- Fix commits marked "(unverified)" were not traced to a hash; the release they shipped in comes from the CHANGELOG section.

### Not counted in Table 1

#### Adjacent: defects in lean's delivery, packaging or neighbours (outside the syncer, gateway, store and model)

- s3-csi: a read-only lean bind was read-write on the host (v1.54.0, CHANGELOG.md:232).
- s3-csi, from the 2026-09-03 integrity audit: a fenced (`Succeeded`) lean worker was not relaunched; an exit-78 refusal was not recognised as final; a lost supervisor launch record; `NodeUnpublishVolume` removed a lean tree without a post-SIGTERM drain attestation (v1.45.0, CHANGELOG.md:3908).
- s3-csi: a plugin restart restarted a lean checkout in progress; a republish could start a published workspace over; a pod-level syncer loss was not relaunched; an undrained tree was removed (v1.45.0, CHANGELOG.md:3926; b148319b).
- s3-csi: a published lean workspace's tree could be deleted under the running pod (`is_mountpoint` by device number); found by the kind drill in unreleased code (v1.45.0, CHANGELOG.md:3940).
- s3-csi worker: the credential document lacked `Token`, which the lean syncer's AWS Rust SDK requires (v1.45.0, CHANGELOG.md:3950).
- flint-lean chart: the install notes' example workspace could not be mounted (unreleased, CHANGELOG.md:36); the chart shipped a webhook it no longer had on a binary from before (96ada567); the CRDs still described the webhook (70c18526).
- docs: `docs/flint-lean-for-agent-fleets.md` described the retired webhook shape (unreleased, CHANGELOG.md:42).
- forge export over lean: `export.rs`'s claim that a foreign write "is overwritten by the next export" was false (v1.46.0, CHANGELOG.md:3089); the export's baseline on an emptyDir froze the legible export (v1.46.0, CHANGELOG.md:3117); `2a213b01` (forge lease heartbeat) touched `lean/sidecar`.
- s3 broker: `s3:GetObjectAttributes` in the read session policy; the static secret logged at INFO (v1.54.0).

#### Considered and not counted

- **Design defects the model refuted before code existed**: the chunk-GC ordering rule of chunked-manifest design §8.1, and "adoption must rewrite what it adopts" (lean/formal/README.md:47, :54; a505621e); `LeanDirectMergeInsufficient`, the inbox is load-bearing (README:134); the stale-dirt sync design (`SyncScanFirst`, README:169); the chunked merge may not splice chunk lists (f34f7181; whether code had it is unverified); the scoped-read design's "link 3 is false" (06b3379b); the reader window gap (ae1e51b4); the Phase 4/5/6 design calls (refusal ordered before provisioning, unreadable lifecycle posture, one probe for two callers, grace bound by the reclaim ceiling; boundary-verbs plan:1101-1125).
- **Improvements not framed as a defect**: gateway 0.2.2's overlay cost (f7d44444, a cost L-28's fix introduced in the same release); slice-by-8 CRC-64, compact JSON, largest-first admission (v1.41.0); commit-section tail tracing (4901a037).
- **The writer heartbeat** was added (371dd1b1) and removed (131371f8) the same day: the model showed it carried no safety. A retired mechanism, not a defect.
- **Scope limits listed as known limitations**: v1.38.0 "S3 proxy not built", "one writer per subtree; full checkout only; ~250k file cap", "no enforced data-plane fencing" (CHANGELOG.md:4790-4797); v1.54.0 "gateway binary has no read-only door" (:284); v1.46.0 "one prefix, one writer is not enforced across products" (:3021, the report L-9 rewords).
- **Gated mode** was removed in v1.52.0 (26b75637). Its defects (gated-1..gated-8, L-71, L-72, L-81, L-89, C2, C5, L-93, L-94) stay in Table 1 under the release that fixed them.

