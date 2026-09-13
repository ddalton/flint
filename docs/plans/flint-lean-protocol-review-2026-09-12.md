# flint-lean protocol review — 2026-09-12, HEAD `33abb284`

The question was "is the design solid — the protocol?", asked the day
the agent contract (`lean/syncer/AGENTS.md`, written into every mount as
`.flint/AGENTS.md`, `33abb284`) was about to ship in v1.51.0. Every
sentence in that file is a promise to an AI agent that has no other API,
so the review's rubric was: a concrete sequence of events that ends in
(a) data loss or a torn/stale tree presented as coherent, (b) an ack an
agent will match by the documented rules that does not mean what the
document says, (c) an agent that follows the document waiting forever,
or (d) a documented rule the code contradicts. No style findings.

Five adversarial reviewers, one per area — the ack protocol; boundary
atomicity + change detection + delete; lease/fencing/succession; foreign
changes (inbox, consume, sync); gated mode + drafts + the marker — each
required to report `file:line`, a numbered scenario, and a HELD list
naming the line that enforces every invariant it checked, and to
adjudicate the two prior audits (`flint-lean-integrity-audit-2026-09-03.md`,
`flint-lean-boundary-verbs-review-2026-08-25.md`) at HEAD rather than
re-discover them. Their full reports are in the session; this document is
the condensed record: the verdict, every finding with its disposition,
the reproduction-fix-test plan, and the live test.

## Verdict

**The commit point holds; the edges around it do not, and the contract
over-promises in nine places.** The manifest CAS is still the atomic
commit; no reviewer found a way to install a manifest naming bytes that
were not uploaded, to run two writers against one prefix, to overwrite a
locally-modified file through the consume's *steady-state* path, or to
publish anything under `.flint/`. Every prior HIGH from 2026-09-03 is
fixed at HEAD except #6 (legacy-manifest migration fence, mixed-version
rollout only) and #7 (lost takeover-acquire skips rotation).

What was found, in the shipped code, is one CRITICAL, seven HIGH and
nine MEDIUM defects, all of them reachable by an agent doing ordinary
work or by a pod being replaced at an ordinary moment — the two things a
fleet does all day. They cluster into four mechanisms:

1. **A refusal without its callers.** The scope validation added on
   2026-09-03 ("an error must not return a legal value") returns an
   error the sentinel loop treats as transient: no ack, pending kept,
   and the error is returned *before* the publish honor and *before* the
   cadence barrier. One `{"scope":[]}` stops every boundary for the life
   of the workspace (ack-1). The same loop shape strands an agent behind
   any deterministic honor error (a corrupt object, a 412 whose object
   is gone: inbox-5).
2. **Sizes and seconds.** The manifest cites the *scanned* size but
   uploads the *current* bytes, so a file that grows during a cadence
   tick — a checkpoint being streamed — is cited with a length the object
   does not have, and every fresh checkout of that entry (> 16 MiB) fails
   its CRC fold: the successor pod can never start (atomicity-1,
   CRITICAL). Change detection compares mtime at whole seconds, so a
   same-size rewrite inside the scan's second is invisible forever — no
   timestamp preservation by the agent needed, an editor saving twice
   does it (atomicity-4).
3. **Parked means abandoned.** A 412 against a foreign version "parks"
   the path; nothing ever un-parks it, every later ack says `ok` with
   `parked: n`, and the drain attests success and lets the node remove
   the tree with the agent's only copy (inbox-1). A crashed compose of a
   > 64 MiB file parks the same way because the compose 412 recognizer
   ignores the crash journal the whole-object path consults (atomicity-2).
4. **Windows between a check and a write.** The consume stats a path
   clean, fetches the foreign bytes over the network, and renames them
   over the path without looking again: an agent write in that window is
   overwritten with no record (atomicity-3 / inbox-2). A file swapped for
   a symlink between scan and upload publishes the link's target — a
   tenant can publish `/proc/self/environ` of the syncer's own process
   (atomicity-6). Gated mode has two of its own: a consume-dirty entry is
   never dropped from the inbox after the citation that supersedes it and
   is consumed a second time over the agent's published edit (gated-1);
   a withheld tombstone survives the file's same-stat return and deletes
   its object (gated-2).

