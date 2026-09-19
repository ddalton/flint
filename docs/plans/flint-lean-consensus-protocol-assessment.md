# flint-lean — which consensus protocol is lean? An assessment

**Date:** 2026-09-13. **Status:** assessment, no code. **REVISED the
same day** after review by the session holding the per-barrier-lease
working tree: §1.1 corrected (the lease IS load-bearing in the adopt
arm), §4.1 WITHDRAWN (it reopens that race; counterexample in §4.1.1),
§4.5 added (convergence — a defect the names predict), the "as built"
rows marked HEAD vs working tree. No change is recommended for code
now; two shapes are recorded for the day the constraints move (§4.2,
§4.3), and one rule is recommended for the convergence defect (§4.5).

**UPDATED again the same day** by that session, once the defects this
review names had been reproduced and fixed in its working tree: §4.6
records each one — failing repro test, fix, mutation check, model run
— for the GC delete, the adopt and repair arms, the sync verb, and the
three convergence defects of §4.5. Row 3's two-writer consequence is
CORRECTED (the commit-section re-read does NOT close it; it is left
open as a residual). All of it is uncommitted as this is written.

**Read with:** `flint-lean-writer-lease-and-gated-assessment.md` (the
per-barrier lease, built the same day; §4 is the loop this document
names, §10 is what was actually built),
`flint-lean-manifest-pointer-design.md` (the pointer layout),
`flint-lean-protocol-review-2026-09-12.md`. The model is `lean/formal/`;
the configs named below exist unless marked "to add".

## 0. The question, verbatim

User, 2026-09-13: "Lean permits a distributed agent model where writes
are permitted concurrently, but still having a single writer model. Can
you suggest if there is a well known distributed consensus protocol
that lean can adopt that matches lean's design? or suggest something
better if you think lean should use? and write the solution in a doc
that can be consumed by a different session in the future."

### 0.1 The answer, short

1. **Lean does not need a consensus protocol and should not adopt
   one.** Its writers never talk to each other; their only shared
   medium is the object store, and the store already offers a
   linearizable compare-and-swap on one key (`PUT` with `If-Match`).
   A CAS register has infinite consensus number (Herlihy 1991): any
   number of writers reach agreement through one conditional PUT,
   wait-free, with no messages between them. S3 and Ozone run the
   consensus for us. Paxos or Raft among syncers would add a
   membership problem (pods churn, and may sit in different clusters),
   a quorum that two writers cannot form, a network path that does not
   exist today — and its decision would still have to be written to
   the bucket, which is a CAS.

