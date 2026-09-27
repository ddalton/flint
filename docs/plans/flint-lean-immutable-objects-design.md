# Lean: immutable object handles — the structural fix for H6

Status: DESIGN ANALYSIS, 2026-09-19. No code. Written to answer three
questions about review finding H6 (`flint-lean-protocol-review-2026-09-18.md`
§H6): what the structural fix is, which defect classes it retires, and what
it costs in requests, bytes and latency. It reopens a trade the delete/rename
design closed ("cheap renames are what a legible layout costs",
`flint-lean-delete-rename-design.md` §2) — with the evidence that the same
layout is also what H6 and five other findings cost.

Every count below was taken two ways where it could be; the second count is
named beside the first. Every latency figure is COMPUTED from request counts
and a stated RTT, not measured, and is marked so.

## 0. Summary

Lean's manifest layer is already shadow-paged: every generation of the
entries is written to a fresh immutable key (`generation_key(seq, flush_uuid)`,
`lib.rs:520`), the pointer CAS is the one commit, and an orphan sweep collects
what a crashed publish left (`manifest.rs:789-830`). The FILE objects are not.
An upload goes to `<prefix>/files/<path>` (`lib.rs:496`), the same key the
committed manifest cites, and overwrites it. Everything that follows —
conditional PUT, the 412 preserve/adopt/park arms, the etag-guarded GC HEAD +
conditional DELETE, the collector-off arm for MinIO and Ozone, the same-bytes
etag findings, checkout's S3-wins adoption, the gateway's "may I overwrite what
sits at the key" rule, the rename-by-copy — exists to arbitrate a slot that
several parties write.

The fix is to give file objects what the manifest already has: **every write
lands at a handle nobody else writes, the manifest cites handles, and a handle
cited by a landed document is never overwritten.** Then the CAS is the only
commit for files too, a checkout reads exactly the cited bytes or nothing, a
lost writer's uploads are uncited garbage rather than half a boundary, and the
arbitration code above has nothing left to arbitrate.

- Retires: 6 defect classes, 24 rows of `lean/FINDINGS.md`, 2 of the 6
  assumptions in `SAFETY.md` §2, and roughly a fifth of the syncer test
  battery's subject matter (§3, with the control counts).
- Costs: the legible path layout, unless the store has versioning (§6.1);
  one DELETE per modified path per barrier (free on AWS; batched 1000 per
  request); transient double storage for modified files inside a barrier;
  ~+15% manifest bytes unless the key is derived (§5.4).
- Gains: no HEAD per delete, so a 10,000-path delete goes from ~20,000
  sequential requests to 10 (§5.2); UI renames from a server-side copy to a
  cell CAS; conditional DELETE no longer assumed anywhere, so the collector
  runs on MinIO and Ozone.
- Effort: 4–6 weeks including the model arm and a three-store live drill
  (§9). Nothing is deployed, so there is no migration (§7).

## 1. The defect class

H6 as confirmed: a peer's uploads land path by path at the cited keys; a fresh
checkout GETs each key `If-Match` the cited etag, meets 412 on the overwritten
ones, and adopts the CURRENT object (`checkout.rs:554-583`). If the peer's pod
is then lost, its committed manifest never arrives: half its change is in every
checkout and half is not. The untracked sweep (`untracked.rs:42-96`) later
tracks the same half, one path at a time, and the next barrier publishes it as
a boundary nobody declared.

The three mitigations that exist are all downstream of the same cause:

| mitigation | what it covers | what it cannot |
|---|---|---|
| the intent journal (`state.rs`, `IntentJournal`), written before the uploads | the writer's OWN container restart: it recognises its half-published uploads | lives in the pod's emptyDir; a pod replacement takes it |
| per-object `flush_uuid`/`epoch` stamps on every upload | grouping a lost writer's objects after the fact | completeness: the key list is only in the journal |
| the untracked sweep, after `UNTRACKED_GRACE_SECS` (600 s) and `untracked_sweep_secs` (3600 s) | eventually tracking what a lost writer left | it tracks the partial set AS a set of paths, which is H6's second route |

"Option 2" from the H6 discussion (a bucket-side intent record) moves the
journal into the bucket. It shrinks the window but leaves the cause: the
prepare phase still destroys the committed version's bytes, so a checkout
during a LIVE writer's barrier still cannot read what the manifest cites.

## 2. The design: immutable handles

A **handle** names one immutable object. The protocol never overwrites a
handle and never guesses one from a path.

- **R1 — every write lands at a fresh handle.** A syncer upload, a gateway
  `put_file`, a draft promote and a sweep adoption each produce a handle no
  other party writes. `If-None-Match: *` stays on the PUT as belt and braces
  (a retry after a torn response is the only way to meet it, and the answer
  is "already there, cite it"), but no guarantee DEPENDS on the store honouring
  it: a fresh key has no competitor.
- **R2 — the manifest cites handles; the pointer CAS is the only commit.**
  `LeanEntry.key` already carries a full key per entry and every reader
  (`checkout.rs:554`, `sync.rs:247`, gateway `get_file`) already reads through
  it. A handle adds an optional `version` (§2.1). A citation REPAIR re-cites
  the handle the tree integrated, by name and with no HEAD — unless the
  document already cites that handle at another path: a rename moved it
  there (a citation move is the only way one handle reaches two names), and
  the path holding it was the source. The model's rename world found this on
  the arm's fourth box run (`Inv_RenameAtomic` at depth 18): a writer adopted
  a UI write, the UI renamed the path, a peer performed the rename, and the
  adopter's repair re-cited the moved handle at its old name. The slot had
  made that impossible by being collected at the source; a handle kept for
  the destination is still there to be found. The clause has its own pin
  now: `RepairRespectsMoves=FALSE` (`LeanImmutableRepairRecites`) is the
  fourth run's shape. Two rules the clause depends on were missing, found
  by the seventh box run and then, sharper, by the core model's first run
  (L-117; the earlier draft of this paragraph re-minted a refused rename's
  destination as a copy, which the core model showed answered the wrong
  question): a rename takes the source's PENDING entry with the citation it
  moves — the entry is that citation, and left behind it had a writer adopt
  one handle at both names — with the removal naming the version the entry
  was written over, so a tree holding that older version applies the move
  instead of refusing it as superseded (`Removal.over`; H4 protects a
  newer version, not that one); and a removal one writer REFUSED still
  moves the named version out of any tree that holds it clean at the source
  — a refusal is one tree's answer, not the document's — so the seventh
  run's shape (the agent's edit at the source deleted after the refusal)
  resolves in one barrier, the source a declared removal, and the
  destination cited once (`apply_removals`; the model's `RenameMovesEntry`
  and `AnsweredRecordsApply`, `LeanImmutableAnsweredRecordSkipped`). And
  the eighth box run (L-118, `Inv_HITLDurable` at depth 18): a repair
  YIELDS to a later acknowledged UI write the document already cites at
  the path — the UI's own hand — and the tree takes the newer version
  through its queue; as shipped the merge let the repair win as an upload
  does, and a peer's publish of the user's newer write was retired by a
  writer that never saw it (`void_stale_repairs`, the model's
  `RepairYieldsToLaterUI`, `LeanImmutableRepairOverridesUI`). A repair the
  commit publishes over a version the tree never integrated is surfaced
  like an upload (R7) — the code's merge always did; the model surfaced
  uploads only.
