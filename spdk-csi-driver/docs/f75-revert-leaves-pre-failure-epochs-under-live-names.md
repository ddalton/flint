# F75 — the catch-up revert leaves a stale replica's pre-failure epochs under live names

Status: **FOUND 2026-10-08 by code review** (while checking, for F74,
whether old epoch snapshots are reaped). **Not observed live. NOT FIXED.**
Pinned by `catchup.rs`
`revert_reaps_the_stale_replicas_epochs_newer_than_its_base` (ignored
until the fix lands; run with `--ignored`). Interacts with F74: the raised
epoch interval makes the common form of this (§3a) the normal case.

## 1. The mechanism

When a stale replica returns, the §5 catch-up
(`docs/incremental-replica-rebuild.md`) picks a base epoch `E_b` cut at
least `T_back` (120 s) before the failure, reverts the replica's head to
its own copy of `E_b` — delete the head, re-clone it from `E_b`
(`catchup.rs` `revert_head`) — and copies the source's lineage from `E_b`
inclusive onto it, snapshotting the destination at the target epoch
(`align_head`).

The replica's **own epoch snapshots newer than `E_b`** — the ones it cut
as an in-sync member between `E_b` and its failure — are not touched by
the revert. They stay on the lvstore under their names, which are the
record's live epoch names. Three things then trust names:

- `align_head` (`catchup.rs` ~1663) issues `bdev_lvol_snapshot` under the
  target epoch's name and treats SPDK's "File exists" as convergence
  ("a resume after a crash between align and record write: same head,
  same content"). That is true for a resume. It is false for a leftover:
  the name now denotes the pre-failure snapshot, and no snapshot of the
  repaired head is cut.
- `select_base_epoch` (`catchup.rs` ~777) tests "present on the returning
  replica" by name.
- The epoch GC (`epoch_scheduler.rs` `gc_candidates`) reaps only names
  **older than the oldest retained** epoch. Leftovers newer than `E_b` are
  retained epochs; they are reaped only when they roll out of the record,
  `K` intervals later.

`revert_head_to_empty`'s comment ("the old chain's snapshots are NOT ours
to reap") is about the §9-5 full build, where there is no shared history
and the old chain's epochs are all below the oldest retained — the GC's.
It does not cover the §5 revert, where the leftovers are retained names.

## 2. Why the record cannot tell

The sync record tracks epochs per volume and the head's `reverted_to`
marker per replica. It has no notion of which of a replica's epoch
snapshots were cut by the scheduler while the replica was in sync versus
aligned by a catch-up afterwards. By name they are the same epoch.

## 3. Consequences

**a. The common form (benign for the head, wrong for the snapshot).** The
outage was shorter than the epoch interval and no survivor epoch has been
cut since, so the target epoch of the catch-up is one the replica already
holds. The copy lands correctly on the head (it is base-inclusive from the
source's blob of the same name), but `align_head` is refused the name and
reports converged. The replica's "epoch N" is the pre-failure snapshot.
The record says the standby is consistent at N. With F74's raised
interval, most outages are shorter than it: this becomes the normal case.

**b. Space.** Leftovers pin the clusters they own until they roll out,
`K` intervals later. Bounded; minor.

**c. The corner that loses data.** A second failure of the same replica
within `T_back` of the first post-admission cut selects, by presence and
timestamp, a leftover as the base: it is recorded, old enough, and present
by name. The revert then clones the head from it — from pre-failure-1
content, which may include a write the first revert deliberately discarded
(a write that completed on this leg only and was never acknowledged, or a
zombie loopback write, §5 "the reverse direction"). The source's lineage
copy from that epoch inclusive never touches a cluster the source has not
written since, so the resurrected write survives admission. After
admission the leg is a read source: the array returns different data
depending on which leg serves the read, silently. Requires: outage 1 with
no epoch cut during it, the leftover cut within `T_back` of failure 1, and
failure 2 within `T_back` of the admission cut — a flapping node is the
shape.

## 4. Severity

Correctness, narrow, not demonstrated live. (a) is reachable on every
short outage; (c) needs the flap. Both are closed by the same fix.

## 5. The fix

In `run_catchup_for_volume`'s revert branch (the non-resume path, after
`revert_head` returns and **before** `store.record_revert`): list the
destination's lvols, and delete every epoch-named snapshot of this volume
whose sequence is **greater than the base's**, newest first (leaf first;
SPDK merges a one-clone snapshot into its clone anyway, `blobstore.c`
`bs_is_blob_deletable`), tolerating "missing". Never the base (it is the
clone parent) and never anything older (the GC's).

Why before the record write: `record_revert` sets `reverted_to`, and a
resume with that marker standing skips the revert. A crash between the
record write and the reap would resume without a revert and never reap.
With the reap inside the revert's idempotent window, a crash anywhere
re-runs it: the head delete, the clone and the reap all tolerate "already
done".

Why not "make `align_head` refuse 'already exists'" instead: the leftover
would never go away and the catch-up would fail forever. Refusing is a
reasonable belt *after* the reap — with leftovers gone, "already exists"
can only mean a resume — but it is not the fix.

**Same hazard class, out of scope here:** user snapshot copies
(`snap_<vol>_<u64>`) cut between `E_b` and the failure survive the revert
the same way, and the §11 lineage replay re-snapshots the destination at
each user snapshot by name ("so healed replicas re-acquire bit-identical
copies") — a leftover under that name is accepted the same way. Check
whether a leftover user-snapshot copy can differ from the source's (the
same `T_back` window applies) before extending the reap to them.

## 6. The test

`catchup.rs` `revert_reaps_the_stale_replicas_epochs_newer_than_its_base`:
replica b failed at 10:20 holding epochs 3, 4 and 5 (5 cut at 10:19,
inside `T_back`, so the base is 4); no survivor epoch since. It asserts
that the catch-up deletes `epoch-vol1-5` on node b, that this precedes the
alignment cut of `epoch-vol1-5`, that the alignment cut actually happened
(the fake now refuses a duplicate name as SPDK does — without that
property the double would pass the swallowed "File exists" as a cut, a
double missing a property passes for the wrong reason), and that epochs 3
and 4 are not deleted. Today it fails at the first assertion: no delete.

The fake's new property (`FakeRpc::deleted` / `created`): one name per
lvstore, `bdev_lvol_delete` removes it from the listing, a clone, create
or snapshot brings it back, and a snapshot under a standing name is
refused with "File exists".

## 7. The formal models

Unchanged by the fix, and neither proves nor refutes the defect.
`formal/FlintSnapshots.tla` (the epoch chain at block-content level) keeps
ONE `tgtBase` per target and takes the base's content from the shared
chain: "the target sits on a RETAINED epoch (its content IS that epoch's
content — a based copy clones from the shared base snapshot)". That is
the premise this defect breaks — the replica holds a snapshot *named*
`X` whose content is not chain `X`'s — and the model has no state in
which to say so, so `Inv_SessionFaithful` holds vacuously over it.
`formal/FlintReplication.tla` models content as a write-set and an epoch
cut as a single event; it has no per-replica snapshot content at all.

To make TLC catch this class, in the style of the module's three existing
mutations: give the target its own snapshot map (`tgtSnaps`, epoch id →
content, its lvstore), start `CopyBased` from `tgtSnaps[tgtBase]` rather
than `chain[j].content`, add a `Revert` that under a `ReapNewer = FALSE`
mutation leaves entries newer than the base in `tgtSnaps`, and an align
step that under the same mutation does not overwrite a standing id. A
second session based on a leftover then violates `Inv_SessionFaithful`.
Worth doing with the fix, not before it.