Everything else is a MEDIUM lease edge (a fence whose local settle fails
swallows the fence; a lost-renew adoption that writes nothing; a lost
acquire response that skips the takeover rotation; a raw touch during a
restarted claimant's wait) or a sentence in the contract the code does
not keep.

## Findings and dispositions

Severity is the reviewer's; confidence is CONFIRMED unless marked. "Fix"
means fixed in this wave with the named reproduction test (each written
to fail at `33abb284` first); "Doc" means the contract sentence was wrong
and is corrected; "Defer" names why and where it is recorded.

| id | sev | one line | disposition |
|---|---|---|---|
| atomicity-1 | CRITICAL | manifest cites the scanned size, uploads the grown file; fresh checkout of the entry fails its CRC fold forever | Fix: `the_manifest_cites_the_uploaded_length_not_the_scanned_one`; live-tested (§Live) |
| ack-1 / inbox-3 | HIGH | an invalid `sync` scope is a permanent unacked error returned before the publish honor and the cadence barrier | Fix: `an_invalid_sync_scope_is_acked_refused_and_never_wedges_publish`, `a_refused_sync_pending_does_not_stop_the_cadence_barrier` — new ack status `refused-scope`; the ticks continue past a non-fence honor error |
| atomicity-2 | HIGH | compose 412 recognizer ignores `prior_uuids`; a crash mid-compose + edit parks forever | Fix: `a_crashed_compose_is_adopted_after_an_edit_not_parked_forever` |
| atomicity-3 / inbox-2 | HIGH | consume overwrites an agent write landing between the dirty stat and the rename | Fix: `a_consume_never_overwrites_a_write_that_landed_during_its_fetch` — re-stat after the fetch, dirty ⇒ the consume-dirty path |
| atomicity-4 | HIGH | same-size rewrite in the scan's second is invisible | Fix: `a_same_size_rewrite_within_the_scan_second_is_still_published` — nanosecond mtime, compat with second-granularity baselines |
| inbox-1 | HIGH | a parked path never un-parks; acks say `ok`; the drain attests and the tree is removed | Fix: `a_parked_path_is_preserved_and_published_over_not_abandoned`, `a_drain_never_attests_a_boundary_with_parked_paths` — a park preserves the foreign version and publishes over it (the consume-dirty rule at upload time); a boundary with parked paths acks `partial` with `report.dropped`; the drain does not attest it |
| atomicity-6 | HIGH (security) | a symlink swapped in after the scan publishes its target, `/proc/self/environ` included | Fix: `the_upload_refuses_a_symlink_swapped_in_after_the_scan` — `O_NOFOLLOW` + regular-file check on every upload read |
| gated-1 | HIGH | a consume-dirty inbox entry is never dropped after the citation that supersedes it; consumed again over the agent's published edit | Fix: `a_consume_dirty_entry_leaves_the_cell_at_the_citation_that_supersedes_it` |
| gated-2 | HIGH | a withheld tombstone survives a same-stat return; the citation drops the file and GC deletes its object | Fix: `a_same_stat_recreated_file_survives_its_withheld_tombstone` |
| ack-2 / gated-4 | MEDIUM | `sentinel-deferred` is stamped on any floor-tick honour, the doc says budget only | Doc |
| inbox-5 | MEDIUM (plausible) | a 412 whose object is gone fails every barrier forever | Fix: `a_412_on_a_vanished_object_recreates_instead_of_failing_forever` |
| inbox-7 | MEDIUM | an unscoped `sync` coalesced with a scoped one is honoured as the scoped one | Fix: `an_unscoped_sync_touch_widens_a_coalesced_scope_to_the_whole_tree` |
| inbox-8 | MEDIUM | containment refuses `.flint/` but not `.flint-sync/`; a planted citation is materialised into the state dir | Fix: `containment_refuses_the_state_directory_too` |
| gated-3 / lease-6 | MEDIUM | a restarted claimant waiting behind a live holder leaves the marker `live`; a raw touch is never consumed or refused | Fix: `a_waiting_claimant_refuses_a_raw_touch_and_flips_the_marker` — consume before refusing, on every Waiting poll |
| lease-1 | MEDIUM | a fence whose settle fails (ENOSPC on `.flint/`) swallows `Fenced`: an immortal leaseless syncer behind a `live` marker | Fix: `a_fence_whose_settle_fails_is_still_a_fence` — the settle's error is logged, the fence is returned |
| lease-2 | MEDIUM (plausible) | adopting a lost-renew token writes nothing; the token stands still for a takeover threshold | Fix: `adopting_a_lost_renew_token_moves_the_cell` |
| lease-3 / audit #7 | MEDIUM | self-recognition by `holder_id` alone skips the takeover rotation after a lost acquire response | Fix: `a_lost_acquire_response_still_rotates` — self-recognition requires the epoch to match |
| atomicity-5 / inbox-4 / audit barrier-7 | MEDIUM | a crash between the CAS and the window clear loses merge-preserved foreign entries | Defer: needs a second manifest writer (the gateway's CAS door or an escaped straggler); the fix is a journal of `foreign_entries` in `intent.json` — recorded in `docs/plans/flint-lean-boundary-verbs-plan.md` follow-ups |
| ack-3 / atomicity-8 / inbox-6 | LOW | publish acks never carry `report.conflicts` | Fix: `a_publish_ack_carries_the_boundarys_conflict_records` |
| ack-4 | LOW | `boundary: "drain"` is emitted and undocumented | Doc |
| ack-5 / gated-5 | LOW | `cadence` mode is documented "(no verbs)"; the code honours them | Doc — the verbs work in every mode; the mode names the lane |
| ack-6 | LOW | `conflicts.jsonl` rotates at 1 MiB, not "every conflict ever" | Doc |
| ack-7 | LOW | `conflicts_since` under-reports when the log rotates mid-sync | Defer: arithmetic edge, LOW; noted in `sentinel.rs` |
| ack-8 | LOW | empty vectors are omitted from acks; the doc's example shows `[]` | Doc |
| ack-9 | LOW (plausible) | an agent timestamping its touch with `date` can never match its solitary ack by the mtime rule on a coarse clock | Doc — take the time from `stat` of the temp file, or use the nonce |
| ack-10 / U37 | LOW | a touch consumed into the staging file but not folded is clobbered by the next consume after a transient `save_pending` failure | Defer: in-code admission at `sentinel.rs`; needs ENOSPC injection |
| ack-11 | LOW | the doc does not require a fresh nonce per touch | Doc |
| atomicity-7 | LOW | a crash-orphaned `*.flint-sync-tmp` is published | Fix: `an_orphaned_consume_temp_is_never_published` |
| atomicity-9 / csi-6 | LOW | the operator's stale-MPU sweep aborts uploads whose `Initiated` the store omits | Fix: `unwrap_or(false)` in `reconcile.rs` |
| gated-6 | LOW | gated applies the inbox at every floor tick, not "only at a boundary" | Doc |
| gated-7 | LOW | a fenced marker carries no `reason` | Fix: `a_fenced_marker_says_why` |
| gated-8 | LOW | gated parks are `stage-412-parked`, the doc says `upload-412-parked` | Doc |
| inbox-9 | LOW | rescope-widen has no dirty check (only with the syncer stopped) | Defer: unreachable while the syncer holds the flock |
| inbox-10 | LOW | the 65th scope entry is dropped silently | Fix: folded into `refused-scope` |
| lease-4 / arbitration-5 | LOW (plausible) | a node clock behind the store's suppresses in-barrier renewal | Defer: judge freshness by a local monotonic clock — follow-up |
| lease-5 / audit #6 | LOW | legacy single-object manifest is poisoned after the pointer install | Defer: mixed-version rollout only; pre-2026-09-04 binaries |

## The plan, as executed

1. **Reproduce.** Every "Fix" row has a test named above, written first
   against `33abb284` and run: the failing run is the reproduction
   (results in §Results).
2. **Fix.** One commit per mechanism (the four clusters above plus the
   lease edges), each carrying its tests and the AGENTS.md sentences it
   changes, so the contract and the code move together.
3. **Test.** The syncer's lib suite in full; the fixed tests
   mutation-checked (fix reverted by string ⇒ test fails).
4. **Live.** The CRITICAL is the one finding whose consequence lives in
   a *successor pod*: on runcu (i4i.large, the deployed door — chart +
   `s3.csi.chert.us` + worker pod + tenant pod) stream a 1 GiB file into
   a lean workspace while a cadence tick fires, delete the tenant pod
   so the workspace is re-served, and watch the successor's checkout. At
   `33abb284` it fails the CRC fold and the worker exits 1 in a loop;
   with the fix it checks out and the file's cited length is the
   object's. The control arm is the same leg with the file finished
   before the tick.
5. **Release.** v1.51.0 only after 1–4.

## Results

**Reproduction.** Twenty-one tests were written against `33abb284`
(with the additive `Ack.reason` field only, so they compile) and run:
**twenty failed, each at the assertion its finding names** — no ack
written; the floor tick erroring; the scope narrowed; the conflicts
missing; the rewrite invisible; the vanished object failing the barrier;
`.flint-sync/x` accepted; the temp published; no `reason`; the cell token
unmoved; the manifest citing 4 MiB over a 6 MiB object; the crashed
compose parked; the agent's write overwritten by the consume; the link's
target published; the superseded inbox entry still in the cell; the
boundary omitting a file on disk; the raw touch unconsumed; the fence
replaced by an I/O error; the path parked forever; the parked boundary
acked `ok`. The twenty-first (`a_lost_acquire_response_still_rotates`)
passed only because its fix had landed with the first batch; it was
mutation-checked instead: with `state.epoch == inc.epoch` removed from
self-recognition the test fails (`0 passed; 1 failed`), restored it
passes.

**Fix + test.** Full syncer lib suite with every fix and the corrected
contract: **219 passed, 0 failed** (208 before the wave). `flint-store`:
27 passed. `lean_operator`: see the commit. Three existing tests were
adjusted, each with the reason in its comment: one pinned the old park
(`foreign_412_parks_never_overwrites` — its real invariant, a foreign
write is never lost and the conflict is surfaced, now holds with the
bytes preserved, and the test says so under a new name); one wrote a
`live` marker into a *fresh* tree before waiting, the restarted-case
fixture under the fresh-case name; one asserted the adopted lost-renew
token equals the landed one, which the adoption's own renew now moves
past.

**A fourth thing the deployed door found while this ran.** The lean
worker for the 6 × 1 GiB workload was **OOMKilled** at its checkout,
three times over: the plugin-wide worker limit is 1Gi
(`node.workers.resources`, for mount-s3 and the syncer alike) and the
syncer's default read window was 512 MiB in flight at fan-out 32. Peak
RSS of the host checkout, measured by `VmHWM` at 200 ms:

| `fetchInflightMb` | `big` peak RSS | `big` wall | `mixed` peak | `mixed` wall | `small` peak |
|---|---|---|---|---|---|
| 512 (shipped) | 1105 MiB (n=1) | 27.1 s (24.1–25.0 s over the drill's 3 reps) | 298 MiB | 19.3 s | — |
| 256 | 1005 MiB (n=1) | 24.3 s | — | — | — |
| **128** | **403–437 MiB (n=3)** | **24.9–25.7 s (n=3)** | **229 MiB** | **18.8 s** | **72 MiB** |

The window past 128 MiB buys nothing on this NIC (the fetch is bound at
~350 MiB/s either way) and costs the difference between fitting the
worker's limit and not. The default moves to 128 MiB in the CRD, the
binary and the config; the host arms of the door drill never saw this
because they ran without a cgroup.

**Live.** Run on runcu's control-plane node (i4i.large, real S3,
us-west-1) against the drill bucket, `live-size.sh` in
`lean/e2e/perf/`: a syncer daemon (`flint-sync run`, floor 10 s) serving
an empty prefix; a writer streaming a 1 GiB checkpoint into the tree at
~30 MiB/s; the moment the first barrier carrying `ckpt.bin` is logged,
the daemon is SIGKILLed (a spot reclaim — no drain) and the writer
stopped; then a successor checks the boundary out into an empty tree
with the same binary. Three arms, run once each in sequence:

| arm | binary | file on disk at the kill | successor's checkout |
|---|---|---|---|
| old, mid-write | `33abb284` (sha `6adb6926…`) | 400,556,032 B | **FAILED in 1 s**: `manifest cites …/files/ckpt.bin at etag "…-5" with CRC-64 03ZM7x7zbtQ=, but the 16 ranges fetched under that etag fold to …` — the wedge |
| new, mid-write | `4fc8bee6` (sha `3d8dd041…`) | 401,604,608 B | **OK in 1 s**: `ckpt.bin` is 271,581,184 B on the successor, equal to the object — the boundary carries what was durable when the upload read the file |
| new, finished (control) | `4fc8bee6` | 1,073,741,824 B | OK in 6 s, 1,073,741,824 B — the fix is not what makes checkouts pass |

The old arm's cited size was the scan's; the object was longer; every
ranged checkout of that entry folds a CRC that cannot match. The new
arm cites the length it uploaded.