- **R3 — a handle cited by a landed document is never overwritten, and is
  deleted only once no document cites it.** Retirement is decided at the CAS:
  the retired set of a barrier is every handle the base document cited that
  the installed document does not (a modified path retires its old handle; a
  deleted path retires its handle), **minus every handle the installed
  document still cites at another path and every handle an inbox entry names**
  — a rename cites the source's handle at the destination, and a rename whose
  destination is still only in the cell would otherwise lose its bytes. A
  retired handle can never be re-cited — nothing discovers handles from paths,
  the merge carries forward only what the base cites, and the sweep (R5) cites
  only ingress objects — so the collector deletes it **unconditionally, without
  a HEAD, without a lease and without a fence**. That removes the C2 renew-
  before-delete dance (`barrier.rs:1666-1668`) along with the conditional
  DELETE. The model refutes the per-path rule (`LeanImmutableRetirePerPath`,
  `Inv_NoDangling`, a 9-state trace): a UI rename's destination cites the
  source's handle, the source's removal retires it, and a collector judging
  path by path takes it from under the destination. The FIRST trace of that
  world, at the same depth, had no rename in it: the model's generation
  numbers were being read as handle names, and the seed generation — one
  number at every path, a different handle at each — was collected at p2 for
  p1's retirement. A handle is a name (`SameHandle`; a rename's citation move
  is the only aliasing); the refutation was withdrawn and re-established
  through the rename it was written about.
- **R4 — orphans are swept under the lease, and the commit re-reads its own
  uploads.** A handle cited by no document, named by no inbox entry and
  preserved by no conflict record is garbage: a lost writer's upload, a UI
  write's superseded predecessor, a removal's leftover. The sweep that
  collects it runs **inside a barrier's commit section**, under the cell, and
  a writer never sweeps its own in-flight uploads (it knows its flush id).
  The commit **re-reads every handle its own uploads cite as its own step
  before the CAS** — one HEAD per uploaded handle, the code's
  `verify_observed_citations`, already paid today — and withholds what the
  sweep took; the path stays dirty and re-uploads next barrier. That pair is
  what makes the sweep safe: the lease keeps another writer's sweep out of the
  window between the re-read and the CAS, and the re-read keeps a swept upload
  out of the document. The sweep's age grace (the manifest sweep's
  `ORPHAN_GRACE_SECS` shape) is a cost lever, not a safety one: the model runs
  the sweep with no grace at all.

  This rule was first drafted the other way — "judge orphans by the lease
  epoch stamped on the upload; a stamp below the cell's epoch is a dead
  writer's" — and writing the model arm refuted it before any run: under the
  per-barrier lease the uploads PRECEDE the claim, so a live writer's uploads
  carry its previous epoch and would read as dead. The two refutations that
  pin the rule as it now stands: a lease-free sweep lands between a writer's
  re-read and its CAS (`LeanImmutableSweepLeaseFree`, `Inv_NoDangling` at
  depth 11), and a commit that cites without re-reading cites a swept handle
  (`LeanImmutableCasCitesBlind`, depth 19). A sweep that does not spare what
  an inbox entry names takes an acked UI write (`LeanImmutableSweepTakesTracked`,
  `Inv_HITLDurable` at depth 9).
- **R5 — `files/<path>` is the INGRESS namespace, not the store.** An outside
  writer's `aws s3 cp` still lands there. The sweep lists it, and for an object
  whose etag no entry has adopted, appends an inbox entry naming a handle: the
  object's own (key, version) on a versioned bucket, or a server-side copy to a
  fresh key (`x-amz-copy-source-if-match` the listed etag, so exactly the
  version it judged) elsewhere. The entry records the ingress etag it adopted;
  the sweep never deletes an ingress object and never needs a conditional
  DELETE. A later outside overwrite is a new ingress, judged the same way.
- **R6 — a rename is a citation move.** The gateway's `POST /rename` appends
  an inbox entry for `to` citing the SOURCE's handle and a removal for `from`;
  no bytes move (`workspace.rs:1075-1120` today: `copy_object`, a HEAD on 412,
  and a second copy). A syncer that finds a new local path whose CRC-64 and
  size equal a retired entry's may cite the retired handle instead of
  uploading (an optimisation, not a rule: the handle is immutable, so citing
  it under a second path is a manifest edit).