2. **What lean has built IS a well-known protocol**, under three names
   the code does not use. Naming it is the deliverable, because the
   names carry the sharper rules (§1, §2):
   - The **commit** is Apache Iceberg's commit: immutable metadata
     (generations, chunks) behind ONE mutable pointer, swapped by CAS,
     re-read and rebased on conflict. git's `update-ref` with an
     expected old value is the same primitive.
   - The **data path** is optimistic concurrency through HTTP
     conditional requests (RFC 9110 §13, formerly RFC 7232 — the "lost
     update" rule): every PUT and DELETE carries `If-Match` on the etag
     the writer last saw; the store arbitrates; the loser preserves and
     retries rather than overwrite.
   - The **fairness and liveness layer** is a lease (Gray–Cheriton
     1989) implemented the way the Amazon DynamoDB lock client does it
     (a record version number a live holder keeps moving; a claimant
     deposes only after observing it unchanged for a lease duration;
     takeover by conditional write), guarded by a fencing token
     (Chubby's sequencer — the epoch, enforced by the CAS), made fair
     by a ticket queue (the FIFO `waiters` in the cell).

3. **Better?** The first draft recommended moving the GC deletes out
   of the lease (§4.1). That is **withdrawn**: the lease is
   load-bearing for one arm — an upload that ADOPTS bytes already at
   its key cites an etag it observed, and a peer's GC recognizes that
   same etag, so `DELETE If-Match` matches it (§1.1, §4.1.1). The one
   rule worth adopting from the named protocols is Iceberg's **commit
   only when there is something to commit** (§4.5): two idle writers
   were observed trading empty generations forever. Two shapes are
   recorded, not built: §4.2 a Delta-style append-only log (only if a
   target store lacks `If-Match`, or branching needs manifest history
   without bucket versioning); §4.3 immutable data objects (rejected —
   the browsable mirror IS the product — and recorded so nobody "fixes"
   the data path without knowing the price).

4. **Blockchain (user's follow-up, §7): irrelevant as a whole, one
   ingredient worth taking.** Its consensus solves distrust among
   peers; lean has a trusted arbiter (the store) and authenticated
   writers. Its full replication is exactly the "pull the whole chain"
   cost feared, and lean never pays it: every generation is a complete
   snapshot, so a reader reads the head and the current tree,
   independent of history length. The one ingredient is the
   hash-linked append-only record — git's commit graph, older than any
   blockchain — and §7.4 gives the shape that adds it BESIDE the
   pointer, at O(changed entries) per barrier, with retention as
   policy. The manifest becomes the ledger the user asked it to be.

### 0.2 Is the design good? (user, 2026-09-13)

**The shape: yes.** A linearised pointer over immutable snapshots,
conditional writes on the data path, a lease for fairness only where
contention needs bounding — it is the shape Iceberg, Delta and git each
converged on independently for exactly this topology (many writers, one
trusted store, no peer channel, crash faults). Nothing in §2 does the
job with fewer moving parts.

**The build at HEAD `ba4f53d9`: not yet.** Three things stand between
the shape and "good":

1. HEAD's GC delete is unconditional — the model's named mutation
   ships. The fix is uncommitted, and its probe has no recorded result
   on S3 or Ozone (§5 items 1, 4).
2. The adopt arm makes the lease load-bearing (§1.1). The verify-in-
   commit fix is built in the working tree, with a failing repro and a
   mutation check (§4.6), uncommitted; the safety
   argument is now narrower than the design's prose claims, and a
   future "optimisation" that shrinks the lease will break it unless
   the bound in §1.1 is read first.
3. Multi-writer does not converge as observed (§4.5): idle ping-pong
   and two foreign-integration losses. The lease design's "the protocol
   already tolerates two writers" (§3.2 there) was wrong. All three are
   now repro'd and fixed in the working tree (§4.6), uncommitted. Until
   that lands, the supersede residual of row 3 is decided, and
   `run-writers.sh` W1–W7 has run on a cluster, "concurrent writers" is
   a design, not a property.

**One structural fact to keep in view.** On S3 the etag IS the content
hash (MD5 for a single PUT; MD5-of-MD5s for multipart), so an etag
guard cannot tell two writers' identical bytes apart — rewriting an
adopted object refreshes its age, not its etag. Every same-etag hazard
(§1.1, §4.1.1, the supersede consequence in §1) descends from guarding
a MUTABLE, PATH-ADDRESSED object by a CONTENT-derived token. The three
escapes are: verify under the lease (built), a grace-period sweeper
(§4.1.1), or unique object names (§4.3). The mirror is worth its
price, but this is where the price is paid, and it is where the next
bug will be.

## 1. Lean's protocol as built, in the literature's vocabulary

Lean's writers are syncers (one per pod), HITL writers through the
gateway's inbox, and anything else that PUTs into the bucket. None of
them has a channel to another. That puts lean in the **shared-register
model**, not the message-passing model, and the whole literature of
message-passing consensus (§2, row 1) is answering a question lean
does not ask.

| Lean, as built | Name in the literature | Where |
|---|---|---|
| `<prefix>/.flint/lean/current` is the ONLY mutable metadata object; generations and chunks behind it are immutable; the install is `PUT If-Match` on the pointer's etag (`If-None-Match: *` on first write) | Iceberg's atomic metadata-pointer swap; git's `update-ref <ref> <new> <old>` | `lean/syncer/src/manifest.rs:90`; `barrier.rs:1080` |
| The merge starts from THEIRS, applies my upserts, applies my deletes only where theirs is unchanged since my base; four lost races ⇒ refuse the barrier | Optimistic concurrency control: read phase, validation, write phase (Kung–Robinson 1981); Iceberg's "retry with validation" | `manifest.rs:931-983`; `barrier.rs:1080-1129` |
| Every data PUT carries `If-Match` on the base etag; a 412 preserves the foreign version and supersedes knowingly, or parks on a second 412. **Two-writer consequence not named before — OPEN:** the "foreign version" may be a PEER'S UNCITED UPLOAD (its PUT landed, its CAS has not). The supersede overwrites it (its bytes survive as the preserved copy), and the peer's CAS then cites a generation the key no longer holds: readers of that path get a 412 until the superseding writer's own commit re-cites it, and for good if that writer never commits again. A re-read in the peer's commit section does NOT close it — the supersede is lease-free and can land after the re-read. No bytes are lost; the citation is wrong. Not fixed (§4.6) | RFC 9110 conditional requests; the lost-update rule of unreserved checkout (Nielsen–LaLiberte 1999) | `barrier.rs` step 4 (~line 915) |
| GC after the CAS: `HEAD`, then DELETE of the recognized etag; an unrecognized etag is never deleted. **At HEAD `ba4f53d9` the DELETE is UNCONDITIONAL** — what ships is the model's `LeanBarrierLeaseGCUnconditional` mutation. `DELETE If-Match` (`delete_if_match`) and `probe_conditional_delete` exist only in the 2026-09-13 working tree, uncommitted as this is written: there the GC HEADs, deletes `If-Match` the recognized etag, and on a 412 re-HEADs (gone ⇒ deleted; replaced ⇒ `gc-skip`), and `flint-sync probe-conditional` runs the DELETE leg after the PUT leg | same rule; Iceberg deletes only files no live snapshot cites | `barrier.rs` Step 6 (`store.delete` at HEAD); working tree: `crates/flint-store/src/lib.rs` `delete_if_match`, `probe.rs` |
| The epoch cell: holder, epoch, token; a heartbeat moves the token; six quiet observations ≥ 10 s apart ⇒ depose; a 150 s claim deadline | A lease (Gray–Cheriton 1989); the DynamoDB lock client's record-version-number protocol | `lean/syncer/src/lease.rs:62-76, 298` |
| The epoch is stamped on entries; `verify_not_deposed` runs before the CAS; a takeover rotates the pointer; "a 412 on the renew is not yet a deposal" | Fencing token (Chubby's sequencer; Kleppmann 2016); lost-response disambiguation | `lease.rs`; `barrier.rs` |
| `waiters` and `handoff` in the cell; `release` names `waiters[0]`; any waiter may depose a dead holder, FIFO resumes at the next handoff | Ticket lock (FIFO); Lamport's bakery is the ancestor; Delta Lake 4.0's "coordinated commits" is the same idea run as a service | `lease.rs:209-296, 495`; `crates/flint-store/src/lib.rs:982-999` |
| `<prefix>/.flint/lean/writers/<holder_id>` heartbeat object for the operator and the gateway | A liveness beacon — observation only, not part of the safety argument | operator `observedWriters`, gateway `status.writers` |

### 1.1 What the lease is for — CORRECTED

The first draft said "safety never rests on the lease". **That was a
"by construction" claim, and it is false in one arm.** It enumerated
three guards (pointer CAS, `If-Match` PUT, `If-Match` DELETE) and
three races (upload-vs-upload, delete-vs-upload, delete-vs-delete),
and missed the arm in which a writer cites an etag it did not
produce.

**The adopt arm.** An upload that finds its bytes already at the key
(a crashed predecessor's PUT, a peer's identical content, a 412 whose
HEAD shows the same CRC) ADOPTS them: it cites the etag it OBSERVED,
not one its own PUT returned. Identical content gives an identical
etag. A peer that has just uncited that path recognizes exactly that
etag in its baseline, so its `DELETE If-Match <etag>` MATCHES — the
conditional delete is satisfied by the very object the adopter is
about to cite. The pointer CAS cannot catch it: the adopter merges
from THEIRS, sees the path absent, re-adds it with the observed etag,
and the CAS succeeds. `Inv_NoDangling` is violated.

The fix that is race-free: **re-read the adopted citation inside the
commit section** (`VerifyAdoptedCitations = TRUE`;
`LeanBarrierLeaseAdoptVerified.cfg` holds, `LeanBarrierLeaseAdoptBlind.cfg`
— the same cfg with the flag off — violates). It is race-free ONLY
because GC runs under the lease: while the adopter holds the cell, no
peer is inside a commit section, so no peer's GC can run between the
verify and the CAS. The citation-repair arm has the same shape and the
same dependency: a path whose integrated bytes (the baseline) differ
from its citation is re-cited at the etag a lease-FREE `HEAD` observed,
and a peer's GC can collect that etag before the CAS just as it can an
adopted one. In the working tree both arms feed one re-read under the
lease (§4.6); the adopt arm has a failing repro and a mutation check,
the repair arm rides the same loop and has no repro of its own.

So the lease carries safety in exactly one place: **verify-then-CAS
must be exclusive with every GC DELETE in the workspace.** Everything
else in §1 holds — the pointer CAS still prevents a lost manifest,
`If-Match` on PUT still prevents a lost update, a false deposal still
costs one barrier and no write (the deposed holder's CAS fails on the
rotated pointer) — but the sentence "you may shrink the lease, never
the guards" has a bound: the lease may not shrink below
{verify, CAS, GC} in one writer, exclusive across writers. §4.1 broke
that bound and is withdrawn.

What the lease buys beyond that: bounded CAS contention (with W
syncers racing unfenced, "four lost races ⇒ refuse" is reachable);
FIFO fairness, the guarantee the user asked for on 2026-09-13; a known
epoch at which the HITL window opens (advisory — `LeanNoWindowHolds`).

## 2. The candidates, and the fit

| Protocol | The problem it solves | Does lean have that problem? | Verdict |
|---|---|---|---|
| Paxos, Multi-Paxos, Raft, Zab, Viewstamped Replication | Agreement among PEERS that have only unreliable messaging and no shared linearizable register | No. Lean has no peer channel and DOES have a linearizable CAS | **No.** It would add membership (pods churn; `N CLUSTERS, ONE HUB` puts writers in different clusters), a quorum two writers cannot form, a network path that does not exist, and its decision still has to be PUT to the bucket |
| Disk Paxos (Gafni–Lamport 2003); Active Disk Paxos (Chockler–Malkhi 2002) | Consensus with SHARED STORAGE ONLY and no messaging — designed for disks that offer plain read/write blocks, no CAS | Only if a target store lacked conditional PUT. With CAS, Disk Paxos collapses to one conditional PUT | **The fallback, not the protocol.** Ozone 2.2.1 enforces `If-None-Match: *` and `If-Match` (probed 2026-09-12, `lean/e2e/perf/results/ozone-probe-2026-09-12.md`); S3 since 2024-11 |
| Wait-free consensus on a CAS register (Herlihy 1991) | The theoretical basis: CAS has consensus number ∞ | Yes — this IS what the pointer CAS does | **Already adopted.** Cite it; do not reinvent it |
| Leases (Gray–Cheriton 1989); Chubby (Burrows 2006); DynamoDB lock client; Azure blob leases | A time-bounded exclusive hold with heartbeat, takeover of a dead holder, and a fencing token against the straggler | Yes, for fairness and bounded contention (§1.1) | **Already adopted**, by construction the DynamoDB lock client protocol on an S3 object. Alignment: count DEPOSALS in the writers drill; a no-fault leg must show zero |
| Delta Lake (Armbrust et al. 2020) | OCC on object storage: data files written without coordination; commit = put-if-absent of `_delta_log/N.json`; rebase on conflict | Same family; the commit log is append-only where lean's pointer is mutable | **Same family, different log shape.** §4.2 |
| Apache Iceberg | Immutable metadata files; commit = CAS the current-metadata pointer (in the catalog); retry with validation | Yes — `flint-lean-manifest-pointer-design.md` IS this shape | **The closest match.** Name it |
| Apache Hudi | Timeline + OCC + an external lock provider (ZooKeeper, DynamoDB) around the commit | Same family; lean's epoch cell is the lock provider, in-bucket | Same family |
| git | Content-addressed immutable objects + CAS'd refs (`update-ref`, atomic push) | Same for the ref; lean's data is path-addressed and mutable | Same for the commit; differs on data (§4.3) |
| Bayou; CRDTs; operational transformation | Concurrent writes that MERGE CONTENT | No. Lean does not merge content (same-file = last-boundary-wins + preserved copy + record; lease doc §4.4) | **No, by decision.** Revisit only if content merge becomes a goal |
| Shared log (Corfu, Delos) | Appenders CAS the log tail; the log is the state | Not lean's shape | No; the log variant is §4.2 |
| Blockchain — permissionless (Bitcoin, Ethereum) or permissioned (Fabric, Quorum) | Agreement among MUTUALLY DISTRUSTING peers with no trusted arbiter; tamper-evident history by hash chaining; full replication so every node can validate from genesis | No trust problem: the store arbitrates and credentials authenticate every writer (Byzantine writers with bucket credentials defeat any chain). Lean's generations are already immutable, content-addressed snapshots | **Out of scope as a protocol** (§7.2). The hash-linked record is worth adding beside the pointer (§7.4); the chain is never pulled (§7.3) |

## 3. Why no consensus protocol is added — so it is not re-derived

A consensus protocol exists to manufacture a linearizable register out
of unreliable peers. Lean's store hands it one. The pointer CAS is a
single-shot consensus per barrier; the chain of pointers is a
replicated log with the store as its only replica set; the merge is
the state machine. Every property Raft would give — a total order of
commits, one decision per slot, no lost decision — the pointer already
has, and has on Ozone as well as on S3, with zero new dependencies.

The one consensus protocol built for lean's topology, Disk Paxos, was
built for a store WITHOUT conditional writes. Its place in lean is a
rule, not code: **a store that fails `flint-sync probe-conditional`
must be refused, not accommodated by timing.** Note that this refusal
is NOT BUILT: the probe is an operator-run subcommand, and nothing in
the syncer refuses at runtime — a store that accepts the header and
ignores it runs today with every guard silently void. Disk Paxos would
be the design to reach for only
if such a store had to be supported anyway, and that decision has not
been asked for.

## 4. Something better?

### 4.1 Shrink the critical section to merge + CAS — WITHDRAWN

Kept as the record of a proposal and why it fails, so it is not
re-proposed. The motivation stands (a mass delete is the one long
stretch of the commit section); the soundness argument below covered
delete-vs-upload (fresh etag) and delete-vs-delete and **missed
delete-vs-adopt (same etag)**, §4.1.1.

**Today** (`barrier.rs:1064-1300`): claim → `verify_not_deposed` →
window open → merge + pointer CAS (Step 5) → **GC deletes** (Step 6:
`HEAD` + `DELETE If-Match` per removed path, `renew_if_due` and
`fence_on_cell` every 200) → baseline rewrite, intent clear, window
clear (Step 7) → release.

**Proposed:** claim → verify → window open → merge + CAS → window clear
→ release → **then** Step 6 and Step 7, outside the lease, guarded
exactly as now.

Why it is sound: Step 6's safety already rests on `DELETE If-Match` on
the recognized etag — the lease never protected it, because uploads
have been lease-free since §4 of the lease design and an upload can
already land between the HEAD and the DELETE (that is why
`delete_if_match` exists; `crates/flint-store/src/lib.rs:794-814`
says so). Step 7 is local state. The window was never load-bearing
(`LeanNoWindowHolds`). What moving Step 6 adds is delete-vs-delete
interleaving between two writers collecting the same path: both carry
`If-Match` on the same recognized etag; one gets 204, the other 412 →
HEAD → NotFound → both report it deleted. A HITL re-create of a path
after the window closes and before its GC lands carries a new etag ⇒
412 ⇒ `gc-skip` + conflict record, as today.

What it buys:
- The lease is held for two requests (a GET of the pointer and a CAS)
  plus an in-memory merge — milliseconds. A mid-commit death becomes
  rare, so the 60 s deposal almost never fires; the 150 s claim
  deadline stops being reachable by a legitimate holder.
- A mass delete — the one long stretch of the commit section, the
  reason `renew_if_due` sits inside a loop — no longer parks W−1
  writers for its duration. (Unmeasured; the W8 leg below measures it.)
- `renew_if_due`, `fence_on_cell` and the "every 200" cadence leave
  `barrier.rs`; the heartbeat inside a barrier goes back to being the
  writers-beacon only.

What must be true first (§5): the store's conditional DELETE is
ENFORCED. `probe_conditional_delete` (`crates/flint-store/src/probe.rs:220`)
exists and has **no recorded result on real S3 or on Ozone** — the
Ozone probe of 2026-09-12 predates the method. A store that accepts
the header and ignores it answers 204, indistinguishable in the
response; only the probe tells. Run and record on both before any
code; the default trait method REFUSES, so an unprobed backend fails
loud, but the S3 override sends the header and trusts the store.

#### 4.1.1 The counterexample, and the literature's answer

Trace (two writers A, B; path x cited at etag e; `ConditionalGC` ON):

1. B's barrier uncites x. B claims, merges, CASes a manifest without
   x, **releases**. B's GC of x is now OUTSIDE the lease and pending.
2. A's barrier has x in its tree with content whose etag is e (it
   adopted the object at x — a re-create with identical bytes, or a
   crashed predecessor's PUT). A claims. A's verify reads x and finds
   e present. Good.
3. B's GC runs `DELETE x If-Match e`. e matches. x is gone.
4. A merges from THEIRS (x absent), re-adds x@e, CASes. Success.
   The manifest cites an object that does not exist. `Inv_NoDangling`.

Under the lease as built, step 3 cannot interleave between A's verify
(step 2) and A's CAS (step 4), because B's GC runs inside B's commit
section and A holds the cell. That is the whole of the safety
argument for the adopt arm (§1.1), and §4.1 deleted it.

If the mass-delete latency ever matters, the named protocols' answer
is **Iceberg's orphan-file cleanup with a grace period**: uncite in
the commit, never delete there; a sweeper deletes an uncited object
only once it has been uncited for longer than any commit can be in
flight (the `claim` deadline bounds that: 150 s, `lease.rs:76`) AND
no live manifest cites it AT the delete. Lean already runs this shape
for chunks (`LeanChunkGC*.cfg`: "refs read AT the delete, a grace, the
grace outliving the publish, adoption REWRITING what it adopts").
Extending it to data objects needs its own model run **in the adopt
world** (`VerifyAdoptedCitations`, `MaxCrashes ≥ 1`, two writers),
because the chunk model's "adoption rewrites what it adopts" rule
(`manifest.rs:574-591`) is what refreshes the age the sweep reads, and
the data-object adopt arm does not rewrite today.

The rest of this subsection is the original proposal, unchanged, as
the record.

Gate in the model — a control moves ONE dimension:
- **to add:** a constant `GCUnderLease` in `LeanSubtree.tla`, TRUE in
  every existing cfg (no behaviour change to the 90+ configs).
- **to add:** `LeanBarrierLeaseGCOutside.cfg` = `GCUnderLease = FALSE,
  ConditionalGC = TRUE`, `MaxBarriers ≥ 3`, two writers: `Inv_NoDangling`
  must HOLD.
- **control:** the same cfg with `ConditionalGC = FALSE` must VIOLATE
  `Inv_NoDangling` (today's `LeanBarrierLeaseGCUnconditional.cfg`
  already does with `GCUnderLease = TRUE`; the new control shows the
  guard, not the lease, is what holds the invariant with GC outside).
- Mutation-check the implementation by making the DELETE unconditional
  on the new path and watching the falsifier unit test fail (a
  positive control through the load-bearing line).

Drill: `lean/e2e/run-writers.sh` gains **W8** — writer A removes
5,000 paths in one barrier while writer B publishes every floor;
report B's barrier latency with and without the change, the deposal
count (must be 0), and a fresh checkout's byte-equality to the union
of both trees. Cost: ~50 lines in `barrier.rs`, one model constant,
two cfgs, one drill leg.

### 4.2 A Delta-style append-only log in place of the pointer — RECORD, do not build

Shape: `<prefix>/.flint/lean/log/<seq>.json` created with
`If-None-Match: *`; the highest `seq` IS the manifest; an optional
`current` hint object, advisory only. Readers find the tip by LIST
(strongly consistent on S3 since 2020-12, and on Ozone) plus the hint.

Gains: (a) the commit needs only put-if-absent — the weakest
conditional primitive, present on S3, Ozone (the one form its gateway
jar says it supports), GCS, Azure and MinIO; (b) every manifest
generation is addressable by `seq` forever: pinned reads, the
`declared` pointer of the lease doc §2, audit, and the manifest half
of the branching design's versioning need come free; (c) no rotation,
no pointer torn-write class.

Costs: the idle tick becomes a LIST (or trusts the hint) instead of
one ~300-byte GET; log retention and compaction (Delta's checkpoint
problem); a rewrite of `manifest::load/install` and the pointer design
of record; and it buys history of the MANIFEST only — data bytes stay
mutable in place, so an old `seq`'s citations 412 once overwritten,
exactly as `pinned_reads` already handles.

Triggers that make this the right move: a target store that enforces
`If-None-Match: *` but not `If-Match` on PUT; or branching landing on a
store without bucket versioning and needing manifest history. Neither
holds today.

### 4.3 Immutable, uniquely-named data objects — RECORD, rejected

Every system in §2 that lean resembles (git, Delta, Iceberg, Hudi)
writes each data object ONCE under a unique name and contends only on
the pointer. That makes uploads idempotent and retry-safe, makes
conflicts between writers impossible on the data path, and lets a
reader of any manifest see exactly the bytes it cites. Lean writes in
place at `<prefix>/<path>` so the bucket is a browsable mirror of the
tree — `aws s3 ls` works, agents read the bucket directly, passthrough
readers exist. **The price of the mirror is the entire
412 → preserve → supersede → park rule, the conflict records, and the
torn-view class that gated mode tried to fix and was removed for.**
This is a deliberate trade, recorded here so it is not "fixed" by a
session that sees only the cost.

### 4.4 Things that look like improvements and are not

- **Elect one publisher per workspace** (a leader) — a leader is a
  life lease; it reintroduces the starvation the per-barrier lease
  removed on 2026-09-13.
- **An external lock service** (etcd, ZooKeeper, DynamoDB) for the
  cell — the S3 cell already runs the DynamoDB lock client protocol,
  and on-prem Ozone deployments have no DynamoDB.
- **Version vectors on the manifest** — the manifest is one
  linearizable register and the CAS totally orders it; there is no
  partial order to track.
- **Quorum writes to several buckets** — solves a durability problem
  S3 and Ozone already solve internally, and reopens the consensus
  problem lean does not have.

### 4.5 Convergence — a defect the names predict

Found 2026-09-13 in the per-barrier-lease working tree by unit tests
written while verifying the model tranche's findings — not by the
writers drill, which has never run — and recorded here because the
literature's rule names the first one exactly. All three are
reproduced and fixed there (§4.6):

- **Two idle writers traded empty generations forever**: `seq` 5 → 13
  in 8 idle barriers, each a claim and a CAS, with nothing changed in
  either tree. Lean committed whenever the POINTER had moved; Iceberg
  commits only when the WRITER has something to commit. A peer's
  install is not a reason to install.
- **A peer's deletes never reached the other tree**, and **a peer's
  consume dropped the other writer's queued edits.** Both are in the
  foreign-integration path (`report.foreign_queued` → next consume),
  which the lease design §3.2 described as already tolerating two
  writers. It did not. The cause was one design choice: a writer's
  merge queued the peer's changes it had to materialize as
  `merge-preserved` entries in the SHARED inbox, where the peer's own
  consume found its bytes there, called the entry integrated and
  dropped it; and a peer's DELETE had no carrier at all.

The rule adopted (D8), in the form built: **a barrier whose merge adds
nothing to theirs installs nothing** — theirs becomes its merge base as
it stands, and the peer's changes go to the writer's own queue. It
still claims once when it finds the pointer moved (the merge runs under
the lease, which §1.1 requires), so two idle writers settle after one
round instead of trading generations. A barrier that finds the pointer
where it left it claims nothing, as before. Falsifier built:
`two_idle_writers_do_not_trade_empty_generations` (seq 5 → 13 without
the rule, unchanged with it).

### 4.6 The defects, reproduced and fixed — rows 1–6 committed `b50c2faf` (local, unpushed), row 7 working tree, 2026-09-13

Every row: the test failed on its load-bearing assertion before the
fix and passes after; the mutation disables the fix by exact string,
the test fails again on the same assertion, and the file is restored
by string with its checksum verified. Syncer suite 180/180 with all of
it; `flint-store` 33 (46 with `s3`), gateway 45, forge 175.

| Defect | Repro test (`lean/syncer/src/tests.rs`) | Fix | Mutation | Model |
|---|---|---|---|---|
| GC `HEAD` then unconditional DELETE removes a peer's lease-free upload of the same path | `a_peer_upload_between_the_gc_head_and_its_delete_is_not_deleted` | `ObjectStore::delete_if_match` (default REFUSES), S3 `If-Match`, the memory double, the perf fake; the GC deletes `If-Match` the recognized etag; `probe_conditional_delete` + its negative control | CAUGHT: "the manifest cites B's edit, and A's GC deleted the object under it" | `LeanBarrierLeaseGCUnconditional` violates; strict runs use `ConditionalGC` |
| An ADOPTED upload (or a citation repair) cites an etag observed with no lease; the peer's GC collects it before the CAS | `an_adopted_upload_deleted_by_the_peer_before_the_claim_is_not_cited` | re-read observed citations inside the commit section; withhold what is gone (`adopt-withheld`, `partial`, path left dirty) | CAUGHT: "seq 4 cites x.txt … but the object is gone" | `LeanBarrierLeaseAdoptBlind` violates, `…AdoptVerified` holds |
| `sync` advances its merge base to the manifest for a path an older inbox entry hid | `a_sync_does_not_advance_its_base_past_a_change_the_inbox_hid` | step 5 keeps the base for overlay-hidden paths, whole-tree and scoped | CAUGHT: "B's sync advanced its base past A's delete and kept the file" | `LeanBarrierLeaseSyncOverlayStale` violates; `…SyncOverlayHolds` (`SyncKeepsHiddenBase`, the one constant moved) HOLDS, 1,493,045 states |
| A peer's consume drops the other writer's queued edit; that tree never converges | `a_peers_change_reaches_the_writer_whose_merge_queued_it_even_if_the_peer_consumes_first` | a writer-LOCAL foreign queue (`state::ForeignChange`), saved BEFORE the baseline; nothing `merge-preserved` in the shared inbox | CAUGHT: "A's change never reached B" | not modelled — convergence is liveness; the model's inbox still carries `foreignQ` shared |
| A peer's DELETE never reaches the other tree | `a_peers_delete_reaches_the_other_writers_tree` | the merge records foreign deletions as tombstones in the same queue; consume removes a CLEAN copy, keeps a dirty one with a record | CAUGHT: "A's delete never reached B's tree" | not modelled |
| Two idle writers trade empty generations | `two_idle_writers_do_not_trade_empty_generations` | install nothing when the merge adds nothing (§4.5) | CAUGHT: "seq 5 -> 13" | not modelled |
| A UI write through the gateway, If-Match a peer's UNCITED upload (nothing holds the gateway off during uploads), is consumed and cited by a second writer, then re-cited over by the uploader's commit — the acked UI write is preserved nowhere. Found by the first full formal gate on rows 1–6 | `a_ui_write_over_an_uncited_upload_is_never_silently_lost`; gateway `a_blind_write_over_an_uncited_upload_is_refused_until_it_is_cited` | the gateway overwrites only a TRACKED version (manifest citation or inbox entry), else the retryable 409 before any precondition; untracked past 600 s or no live writer is fair game (`inbox::hitl_may_overwrite`) | CAUGHT in both (the rule disabled by exact string, each test fails) | `Inv_HITLDurable` in `LeanBarrierLeaseSentinel`; pinned as `LeanBarrierLeaseHitlOverUncited` (VIOLATED, 18,939,222 states), arm `HitlOverwritesTrackedOnly` |
| A writer lost for good between its upload and its commit leaves an uncited object at the key: the manifest keeps citing a generation the key no longer holds, a fresh checkout reads the orphan, a live writer keeps the cited version — until someone rewrites the path (finding 10) | `a_writer_killed_after_its_upload_does_not_leave_the_trees_diverged` (`#[ignore]`, fails today) | **not fixed.** Candidates: heartbeats name in-flight upload paths and a live writer reconciles a dead writer's list; or re-publish over an untracked object past a grace, preserving it | — | not modelled: the model's crash keeps the state directory |
| Row 3: a supersede of a peer's UNCITED upload leaves the peer citing a generation the key no longer holds | none | **not fixed.** Candidates: supersede a peer-stamped uncited object only inside a commit section AND re-read every upsert there (a HEAD per publish, on the lease), or park on it while its writer's heartbeat is fresh | — | hidden: the model's 412 arm PARKS on the first foreign 412, and `Inv_NoDangling` checks existence, not the generation |

Not done, and needed before anyone calls this "as built": the whole
formal gate has not been run on the extended module; the writers drill
(W1–W7) has never run; `probe_conditional_delete` has no recorded result
on real S3 or Ozone.

## 5. Verify before any code

1. `probe_conditional_delete` against a real general-purpose S3 bucket
   AND Ozone 2.2.1. **DONE 2026-09-13 (writers drill):** S3 us-west-1
   PASSES both legs; Ozone 2.2.1 FAILS the DELETE leg — a DELETE with a
   wrong If-Match returned 204 and removed the object, confirmed with the
   AWS CLI against S3 as the control (412 there). That is Ozone's scope:
   conditional DeleteObject is HDDS-14907 (under the HDDS-13117 umbrella),
   fix version 2.3.0, unreleased as of 2026-09-13. Multi-writer on Ozone
   2.2.x is unsafe; re-probe on 2.3.0 (`lean/e2e/writers-live/results/2026-09-13/`). §4.1 is blocked on this;
   so, strictly, is the confidence in today's Step 6 on Ozone.
2. `lean/e2e/run-writers.sh` W1–W7 — written 2026-09-13, never run;
   add the deposal count as a reported metric (zero on no-fault legs).
3. §4.1 is withdrawn; if the Iceberg grace-period sweeper (§4.1.1) is
   ever pursued, its model run in the adopt world comes first.
4. **Describe HEAD, not the working tree.** This document's first
   draft called `delete_if_match` "as built"; it is uncommitted. Run
   `git diff HEAD --stat -- crates/flint-store lean/syncer lean/formal`
   before writing "as built" about lean (80 files differed when this
   was checked).
5. Runtime refusal of a store that fails the conditional probes is not
   built (§3). Decide whether the syncer probes at `run` startup, and
   what it does on failure.
6. The convergence defects of §4.5: DONE in the working tree — each
   was a failing test before its fix and is mutation-checked (§4.6).
7. Decide row 3's residual (a supersede of a peer's uncited upload):
   the candidates are in §4.6; neither is built.
8. Run the whole formal gate on the extended module (never run
   end-to-end since tranche 6 and the `SyncKeepsHiddenBase` arm), and
   model the writer-local queue: the module still carries `foreignQ`
   in the SHARED inbox, which is no longer what the code does.

## 6. Decisions

| # | Decision |
|---|---|
| D1 | No consensus protocol is added to lean. The store's CAS is the consensus; a store without conditional PUT MUST be refused, not accommodated — and that refusal is not built (§3, §5 item 5). Revisit only if such a store must be supported (then Disk Paxos, §2). |
| D2 | Lean's protocol is named by its literature names in docs and code comments — Iceberg-style pointer commit; RFC 9110 conditional-request OCC on the data path; DynamoDB-lock-client lease with a fencing epoch; ticket-queue fairness. No behaviour change. |
| D3 | **WITHDRAWN.** §4.1 (GC outside the lease) reopens the delete-vs-adopt race (§4.1.1). The lease may not shrink below {verify, CAS, GC} exclusive across writers (§1.1). A grace-period sweeper is the only latency answer, and needs its own model run first. |
| D4 | §4.2 (append-only log) is recorded with its triggers and not built. |
| D5 | §4.3 (immutable data objects) is rejected and recorded as the price of the mirror. |
| D6 | No blockchain, permissioned or otherwise: its consensus answers a trust question lean does not have, and its replication is the cost the user named. The credential remains the enforcement (per-user access design). |
| D8 | A barrier whose merge adds nothing to theirs installs nothing; theirs becomes its merge base, the peer's changes go to the writer's own queue (§4.5). It claims once per foreign install it finds, never per idle tick. Built in the working tree with the foreign-integration fixes (§4.6), uncommitted. |
| D7 | The manifest IS the ledger. The ledger-aligned shape is §7.4 — an immutable, delta-sized, `prev`-linked record per barrier beside the CAS'd head, written pre-CAS and named by the pointer, retention a policy. Recorded; build only with the branching design, whose merge base it supplies. Supersedes §4.2's "replace the pointer". |

## 7. The manifest as a ledger; blockchain compared

User, 2026-09-13 (follow-up): "It should align with the manifest (a
ledger type of solution). Just for comparison — would a blockchain
solution help or that is irrelevant and out of scope and causes growth
in storage (hoping don't have to pull whole blockchain in)?"

### 7.1 What the manifest already is

- **Immutable, content-addressed blocks.** A generation is a pointer
  body naming a list of chunks at `.flint/lean/chunks/<addr>`, `addr`
  = hash of the chunk body (`lean/syncer/src/chunk.rs:84,101`). The
  pointer commits to every chunk's address and entry count; each
  entry carries its own CRC64 and etag. That is a one-level Merkle
  commitment to the whole tree: a reader that trusts the head can
  verify every byte it fetches.
- **A total order.** `seq` is bumped by every install and every
  rotation; the CAS on `current` is exactly "append the next ledger
  entry, or learn that someone else did" (§1).
- **Structural sharing.** Chunk boundaries are a function of the KEY
  ALONE (content-defined, `chunk.rs:26-41`), so an untouched chunk
  re-addresses to the same object and a barrier writes O(changed
  chunks), ~1 MiB each at `CHUNK_TARGET = 4096` × ~277 B/entry.
  Consecutive generations share every chunk they did not change —
  git's trees, by another name.
- **Bounded retention.** `KEEP_GENERATIONS = 5` (`manifest.rs:720`)
  behind the live one; chunk GC is refs-with-grace (`LeanChunkGC*.cfg`).

What it is NOT yet: a generation does not name its predecessor. There
is no `prev` link (`grep -i prev_seq\|parent lean/syncer/src/` is
empty), so history can be walked only by listing what retention has
not yet reaped. That single missing field is the difference between
"a sequence of snapshots" and "a ledger".

### 7.2 Blockchain, decomposed — three ingredients, three verdicts

| Ingredient | What it is for | Lean | Verdict |
|---|---|---|---|
| **Consensus among distrusting peers** (proof-of-work, proof-of-stake, PBFT; in permissioned chains an ordering service, which is Raft or PBFT) | Agreeing on the next block with NO trusted arbiter, tolerating Byzantine members | The store is the trusted arbiter and every writer is authenticated. A Byzantine writer holding bucket credentials can delete any object, chain or no chain — the credential is the enforcement (`flint-lean-per-user-access-design.md`) | **Irrelevant.** Solves a trust problem lean does not have; costs block cadence (seconds per commit against lean's milliseconds), a validator set, and a peer network (§2 row 1) |
| **Full replication, validate from genesis** | A node trusts nobody, so it must check the whole chain itself | A lean reader trusts the store's CAS: the head IS authoritative. It reads the pointer and the chunks of the CURRENT tree — O(tree), independent of how many barriers preceded it | **Never paid.** Lean's readers are "light clients" by construction (§7.3) |
| **Hash-linked, append-only record** | Tamper-evidence relative to a trusted head; a walkable history | Generations are already immutable and content-addressed; only the `prev` link is missing | **Take it.** It is git's commit graph (1980s Merkle trees, 2005 git), not a blockchain invention. §7.4 |

So "a blockchain solution" as a protocol is out of scope, and for the
reason the user guessed: what it adds beyond lean's CAS is paid for in
replication and block cadence, and what it shares with lean is already
there. The trust model is the decisive fact, not the storage cost: a
chain only protects history against parties who cannot write the
store, and lean has no such writers.

### 7.3 Storage: you never pull the chain

Why a blockchain node pulls the whole chain: (a) it trusts no
checkpoint, so it validates from genesis; (b) blocks are DELTAS, so
the state exists only by replay. Lean has neither property. Every
generation is a FULL snapshot (a chunk list), and the head is trusted
because the store linearised it. A checkout at seq 10,000,000 costs
the same requests as at seq 10.

What a retained history costs, per barrier, three ways:

| Retained per barrier | Size | At 17,280 barriers/day (5 s floor, always dirty) |
|---|---|---|
| Today: K = 5 generations, chunks refs-with-grace | bounded | flat |
| Full snapshot per generation (keep every pointer + every changed chunk forever) | ~25 KB pointer at 1M entries + ~1 MiB per touched chunk; a barrier touching 10 chunks ≈ 10 MiB | ~170 GB/day — **the blockchain growth curve; do not do this** |
| Delta-sized record per generation (§7.4): the barrier's upserts + deletes + header | ~277 B × changed entries + ~300 B; 20 changed files ≈ 6 KB | ~100 MB/day, ~36 GB/year per busy workspace — **still a policy, never "forever"** |

Readers touch none of these rows; only history tools do. Retention is
the Iceberg `expire_snapshots` / Delta `logRetentionDuration` /
`git gc` question, and lean already has the machinery (`KEEP_GENERATIONS`,
refs-with-grace).

### 7.4 The ledger-aligned shape — RECORD, build with branching

Keep the CAS'd `current` as the head; it is the ledger's height
register and the reader's 300-byte fast path, and its safety argument
is proven. Add an immutable record per barrier BESIDE it:

- `<prefix>/.flint/lean/log/<seq>-<flush_uuid>` written with
  `If-None-Match: *` BEFORE the pointer CAS, exactly as chunks and
  generation objects are written today, so a lost CAS leaves an orphan
  that the existing age rule reaps (`manifest.rs:724` — "an object
  above the live generation… age is the only honest discriminator").
  A merge retry rewrites it under the new `seq` with the new `prev`.
  A retry that finds its own record already present must REWRITE it,
  not adopt it by reference — the same rule chunks follow
  (`manifest.rs:574-591`), because the orphan sweep reads age.
- Body, O(changed entries): `{seq, prev: {seq, addr}, pointer_addr,
  epoch, holder_id, at_unix, boundary_source, upserts: [...],
  deletes: [...], foreign_preserved: [...]}` — the barrier already
  computes every field (`upserts`, `classified.deletes`,
  `report.foreign_queued`); `addr` is the record's content address,
  `prev` is the record THEIRS named.
- The pointer gains one field, `log: Option<{key, addr}>`, naming the
  record that produced it. That is the head-to-ledger link; the
  record-to-record `prev` links are the chain.
- Retention: a policy field (duration or count) with the same
  refs-with-grace GC; default = today's behaviour. "Forever" is an
  operator's choice with §7.3's numbers in front of them.

What it buys: a `prev`-walk from the head without LIST; an audit of
who published what and when (the forge/JWT work wants this); the
**merge base the branching design needs** (`flint-lean-branching-design.md`
§3.4 was re-based onto versioning lean no longer has; a common
ancestor by `seq` plus retained records is the other half of that
answer, the data-byte half remaining §4.3 or bucket versioning);
tamper-evidence relative to a trusted head, for what that is worth in
lean's trust model (audit, not security).

What it does not buy, stated so it is not oversold: Byzantine safety
(§7.2); point-in-time reads of the BYTES (data objects are mutable in
place, §4.3); and it does not replace the CAS — the record is a
projection of the head chain, and the head stays the commit. It
supersedes §4.2's "replace the pointer with a log": the log is added,
the pointer stays, the weakest-primitive argument of §4.2 is given up
(lean keeps requiring `If-Match`, which every probed store has).

Gate, as for everything in this doc: the model first (a `Ledger`
constant; a record orphaned by a lost CAS must never be named by a
pointer — `Inv_LedgerNamesOnlyInstalled`; its control deletes the
pre-CAS ordering and must violate), then the code, then a mutation
check on the `prev` link.

## 8. References

- Herlihy, M. "Wait-Free Synchronization." ACM TOPLAS 13(1), 1991 — CAS has consensus number ∞.
- Gray, C. and Cheriton, D. "Leases: An Efficient Fault-Tolerant Mechanism for Distributed File Cache Consistency." SOSP 1989.
- Burrows, M. "The Chubby Lock Service for Loosely-Coupled Distributed Systems." OSDI 2006 — sequencers (fencing tokens).
- Kleppmann, M. "How to do distributed locking." 2016 — fencing tokens for lease holders that do not know they are dead.
- Amazon DynamoDB Lock Client (README) — record version number, heartbeat, lease duration, takeover by conditional write.
- Gafni, E. and Lamport, L. "Disk Paxos." Distributed Computing 16(1), 2003. Chockler, G. and Malkhi, D. "Active Disk Paxos with infinitely many processes." PODC 2002.
- Kung, H. T. and Robinson, J. T. "On Optimistic Methods for Concurrency Control." ACM TODS 6(2), 1981.
- Armbrust, M. et al. "Delta Lake: High-Performance ACID Table Storage over Cloud Object Stores." VLDB 2020. Delta Lake 4.0, "Coordinated Commits."
- Apache Iceberg Table Spec — "Optimistic Concurrency": writers swap the metadata-file pointer atomically and retry with validation.
- Apache Hudi — concurrency control with lock providers.
- RFC 9110 §13 (HTTP Semantics: Conditional Requests), formerly RFC 7232. Nielsen, H. F. and LaLiberte, D. "Detecting the Lost Update Problem Using Unreserved Checkout." W3C Note, 1999.
- Lamport, L. "A New Solution of Dijkstra's Concurrent Programming Problem." CACM 17(8), 1974 — the bakery (ticket) algorithm.
- Fischer, Lynch, Paterson. "Impossibility of Distributed Consensus with One Faulty Process." JACM 1985 — why FIFO fairness with crash tolerance needs a timeout somewhere (lean's quiet polls).
- Amazon S3: strong read-after-write consistency (2020-12); conditional writes `If-None-Match` (2024-08) and `If-Match` (2024-11).