- **R7 — a commit that publishes over a citation it never integrated
  surfaces it.** The slot answered this at the PUT: If-Match on the writer's
  last known etag, a 412 when a peer's publish or a UI write consumed
  elsewhere had moved the key, and under `Upload412Preserves` the foreign
  version preserved and superseded knowingly. A fresh handle has no slot to
  fail on, so the CAS — the one place the citation is read — takes the same
  decision: for every path the commit uploaded whose citation is neither its
  merge base nor a generation it integrated, the cited handle is surfaced as
  a conflict (the record names the handle; the copy is fetched from it in the
  same commit section, before the collector runs) and the upload is cited
  over it. The model's first HitlOverAny run under handles had no such step
  and cited an acked UI write a peer had published over blind
  (`Inv_HITLDurable` at depth 18); its consume had also read "the citation
  moved to the entry itself" — a peer's publish of that very write — as
  "superseded", and dropped it. Both are rules now: an entry whose citation
  moved to its own handle is LIVE (a clean tree adopts it, a dirty one
  surfaces it and publishes over it knowingly), and the CAS surfaces what it
  publishes over (`CommitSurfacesForeign`; `LeanImmutableCasOverridesUI` and
  `…CasOverridesPeer` are the shape without it, for an acked write and for a
  peer's own publish).

### 2.1 Two realisations, one protocol

**Decided 2026-09-19: one realisation.** Fetch-by-bare-path is relaxed
everywhere, so the unversioned row below is THE layout on every store and the
versioned row is not built: no version-id plumbing, no dependence on bucket
versioning, one e2e matrix, one model. The table stays as the record of what
was weighed.

| store | the handle | legibility | orphan backstop |
|---|---|---|---|
| a versioned bucket (S3; MinIO with versioning, which `bootstrap` already recommends — A9, `s3.rs:1281-1291`) | `(files/<path>, versionId)`: the PUT goes to the path key with NO condition and returns `x-amz-version-id` (`probe.rs:68` already requires it); reads are `?versionId=`; the collector calls `delete_version` (`flint-store` has it: `head_version`, `get_version`, `delete_version`, `list_versions`) | `files/<path>` is still the tree — the newest version at each path, exactly as today | a `NoncurrentVersionExpiration` rule; R4 for current-version orphans |
| an unversioned store (Ozone; MinIO without versioning) | a fresh key: `files/<path>@<flush_uuid>` (sorts beside the path — a listing of `files/src/` shows every live version of `f1.bin` together) | semi-legible: the path is readable in a listing, but `aws s3 cp` of the bare path is gone; a passthrough mount of `files/` no longer sees the tree (§6.1) | R4 sweep |

The syncer, the gateway, the merge, the collector and the MODEL see one thing:
a handle. Only the store adapter differs, and the conformance probe already
tells the two apart (`probe-versions`).

### 2.2 What each barrier step becomes

| step | today (`barrier.rs`) | with handles |
|---|---|---|
| 3 intent journal | keys = `file_key(p)` per upload | (flush, path) → handle per upload; still pod-local, now an optimisation (a restart re-uploads instead of recognising) |
| 4 uploads | conditional PUT; 404-on-If-Match arm; 412 → HEAD → own/foreign/same-crc → preserve copy (GET+PUT) → second PUT → park (`upload_one`, 195 lines; `upload_compose` repeats it) | PUT to a fresh handle; one retry arm (own torn response → HEAD → cite). ~30 lines |
| 5 manifest CAS | three-way merge; citation repairs HEAD the path key and re-cite what matches the baseline etag (`barrier.rs:1312-1335`) | merge unchanged; repairs cite the baseline's HANDLE if the base still cites it, else theirs wins — no HEAD |
| 6 GC | per deleted path: renew if stale, HEAD, `delete_if_match`, skip-and-record on an unrecognised etag, leak on a collector-off store (`barrier.rs:1666-1800`) | per retired handle: unconditional delete, batched (`DeleteObjects`, 1000 per request — a new `delete_many` on the trait) |
| consume | GET `file_key(path)` If-Match the entry etag; "already integrated" by etag | GET the entry's handle; identity by handle |
| checkout / sync | GET If-Match; 412 → S3-wins adopt (or refuse under `sole_writer`); 404 → hole | GET the handle; 404 → the pointer moved and the old handle was collected → re-resolve the pointer, retry. `If-Match` is redundant on an immutable handle; the CRC-64 verification stays |

## 3. What it retires

Six classes. For each: the `FINDINGS.md` rows, the code arm, and what pins it
today. The control for the row count is the test-name census at the end of
this section.

**Class 1 — same-key write races.** F1/L-10 (GC HEAD then unconditional
delete took a peer's upload), F2/L-11 (an upload cited bytes it found at the
key; the peer's collector removed them), F8/L-16 (a UI write overwrote an
uncited upload), finding 11/L-17 (S3's 404 on `If-Match` at a missing key
wedged every barrier), L-24 (a supersede left the peer citing a generation the
key no longer held), atomicity-2 (the compose 412 recogniser ignored
`prior_uuids`), inbox-1 (a park never un-parked; the drain removed the tree),
inbox-5 (a 412 whose object was gone), M7/L-113 (a late append replaced the
entry of a newer write). Code: `upload_one`'s 412/404 arms,
`preserve_foreign_412`, `preserve_conflict_copy`, `UploadOutcome::Parked`,
`hitl_may_overwrite` (`inbox.rs:579`), `gateway_append`'s HEAD (`inbox.rs:255`),
`put_file`'s HEAD-then-conditional-PUT (`workspace.rs:706-770`).

**Class 2 — same-bytes etag aliasing.** finding 13 / L-19 and its second route
L-20, the `MaxSameBytes` model arm, and the false positive it caused in
`Inv_HITLTracked` (FINDINGS "model" table, 2026-09-14). Two writes of the same
bytes get two handles; an etag is never asked to name a VERSION again. The
`SAFETY.md` §2 row "an etag names the bytes" is deleted, not weakened.

**Class 3 — the conditional DELETE.** L-102 (MinIO ignores `If-Match` on
DELETE), L-27 (Ozone, HDDS-14907), C2/L-104 (the GC fence was a count; DELETE
has no fencing token), H1/L-105 (the leaked object resurrected a published
delete and flapped forever), H1b/L-106 (the sweep's entry outlived its
citation). Code: the collector-off decision (`conformance.rs:182-190`),
`conditional_delete_enforced`, `report.leaked`, the GC HEAD, the renew-before-
delete, `LeanBarrierLeaseGCUnconditional` and the `LeakResurrects`/`LeakFlaps`
worlds. The `SAFETY.md` §2 row "the store honours `If-Match` on DELETE" —
false on two of the three stores we run — is deleted. `SAFETY.md` §4 item 7
("storage growth ... on a store without a conditional DELETE") goes with it.

**Class 4 — S3-wins adoption.** H6 itself, L-53 (a reader adopted a foreign
write into forge's mirror), C5 (enabling gated on an existing workspace adopted
mid-change bytes), L-28 (the gateway read 409 `moved` until the syncer
re-cited), and the `sole_writer` refusal arm (`checkout.rs:556-570`), which
exists only to refuse an adoption. A checkout reads the cited handle or
re-resolves the pointer; there is no "current object" to adopt.

**Class 5 — the UI door's overwrite judgement.** L-48 (two browsers both got
200) stays fixed but moves: the caller's `If-Match` is judged in the inbox
CELL CAS against the tracked-or-cited handle, atomically, rather than at the
object PUT and again at the append (today `judge_preconditions` then
`ConcurrentWrite` on the HEAD-to-PUT window, `workspace.rs:743-770`). The
drafts' `stale` flag (`drafts.rs:280`, `:360`) compares against the cited
handle, not a HEAD of the path. The consume's half of the same judgement
(today the 412 arm's "preserve, then supersede knowingly"): the entry records
the citation the gateway saw when it appended (`InboxEntry.cited`, the field
the sweep's H1b rule already uses), and a consume adopts it only while that is
still the citation at the path. A citation that moved to a later UI write
supersedes the entry (dropped, retired by the UI's own hand); one that moved
to a writer's publish makes it a conflict — the acked bytes stay at their own
handle, preserved by a server-side copy under `conflicts/` and surfaced, never
adopted over the published version and never dropped in silence. A delete
(citation 0) is modify-wins, as it always was. A citation that moved to the
entry's OWN handle is a peer's publish of that very write: the entry is live
and the ordinary rules apply (the model's first HitlOverAny run read it as
"superseded" — R7). "Later" is read from the citation alone: a gateway handle
is named with the cell sequence its append took (`files/<path>@ui-<seq>`), so
a consumer knows from the name whether a citation is a UI write and whether
it is later than its own entry; a citation that moved to an EARLIER UI write
(published by a peer before this entry replaced it in the cell) leaves the
entry live — the newer write wins, and the older is surfaced at the CAS by
R7. That is today's outcome for a UI write that raced an agent's upload (the
agent's version wins, the UI's is preserved), reached without the 412.

**Class 6 — rename by copy.** The delete/rename design's §7 prerequisite (a
cross-key server-side copy, its 5 GiB single-copy wall, and the
MPU+`UploadPartCopy` arm that "has never executed anywhere",
`flint_sync.rs:198`) is no longer on the rename path. It remains for the
unversioned-store ingress copy only (R5), where objects arrive one at a time.

**Simplified, not retired.** H1d/H1e's tombstones (`LeanManifest.tombstones`)
still tell "adopted while pending, then knowingly deleted" from "a delete raced
the write", but they name a handle rather than an etag, and the same-bytes
ambiguity in that judgement (FINDINGS 2026-09-18 note on
`Inv_NoDeleteResurrected`'s first drafts) is gone. H4's removals name a handle.
The intent journal survives as a restart optimisation.

**Not touched.** The lease, the epoch, the pointer rotation and the straggler
fence (H2, C2's pointer half, L-5); the inbox cell's own CAS (H3's 409); every
local-tree race (H7, H5's symlink walk, the two-scan rule); the CRC-64
verification (L-33/L-34); the sentinel and its ack (H10, the carrier); the
merge's delete-vs-modify rules; budgets and fan-out.

**The control.** Two counts of "how much of the shipped behaviour is about the
shared key":

| method | count |
|---|---|
| `FINDINGS.md` rows classified above (classes 1–6) | 24 rows of the table's ~120 |
| syncer/gateway test names containing `412`, `adopt`, `preserv`, `park`, `overwrit`, `untracked`, `gc_`, `leak`, `moved`, `supersed`, `foreign` (`grep -c "fn .*<word>"`, overlaps not removed) | 5+18+5+3+8+1+3+1+2+4+9 = 59 name hits over 253 syncer and 34 gateway-verb tests |

### 3.1 The census after the code tranche (2026-09-19)

What the store, syncer and gateway code of tranches 1, 3 and 4 (§9) retired
from the syncer battery and the gateway's verb tests, and what stands in each
place. A retired test's PREMISE is gone with the slot: nothing it staged can
arise. The replacement pins the rule that made it impossible, against the
model world named in §8.1 where one exists.

| retired (syncer `tests.rs` unless noted) | class | stands in its place |
|---|---|---|
| `foreign_412_is_preserved_and_superseded_never_silently_overwritten` | 1 (412 preserve) | `a_version_published_under_the_agents_edit_is_surfaced_at_the_commit_never_silently_overwritten` (R7) |
| `gc_refuses_unrecognized_etag` | 1 (GC HEAD guard) | `a_delete_retires_only_the_handle_it_cited_never_a_ui_re_creation` |
| `a_vanished_base_recreates_{on_s3s_404, on_ozones_412, through_compose_on_s3s_404, through_compose_on_ozones_412}` | 1 (L-17) | `a_collected_predecessor_costs_the_upload_nothing_{on_s3s_404, on_ozones_412, through_compose}` |
| `a_parked_path_is_preserved_and_published_over_not_abandoned`, `a_boundary_with_a_standing_park_is_partial_and_the_drain_does_not_attest_it` | 1 (park) | `a_commit_withholds_an_upload_a_peers_sweep_took_and_uploads_afresh_next_barrier` (R4, `LeanImmutableProbeUploadWithheld`) |
| `a_claim_that_reaches_the_deadline_fails_the_barrier_and_the_retry_adopts_the_uploads`, `a_crashed_compose_is_adopted_after_an_edit_not_parked_forever` | 1 (adopt-own) | `a_restarted_barrier_uploads_afresh_and_leaves_its_earlier_handles_to_the_sweep` |
| `a_peer_put_over_an_identical_bytes_upload_is_not_cited_as_the_old_version`, `a_peer_upload_between_the_gc_head_and_its_delete_is_not_deleted`, `a_peer_upload_of_identical_bytes_before_the_gc_delete_is_not_deleted`, `an_adopted_upload_deleted_by_the_peer_before_the_claim_is_not_cited` | 1, 2 (F1, F2, finding 13) | `a_peers_in_flight_upload_is_untouched_by_anothers_collector` (`LeanImmutableGCUnconditional`, `…AdoptBlind`) |
| `a_store_that_ignores_if_match_on_delete_leaks_the_object_instead_of_collecting_it`, `a_peers_delete_on_a_collector_off_store_reaches_the_other_tree_and_is_not_resurrected` | 3 (L-102) | `a_store_without_a_conditional_delete_collects_just_the_same` (`LeanImmutableLeakHolds`) |
| `a_pending_adoption_cited_and_then_knowingly_deleted_is_not_re_cited`, `…cited_by_its_own_writer_that_restarted…`, `…of_an_entry_that_outlived_its_citers_restart…`, `a_queued_delete_superseded_by_a_re_cite_that_is_deleted_again_before_the_install_still_leaves_the_tree`, `an_adopted_ui_write_deleted_under_an_older_peer_publish_converges_when_the_{pod_is_replaced, writer_survives}`, `a_repair_citation_withheld_at_the_commit_still_receives_the_version_that_replaced_it` | 3 (H1c-f leaks) | `a_pending_adoption_cited_then_knowingly_deleted_cannot_be_re_cited_its_handle_is_gone` |
| `a_sweep_adoption_is_void_once_the_citation_it_was_judged_against_moves`, `a_sweep_entry_that_outlived_the_citation_it_was_judged_against_does_not_resurrect_a_delete`, `a_voided_sweep_adoption_of_a_path_the_merge_base_never_had_is_removed` | 4 (finding 10, H1b) | `an_ingress_adoption_is_void_once_the_citation_it_was_judged_against_moves` (R5) |
| `a_writer_killed_after_its_upload_does_not_leave_the_trees_diverged`, `the_floor_sweeps_for_untracked_uploads_only_past_the_grace_and_when_due` | 4 (finding 10) | rewritten in place: the orphan is collected, never adopted (`LeanImmutableOrphanCollected`, `…ProbeSwept`); `the_floor_sweeps_for_ingress_objects_only_past_the_grace_and_when_due` |
| `a_ui_write_over_an_uncited_upload_is_never_silently_lost`; gateway `a_blind_write_over_an_uncited_upload_is_refused_until_it_is_cited` | 5 (F8) | `a_ui_write_landing_beside_an_in_flight_upload_is_preserved_never_lost` (`LeanImmutableHitlOverAny`); gateway `a_ui_write_beside_an_uncited_upload_lands_at_its_own_handle_and_is_tracked` |
| `a_late_inbox_append_never_replaces_the_entry_of_a_newer_write` | 5 (M7) | `a_sweeps_late_append_never_replaces_a_gateway_entry` |
| `a_publish_that_lands_mid_checkout_names_the_publisher_not_a_stranger` | 4 | `a_publish_that_lands_mid_checkout_is_re_resolved_not_blamed_on_a_stranger` |
| `a_holder_deposed_after_its_cas_landed_does_not_collect_what_the_successor_re_cited` | C2 | no unit test: the deposal-after-CAS is staged by the slot's stall hook, which is gone; `LeanImmutableStragglerGC` (the model) holds, and the drill's C2 leg is the live check |
| gateway `a_rename_overwrites_its_own_orphan_but_never_a_strangers_object` | 6 | `a_rename_moves_no_bytes_and_a_tracked_destination_refuses` (R6) |
| gateway `a_read_of_an_unmodified_cited_file_never_fetches_the_inbox`, `an_overwrite_of_a_cited_file_reads_at_once_and_an_outrun_entry_yields_to_the_citation` | 5 (the read door) | `a_read_costs_one_cell_fetch_and_one_object_fetch_whatever_lingers_in_the_cell`, `…and_a_judged_entry_yields_to_the_citation` |
| — | R2 (new) | `a_repair_never_re_cites_a_handle_the_document_moved_to_another_path` (the rename world's fourth-run gap) |
| — | L-117 (new) | `a_refused_renames_destination_is_published_once_the_source_edit_is_gone` (the rename world's seventh-run gap: the shipped rule fails its first assertion), `a_removal_one_writer_refused_still_moves_the_handle_out_of_a_tree_that_holds_it_clean` and `a_renames_source_entry_moves_with_it_and_is_never_adopted_at_the_old_name` (the core model's two traces; each fails as shipped) |
| — | L-118 (new) | `a_repair_yields_to_a_later_ui_write_the_document_already_cites` (the rename world's eighth-run gap; fails as shipped) |
| — | L-119 (new) | `a_rename_refused_at_a_dirty_source_still_completes_once_the_source_leaves` (the tenth run's gap: a refused rename's destination dropped when its adopter committed before the source left; fails as shipped). The rule is in both models as `PendingAdoptionStays`, with `LeanImmutablePendingAdoptionDropped` / `LeanCorePendingDropped` refuting it and `…ProbePending` showing the shipped world reaches it |

Every remaining test that planted or read an object at `files/<path>` reads the
cited handle or the path's handle set instead (`cited_key`, `handles_for`,
`cited_bytes`, `object_at`, `peer_publish` in the syncer battery; `tracked`,
`handles_of` in the gateway's). Syncer battery: 238 tests green; gateway: 24
battery, 26 verb, 5 doc, 1 unit.

Both say the same thing: a substantial minority, about a fifth, of what the
battery pins is arbitration of a slot the design removes. Those tests are
retired WITH their findings (marked "retired by design" in the table, not
"fixed"), and the design's own rules get new ones (§8).

## 4. What it does not fix

- **A live writer's barrier is still not a boundary until its CAS.** Nothing
  changes about WHEN a change becomes visible; only that nothing partial is
  visible before it.
- **UI writes are still an overlay.** The inbox entry is visible to readers
  before a barrier cites it, by design (gateway README: "readable by everyone
  the moment `put_file` returns").
- **Outside writers to `files/<path>` are still adopted path by path** (R5).
  That is the ingress contract, and it now applies ONLY to outside writers;
  a lost flint writer's set is never adopted (R4 deletes it).
- **Storage for orphans until the sweep runs.** A lost writer's uploads sit
  uncited until the next sweep (hourly today). A cost, not a loss — and today
  the same bytes are the H6 hazard.

## 5. Performance

Notation: a barrier with U uploads of which M modify a path the base cites,
D deletes; a checkout of N files; RTT r. Today's counts are from the code
cited; "with handles" from §2.2. Latency is COMPUTED as sequential RTTs where
the loop is sequential (the GC loop is: one `await` per path,
`barrier.rs:1670-1760`).

### 5.1 Requests per barrier

| phase | today | with handles | change |
|---|---|---|---|
| uploads | U PUT (+ per 412: HEAD, GET+PUT preserve, PUT — "reached almost never", `barrier.rs:2143`) | U PUT | 0 in the common case; the rare arm is gone |
| GC | D × (HEAD + conditional DELETE) + a renew every 20 s of stale cell | ⌈(M + D)/1000⌉ batched DELETE, or M + D single DELETEs | −2D + (M+D): +1 per MODIFIED path, −1 per deleted path; batched, near zero |
| citation repairs | 1 HEAD per repair candidate | 0 | −1 per candidate |
| cell (fence, window) | 4 | 4 | 0 |
| manifest | entries PUT + pointer CAS | same | 0 |

On AWS S3, DELETE and batch DELETE requests are not billed, so the +1 per
modified path is zero dollars; on MinIO and Ozone it is one local request.
The requests that leave are HEADs (billed as GET-class) and the per-path
serial dependency.

### 5.2 Latency of the GC step (computed)

| deletes | today, 2 sequential RTTs each (at r = 20 ms) | with handles, batched |
|---|---|---|
| 100 | 4 s | 1 request, ~20 ms |
| 1,000 | 40 s (plus one renew) | 1 request |
| 10,000 (`rm -rf build/`) | 400 s (plus ~20 renews) | 10 requests, ~0.2 s |

The deletes run AFTER the CAS, so today's figure is time the boundary is
already visible but the barrier is still holding the lease; with handles the
lease is not needed for the deletes at all (R3), and the writer could release
before collecting.

### 5.3 Bytes

| flow | today | with handles |
|---|---|---|
| upload | file bytes once | same |
| checkout / sync | file bytes once (+ a second GET per 412 adoption) | same, no second GET |
| UI rename of B bytes | server-side copy of B (multipart above `copy_whole_max`); a directory of n files is n copies | 0 bytes; one cell CAS for the whole batch |
| local `mv dir/` by the agent (n files, B bytes) | B uploaded again + n × (HEAD + DELETE) | B uploaded again by default; 0 with the R6 cite-by-content optimisation |
| conflict preserve | GET + PUT of the foreign version (`preserve_conflict_copy`) | 0: the foreign bytes already sit at their own immutable handle; the conflict record cites it |

### 5.4 Storage and metadata

- **Inside a barrier:** each modified file exists twice between its PUT and
  the GC delete — seconds to minutes. Today it exists once (the old bytes are
  destroyed by the upload, which is the defect). Across the fleet this is
  bounded by the churn of one barrier per writer, not by tree size.
- **Orphans:** a lost writer's uploads until the sweep (§4). Today: zero
  orphans, because the same bytes overwrite committed data instead.
- **Manifest bytes:** `LeanEntry.key` is serialised in full (`serde_json`,
  `manifest.rs:106`). A key carrying a flush uuid adds ~37 bytes to an entry of
  ~277 (66 MiB at the 250k cap, `workspace.rs` `status` comment): +13%. A
  versionId adds ~32 (S3) to ~36 (MinIO). Avoidable by deriving the key from a
  per-manifest table of flush ids and a 4-byte index; not worth doing first.
- **Versioned buckets:** noncurrent versions are billed until deleted. The
  collector deletes the exact retired version at step 6, so steady-state
  storage equals today's; `NoncurrentVersionExpiration` is the backstop for a
  writer lost mid-barrier, and `bootstrap` already recommends it.

### 5.5 Requests per gateway verb

| verb | today | with handles |
|---|---|---|
| `put_file` | HEAD + manifest load + inbox load (`hitl_may_overwrite`) + PUT + cell CAS (GET+PUT) ≈ 6 | PUT + cell CAS ≈ 3 |
| `get_file` | GET If-Match; on 412 a cell GET and a second GET | GET the cited handle; the cell GET only when the path is uncited (as today) |
| `rename` (n files) | n × COPY (+ HEAD and a second COPY on 412) + cell CAS | cell CAS |
| drafts list | 1 HEAD per draft for `stale` | 0: compare the base handle to the citation already in hand |

### 5.6 What does not change

Checkout throughput (the disk is the floor; `checkout.rs:342`,
`lib.rs:316`), upload fan-out and the byte gate, the budget, the cell's four
requests, the sentinel's cost, the chunked manifest's cost.

## 6. Costs and risks

### 6.1 Legibility — the trade being reopened

`docs/flint-lean-for-agent-fleets.md:327` promises "the `files/` prefix is
the workspace", and the delete/rename design §2 records that a passthrough
mount pointed at `files/` sees the tree (the 2026-09-10 door drill), which is
why lean "deliberately does not have" a content-addressed layout.

- On a **versioned bucket** the promise holds unchanged: `files/<path>` is the
  newest version at the path, which is what it is today (today it is the
  newest UPLOAD, committed or not; with handles, likewise). The passthrough
  door works.
- On an **unversioned store** (Ozone) the path becomes `files/<path>@<flush>`:
  listable beside the path, not fetchable by the bare path. The passthrough
  door over an Ozone workspace is lost. An optional post-commit **legible
  mirror** (a server-side COPY of each changed handle to `files/<path>` after
  the CAS, never read by the protocol, with the mirror's etag recorded so the
  ingress sweep can tell an outside write from its own copy) restores it at
  one COPY per changed file per barrier and permanent double storage; it
  would be off by default and on for forge's legible export. This is the one
  place the never-executed multipart-copy arm would matter again (files above
  the single-copy limit).

The decision this asks for: is a fetch-by-bare-path on Ozone worth more than
classes 1–4? The evidence of the two review sessions says no, but it is a
product call.

### 6.2 Versioned-realisation semantics

- A DELETE of a path key would create a delete marker; the protocol never
  issues one (the collector deletes VERSIONS). `list` sees current versions
  only, which is what the ingress sweep wants.
- `get_range_segments`, `compose_generation` and `copy_object` must carry a
  versionId (CompleteMultipartUpload and CopyObject both return one).
- A bucket whose versioning is later SUSPENDED would hand every PUT the null
  version: the probe that already requires `x-amz-version-id` refuses that
  before the first verb.

### 6.3 Other risks

- **Two realisations in one adapter.** Contained by a single `Handle` type
  in `flint-store` and one conformance decision; the protocol and the model
  never branch on it. The e2e rig must run BOTH (MinIO with versioning on,
  and Ozone), or the unversioned arm is the one nothing reaches.
- **The retired-set computation at the CAS** is new load-bearing code: a
  handle wrongly retired is a hole in every checkout. It is the one place a
  `by construction` claim must be a test (§8), and the model has already
  refuted its first draft once (R3: the rename).
- **Batch delete** is a new store operation; Ozone's `DeleteObjects` support
  is to be probed, not assumed.
- **The model's state space** grows (a set of live generations per path
  instead of one); MaxGen stays small and retirement is prompt, so it should
  stay within the current worlds' budgets. Unknown until run.

## 7. Migration

None. Nothing is deployed (standing note). The layout change lands as the
layout; the drills that write `files/<path>` directly to stage a foreign write
(`lean/e2e/writers-live/hostlegs.sh`, `lean/e2e/perf/ozone-probe/oz-drill.sh`,
`drafts-rescope-drill.sh`) become ingress legs and are re-scripted, not
preserved.

## 8. How we would know

Falsifiers, in the order they would be built (test first: each on the
unfixed tree, refuting; each fix line mutation-checked).

1. **Model — BUILT 2026-09-19** (`lean/formal/LeanSubtree.tla`, the
   `ImmutableObjects` arm; `formal/README.md` "Immutable object handles").
   `objects` is frozen (there is no slot) and `versions[p]`, the substrate the
   gated tranche left behind, is the set of live handles at `p`.
   `UploadIO`/`HitlWriteIO`/`HitlRenameIO` register a handle with no
   condition; the CAS records per path what it stopped citing (`sc.retire`);
   `GCCollect` deletes the retired set in one unconditional batch, sparing
   what the installed document cites elsewhere and what an inbox entry names;
   `VerifyUploads` is the commit's own re-read step; `SweepOrphan` runs in a
   commit section; the consume judges a gateway entry against the citation the
   gateway saw (`gh.uiBase`, the entry's `cited`); the CAS surfaces what it
   publishes over (`CommitSurfacesForeign`, R7). Twenty-one gate runs: four
   rules refuted by their mutations (`RetirePerPath`, `SweepSparesTracked`,
   `SweepUnderLease`, `CommitSurfacesForeign` — the last in two worlds, an
   acked UI write and a peer's own publish) and the commit's re-read by
   `VerifyUploadedCitations` (box depths 11, 9, 12, 19, and 19 and 20 for
   the R7 pair); six
   known-bad shapes of the retired classes run under handles with the
   same-key mutation left on and inert, holding every handles invariant; five
   probes showing the new steps fire. The census control arm re-runs the
   shipped-shape worlds unchanged. `Inv_NoStaleOverride` stays in the handles
   worlds — its ghost has a handles form, the CAS citing an upload over a
   citation the writer never integrated with no record — and
   `Inv_QuiescentConverged` is not checked there: under a lease-bound sweep,
   quiescence precedes the sweep, and the harm the invariant named (finding
   10) has no state to occupy.

   **What the first box run of the arm found (two design gaps, both now
   rules).** (a) `Inv_HITLTracked` in every world with a UI write: a writer
   consumed the UI write, its agent then deleted the path, the install
   retired only the old citation, and the consumed-but-never-cited UI handle
   was left as garbage with no decision recorded. The slot had expressed the
   decision by being overwritten. Rule: a writer that integrated an acked
   write and publishes its own edit or delete of the path retires it AT ITS
   CAS — the collector's-decision wording extended to what the tree
   integrated (`gh.hitlRetired`, stamped in `CASInstall`). (b)
   `Inv_RenameAtomic` in the rename world: a UI rename to a destination that
   another writer is concurrently creating. The gateway cannot see an
   in-flight upload — nothing sits at a key — so the rename lands, the
   install cites the agent's file at the destination while the source still
   cites the moved handle, and the rename's entry is then stale. Rules: the
   atomicity claim under handles is "the same handle is never cited under
   both names"; a stale destination entry is a TAKEN destination, so the
   source's removal is refused and the source stays, the moved bytes surviving
   as a conflict copy at the destination — the delete/rename design's §5
   outcome, reached by the consume rule of class 5 rather than by a key
   collision.

   **What the second box run found (one gap, one withdrawn refutation).**
   (c) `Inv_HITLDurable` at depth 18 in the HitlOverAny and rename worlds,
   with (a) and (b) in place: the UI writes a path and is acked; writer B
   consumes the entry and publishes it; writer A, dirty at the path from an
   edit made against the old version, consumes the same entry — and the
   consume read "the citation moved to the entry itself" as "superseded",
   dropped it and learned nothing; A then uploaded to a fresh handle and its
   CAS cited that over the published UI write. Under the slot A's PUT would
   have failed If-Match on the old etag and `Upload412Preserves` would have
   surfaced the UI's version and superseded it knowingly; a fresh handle has
   no slot to fail on. Two rules (R7): an entry whose citation moved to its
   own handle is LIVE, and the CAS surfaces what it publishes over.
   (d) Reading the `RetirePerPath` trace while at it: no rename in it — the
   collector had taken p2's seed generation for p1's, the model's numbers
   read as names. `SameHandle` fixed the model (the collector, the sweep and
   the quiescence invariant are by name now), and the refutation was
   re-established through the rename (R3 above).
2. **Unit.** `a_checkout_reads_only_what_the_manifest_cites` (a peer's
   in-flight uploads never appear); `a_lost_writers_uploads_are_collected_not_
   adopted` (R4: the sweep runs in a commit section, a writer's own in-flight
   uploads are spared, and a peer's are re-read before its CAS cites them);
   `a_stale_ui_entry_is_a_conflict_not_an_adoption` and
   `a_ui_entry_superseded_by_a_later_ui_write_is_dropped` (the consume rule of
   class 5); `a_retired_handle_is_cited_by_no_later_document` (R3, across a
   rename and a modify in the same barrier); `the_gc_needs_no_lease` (deletes
   after release); `a_rename_moves_no_bytes` (request census);
   `an_ingress_object_is_adopted_once` (R5); the retired-set computation under
   every merge outcome; the versioned and unversioned adapters against the
   same protocol tests.
3. **Live.** The writers drill's host legs on S3 (versioned), MinIO
   (versioned) and Ozone (unversioned): the foreign-write leg becomes the
   ingress leg; the lost-writer leg (`kubectl delete pod` between an upload
   and its CAS) must show a checkout identical to the last boundary and the
   orphans collected by the next sweep; the collector-on-MinIO leg that L-102
   made impossible.

## 9. Effort and sequencing

| tranche | scope | estimate |
|---|---|---|
| 1 | `flint-store`: `Handle`, `put` returning a handle, get/delete by handle, `delete_many`, versioned + unversioned adapters, probes | 3 days |
| 2 | the model arm and its worlds; the gate | 1 week (TLC time on the box included) |
| 3 | syncer: upload, GC, consume, checkout, sync, sweep, manifest entry, conflict records; retire classes 1–4's arms and tests; new tests | 1.5–2 weeks |
| 4 | gateway: `put_file`, `get_file`, rename, drafts; tests | 3–4 days |
| 5 | docs: `SAFETY.md` §1/§2/§4, `FINDINGS.md` "retired by design", the agent contract (H6's clause removed), the fleets doc's legibility note, the delete/rename design's §2, CHANGELOG | 2 days |
| 6 | live drill on the three stores; e2e legs re-scripted | 1 week |

Four to six weeks. The model arm (tranche 2) is the go/no-go: if the known-
bad worlds do not hold under `ImmutableObjects`, the retirement claim is
false and the rest is not built.

**Status 2026-09-19 (evening).** Tranches 1, 3 and 4 are BUILT, uncommitted,
in the one realisation (fresh keys, §2.1):

- `flint-store`: `ObjectStore::delete_many` with `DeleteManyReport` (the
  memory double counts one request per thousand keys; S3 uses
  `DeleteObjects` and falls back to per-key DELETEs on a dialect without it).
- syncer: `LeanConfig::{handle_key, files_prefix, handle_parts}`,
  `ui_flush`/`ui_flush_added` (the gateway's flush carries the order R7
  reads); `InboxEntry.key` and `cited` as the citation KEY the writer saw;
  `BaselineEntry.key`, `ForeignChange.key`; the upload lands at a fresh
  handle with no condition (the 412/404/park/adopt-own arms are gone);
  repairs cite the baseline's handle with no HEAD, voided when the document
  cites that handle at another path this install keeps (R2); the merge
  returns what mine publishes over; the commit computes the retired set
  (theirs-at-CAS minus installed minus cited-elsewhere minus named by an
  entry that outlives this barrier), surfaces the overridden versions it
  never integrated by a server-side copy and a `commit-surfaced-foreign`
  record (R7), deletes the retired set in one batch, and runs the orphan
  sweep inside the commit section when due (R4); the consume judges a
  gateway entry by `judge_ui_entry` (live / superseded / stale, the stale
  one preserved by a copy and a `consume-stale-ui` record) and the tombstone
  pass by the document and the cell with no HEAD; the ingress sweep copies a
  bare-path object to a handle named after its etag and appends the entry
  (R5); the checkout re-resolves the pointer when a handle it was fetching
  is gone and the pointer moved (bounded, `CheckoutReport.reresolved`); the
  gateway's precondition is judged again at the cell's CAS
  (`inbox::Expect`, `LeanError::Lost`), and `hitl_may_overwrite` is gone.
- gateway: `put_file` (no HEAD, a fresh handle, the citation it saw carried
  in the entry), `get_file` by the tracked handle (the cell's entry first,
  the citation after; one re-resolve on a gone handle), rename as a citation
  move (no copy), drafts judged against the tracked version and promoted to
  a fresh handle.
- tests: the syncer battery is 244 green and the gateway's 56 (§3.1 names
  what was retired and what stands in its place).

What the models found in the code after the tranche (L-117; the R2 bullet
above has the traces): a removal one writer refused was final for every
writer, and a rename left the source's pending entry beside the
destination's — one handle cited under two names either way, and a refused
rename's destination unpublished while the source lingered. The seventh box
run of the rename world found the lag (`Inv_HITLTracked` at depth 13); the
core model, `lean/formal/LeanCore.tla`, found the two rules in its first
five minutes, before the history model had reached them. Three tests written
from the traces fail on the shipped code. The core model is the user's
step 1 of the evening's plan: the shipped shape only, state-based
invariants, a mutation world per rule, and a TLC-checked refinement from
this module's `IMPL` + `ImmutableObjects` worlds. The core's own fifteen
worlds — the shipped shape, ten mutations, four reachability probes — are in
the gate since 2026-09-20, as is the refinement's queue and probe world
(`lean/formal/LeanRefine.tla`; the rename world runs on the box first).
`lean/PROTOCOL.md` is written from it (step 3), and `SAFETY.md` §3.1 is the
H8/H9 table (step 2): what is certified on the shipped shape, what the core
states, what nothing checks; three gated-lane invariants retired and the
no-resurrection claim restated as an action property.

What the code found that the model had not (each now a rule): a handle named
by an entry THIS barrier consumed spares nothing at its own collector (the
entry leaves the cell at the window clear; the model's `GCCollect` now says
the same — tightened with the seventh run's fix, in the eighth); the read door
reads the cell on every read (one small GET) and hides a path whose removal
is pending, as the listing does; a UI write consumed-dirty over a locally
deleted path by one writer and synced by another survives the delete
through the second writer's re-cite — a delete that raced an acked write is
modify-wins, as the consume rule says, where the slot's collector had
happened to take the UI's bytes with the seed's key. Tranche 2 (the arm) is
in the gate and on the box; tranche 5 is partly done (SAFETY.md §3/§5,
CHANGELOG, FINDINGS); tranche 6 (the live drill) is not started.

## 10. Recommendation

Build it, in that order, after the current review's gate is green and its
work is committed. Until then, document H6 (option 1 of the earlier
discussion) so the contract stops claiming a coherent point it cannot
deliver, and do not build option 2: it adds a PUT and a completeness
protocol to shrink a window that handles remove.

Decision taken 2026-09-19 by the owner: fetch-by-bare-path is relaxed
EVERYWHERE (§2.1, §6.1), one realisation is built, and the model arm
(tranche 2) goes first. The arm is built and its runs are in the gate
(§8.1). Second decision, the same evening: go ahead with the code, the store
type and the syncer commit section first — built (§9, status), with the
gateway's verbs, uncommitted. After it: a shipped-shape-only core model of
the handles protocol (`LeanCore.tla`: handles, the pointer CAS, the cell, the
inbox, the merge; state-based invariants; a TLC-checked refinement from
`LeanSubtree` under IMPL and `ImmutableObjects`; a mutation world per
invariant), the H8/H9 cleanup restated as the core's claims, and a
`PROTOCOL.md` from the core — in that order, once the arm's box run is green
on the final module.
