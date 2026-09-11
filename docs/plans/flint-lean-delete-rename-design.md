# UI delete and rename on lean — design of record

Status: **DESIGN — no code.** Written 2026-09-11, after a user asked
whether a Rust backend powering a file-browser UI could use flint-lean
as a LIBRARY rather than pointing at a gateway endpoint per bucket. It
can, for list/read/create. It cannot for delete, rename or move — and
this document says why, what the shape of the fix is, and the one thing
about it that turns out to be better than the agent-side behaviour it
copies.

Scope note: everything here is equally a library and an HTTP feature.
`gateway.rs` is a shell over `inbox.rs` (§8), so a verb that lands in
the library is reachable from both without a second implementation.

## 0. The gap, precisely

**Lean deletes work.** An agent that runs `rm` in the mount gets the
deletion published: `classify` puts the path in `first_absence`, the
next scan promotes it to `deletes`, and `barrier.rs:853-887` issues the
object DELETE. `mv` works too, because locally it is a delete plus a
create.

What does not work is a delete from **outside the pod**. `InboxEntry` is
`{path, etag, author, added_unix}` — there is no way to say *gone*. And
`consume_inbox` HEADs `file_key(path)` for every entry, so an entry
naming no object lands in the NotFound arm as a spurious
`consume-object-missing` conflict. The inbox cannot express a deletion,
and the function that drains it would misread one if it could.

Rename and move inherit the gap: they are a delete plus a create.

## 1. Why the operations are asynchronous at all

Worth stating, because the fix has to live inside this constraint.

**The sidecar is the only process that may write the workspace.** The
gateway holds no tree at all — `LeanConfig::new(prefix, "/nonexistent")`
— and a library caller in a backend service is in the same position:
different pod, probably different node. So an outside write is not
"deferred for throughput"; it is queued because **the caller physically
cannot touch the tree**, and the one process that can is also the one
holding the lease.

The codebase draws the line by trust boundary, not by latency. Compare
the two sync doors:

- `inbox::gateway_request` — *carried, never performed* (D14). A remote
  caller asking lean to rewrite a running agent's tree at a time of the
  caller's choosing is an escalation of what a leaked bearer can do.
- `uds::CtlRequest::Sync` — *executes*, "because the caller is inside
  the pod: it is the agent asking for its own tree to be updated, which
  is the agent's own decision to make."

So lean already has a synchronous door. It is pod-local on purpose.

**What is NOT deferred:** the bytes. A UI write PUTs the object
synchronously and returns its ETag; `GET /files/{path}` already falls
back to an uncited-but-tracked inbox entry. The object is durable and
readable the moment the call returns. What waits for a barrier is the
CITATION — i.e. when the agent's tree sees it, and when the manifest
names it. `boundary_request` is the lever that stops that wait being the
cadence floor.

## 2. The decisive constraint: the key IS the path

```rust
pub fn file_key(&self, path: &str) -> String {
    format!("{}/files/{}", self.prefix, path)
}
```

A rename therefore moves bytes. It is not a manifest edit. Under a
content-addressed layout it would be free — and lean deliberately does
not have one, because `<prefix>/files/<path>` is legible: a passthrough
mount can be pointed straight at a lean workspace and see the tree,
which is exactly what the 2026-09-10 door drill did. **Cheap renames are
what a legible layout costs.** That trade is not reopened here.

Two consequences that exist TODAY, before any new work:

1. `mv big.ckpt other.ckpt` in the mount **re-uploads the whole file**.
   The new path is a new key, so the barrier has no base generation to
   compose from (`PartSource::BaseCopy` copies ranges from the SAME
   key). A 10 GB checkpoint rename is a 10 GB upload, and it reads like
   a network problem rather than a design consequence.
2. An agent rename is **not atomic**: barrier 1 uploads the destination
   while the source is only `first_absence`, so the manifest transiently
   cites both; barrier 2 deletes the source. A reader in between sees a
   duplicate, never a hole — the right direction, and it falls out of
   the existing rule rather than being designed for.

## 3. The inbox surface: a FIELD, not a tombstone entry

`InboxDoc` already faced this question once, for "please publish", and
answered it in a comment that applies verbatim here:

> *Deliberately a **FIELD** and not a fake no-object `InboxEntry`:
> `consume_inbox` HEADs `file_key(path)` for every entry, so an entry
> naming no object lands in the NotFound arm as a spurious
> `consume-object-missing` conflict — and special-casing the single most
> safety-critical function in the crate to avoid that is worse than
> either.*

So: a `removals` field on `InboxDoc`, alongside `entries`,
`boundary_request` and `sync_request`, `#[serde(default)]` so existing
cells parse unchanged. `consume_inbox` is NOT modified — it keeps
draining `entries` exactly as it does now, and a separate pass applies
removals. One CAS document, two lists, no new behaviour inside the
crate's most dangerous loop.

A removal carries `{path, author, requested_unix}` and, for the rename
case, a `moved_to: Option<String>` — recorded for the audit trail and
the conflict message, never load-bearing for correctness.

## 4. A DECLARED delete does not need the two-scan guard

This is the part worth getting right, and it makes the UI path better
than the agent path rather than a degraded copy of it.

`scan.rs` states why deletion is withheld for one scan: *a directory
renamed mid-walk can appear in neither pass of one readdir, and that
must never read as mass deletion.* The guard exists to distinguish a
real deletion from **an artifact of inferring absence by walking**.

A UI removal is not inferred. The intent is recorded in the inbox cell,
durably, before anything happens. The guard has nothing to protect
against — so a declared removal goes **straight into
`classified.deletes`**, skipping `first_absence`.

And that unlocks the good property. `merge` takes `classified.deletes`
and the barrier installs ONE manifest generation. If a rename's create
and removal both reach the same barrier, **one CAS'd manifest carries
both halves, and every reader resolving through the manifest sees the
rename atomically** — no duplicate window, no hole window. The UI path
gets an atomicity the agent path cannot have, because the agent path
only ever has absence to reason from.

`confirm_absences` still runs, and still fails closed. For a declared
removal it is not the delete oracle — the declaration is — but it is the
check that the local unlink actually happened. An unlink that failed
must not publish a deletion, which is the same rule `14b3637c`
established for the inferred path.

## 5. Ordering: create first, removal second. Always.

Within one inbox transaction and within its application. A partial
application must leave an EXTRA file, never a missing one — the same
rule as object-first-inbox-second on the write path, for the same
reason: an orphan is recoverable and a hole is not.

## 6. Rename is ONE transaction or it is not a rename

The inbox is a single CAS'd document, so a rename is one CAS carrying
the destination entry and the source removal together. Two calls would
be two renames' worth of failure modes, and the crash between them is
the one that loses data.

The sidecar then:
1. applies the destination (existing consume path, unchanged),
2. refuses on a locally-dirty source — conflict recorded, nothing
   applied, both halves left in the cell to retry,
3. unlinks the source and marks it for immediate deletion (§4),
4. one barrier, one manifest generation, both halves.

**One honest limit:** atomic *for manifest readers*. A passthrough mount
over `<prefix>/files` reads the bucket, not the manifest, so it sees two
object operations. Another reason for one product per prefix.

## 7. Prerequisite: a cross-key server-side copy

CORRECTION (2026-09-11, on building it): flint-store did not have
"none" — it had a cross-key path nobody could reach. `ComposeSpec`
carries `base_key`, used at `s3.rs` and `memory.rs` as
`spec.base_key.unwrap_or(spec.key)`, written for the A7 re-key flush.
**`base_key: Some(..)` appeared NOWHERE in the repository** — not in
production, not in a test — so the branch had never executed. The
statement to keep is the narrower one: there was no WHOLE-OBJECT copy,
and the range-copy path that existed was dead code.

`PartSource::BaseCopy` is same-key by construction otherwise. Without a
copy, a UI renames a 10 GB file by downloading and re-uploading it,
which is worse than the agent path it is meant to improve on.

**SHIPPED.** `copy_object(src_key, src_if_match, dst_key, condition,
stamps)` on the `ObjectStore` trait, both backends and all seven test
doubles: `CopyObject` under a settable 5 GiB ceiling, MPU with
`UploadPartCopy` above — and that MPU arm is the FIRST caller of
`base_key`, so the dead branch now has a caller and a test.

Two rules the implementations agree on, because a double that gets them
wrong passes a test the real store fails:

- the destination's CRC **is** the source's — identical bytes, and a
  checksum that changed means the bytes did;
- the destination's **stamps are not** — `generation`, `epoch` and
  `flush_uuid` describe a publish, and inheriting them files one
  object's history under another object's key.

It was **separable and paid for itself twice** — it is also the fix for
the agent-side `mv` re-upload in §2.1, and it landed alone.

A rename's destination must carry its own `GenerationStamps` and a
`crc64_b64` that describes the bytes: a copy that inherits the source's
stamps installs a generation number from another object's history. The
memory double must model that, or it will pass a test the real store
fails — the double that returned ranges instantly and the one that never
quoted etags are the precedent.

## 8. Library form falls out; one wart to fix on the way

`inbox.rs` is already the library: `load`, `cas_write`, `admits_hitl`,
`gateway_append`, `gateway_request`, `open_window`, `drop_entries`,
`clear_window` — all `pub`, all over `&dyn ObjectStore` + `&LeanConfig`,
no HTTP. `gateway.rs` is `err_reply`, `ok_json`, `token_ok`,
`judge_preconditions` and a dozen `handle_*` functions that unwrap
headers and call the above. That split holds because every piece of
gateway state lives in the bucket — the window is read from the CELL by
every replica, which is what makes the gateway stateless AND linkable.

So the verbs land as `inbox::gateway_remove(store, cfg, removals,
author)` and `inbox::gateway_rename(store, cfg, from, to, author)` next
to `gateway_append`, and the HTTP routes are ~20 lines each.

**The wart:** `path_ok` (`gateway.rs:110`) is private. It is the rule
that rejects traversal, absolute paths, empty segments, `.`/`..`,
`STATE_DIR` and anything under `.flint/`. A library caller must
reimplement it today, and a hand-rolled second copy of a path-traversal
check is precisely the defect that reimplementation produces. It moves
into the library and becomes `pub` as part of this work — one predicate,
not two copies to drift. Same argument as the scoped-read admission
filter.

## 9. What the API must make INEXPRESSIBLE

In-process, some rules cannot be enforced for the caller: object-first-
then-inbox, and checking `admits_hitl` before writing. A removal has a
sharper one — **the caller must never issue the `DeleteObject` itself.**
The field records intent; the barrier performs it; the manifest stays
the authority on what exists. A service that deletes a cited object
directly wedges the workspace: the next checkout refuses with *"manifest
cites `<key>` but the object is gone — refusing a silent hole
(mixed-writer bucket?)"*.

The way to make that safe is not documentation. **The library must
expose no function that deletes a cited object.** If the dangerous thing
is not expressible through the API, it does not need a rule.

## 9a. The conflict has to reach a human who closed the browser

The refusals in §4 and §6 are useless if nobody learns of them. A user
saves, closes the tab, and the conflict happens minutes later inside a
barrier. No HTTP response can carry it — that is inherent to §1, not a
gap in the design.

**Start from what is already true: nothing is lost.** In `consume-dirty`
the agent's local version wins, but `preserve_conflict_copy` runs FIRST
and the comment says why — *"a conflict record must keep both versions
recoverable"*. The foreign bytes ARE the user's write, and they land at
`{prefix}/.flint/lean/conflicts/{uuid}/{path}`. So the message is never
"your work is gone", it is "your version is here", and a missed
notification is an annoyance rather than a loss. Every decision below
follows from wanting to keep that true.

**What is missing is three small things.**

1. **`ConflictRecord` carries no author.** It is `{path, foreign_etag,
   preserved_key, kind, at_unix}`. Publish that and you still cannot say
   WHOSE change lost. `entry.author` is already in scope at the
   `consume-dirty` site — a one-field addition, and without it the
   feature cannot route.
2. **The records are in the wrong place.** `conflicts.jsonl` lives in
   the sidecar's pod-local state dir and `gateway.rs` has ZERO
   references to it. It becomes a cell in the bucket, the same shape as
   the inbox: one CAS'd document, the sidecar the only writer, gateway
   and library as readers, with an acknowledge verb.
3. **Nothing else is needed to correlate.** `foreign_etag` IS the etag
   the caller's PUT returned, so `(path, foreign_etag)` matches a
   caller's own outstanding-write row exactly. No correlation id to
   invent.

**The division of labour.** Lean's job ends at making the event
durable, attributable and addressable. Delivering it to a human — email,
Slack, a badge at next login — belongs to the calling service, which
already knows the author, their preferences, and whether they are
connected. Lean should not grow a notification system.

**What a caller owes.** Persist its own outstanding writes —
`(workspace, path, etag, author, at)` — and reconcile on next load:
cited at your etag means delivered; in the inbox at your etag means
queued; neither means look in the conflicts cell.

## 9b. Retention, because today BOTH of these grow forever

The barrier runs two reapers. `sweep_chunks` lists
`{prefix}/{LEAN_DIR}/chunks/`; `sweep_generations` lists `manifests/`.
**Nothing lists `conflicts/`.** And `append_conflict` opens the jsonl
with `.append(true)` and writes a line — no cap, no rotation, and
`load_conflicts` re-parses the whole file on every call.

**This is the EASY collection case.** A chunk may be referenced by
several generations, which is why `sweep_chunks` needed four
model-established rules and six mutation configs. A conflict copy is
referenced by exactly ONE record. Record gone ⇒ copy is garbage. One
mark-and-sweep at barrier time beside the other two, on the same
best-effort terms: list `conflicts/`, subtract the `preserved_key`s of
live records, delete the difference — which collects acknowledged copies
and crash-orphans in the same pass.

**Ordering: record first, then object.** A crash between leaves an
orphaned copy, collected by the next sweep. The other order leaves a
live record whose `preserved_key` 404s — the UI offers "view your
version" and hands the user an error about the thing it promised was
safe. That is the dangling-citation shape D8 already has rules about.
Same principle as object-first-inbox-second: leave an extra thing, never
a missing one.

**Three ways a record dies, and the third is the one that bounds
growth:**

1. **Acknowledged** — the user discarded or applied. Immediate.
2. **Aged out** — a TTL, because most conflicts are never acknowledged;
   the user simply never returns. It must be a STATED number the UI can
   show: "your version is kept for N days."
3. **Superseded** — a conflict copy is made PER WRITE, not per barrier,
   so an auto-saving editor against a file the agent holds dirty
   produces one copy per save, potentially of a large file. That is the
   real growth scenario, and the crate already answers it in
   `gateway_append`: *"A newer write to the same path supersedes the
   queued one."* The conflicts cell does the same on `(path, author)` —
   the older copy is that user's own earlier draft, which their later
   save already replaced.

Plus a hard cap on the cell, oldest evicted, as the inbox backlog is
capped. `ObjectMeta.size` is known at preserve time, so the record
should carry `bytes` and the cap should be a BYTE budget as well as a
count: one 10 GiB checkpoint conflict matters more than ten thousand
small ones.

**A bucket lifecycle rule is a BACKSTOP, never the mechanism.** It is
out-of-band from the record, so as the primary it produces exactly the
dangling `preserved_key` ruled out above. Set LONGER than the record TTL
— records 30 days, lifecycle 45 — it covers the one case the sweep
cannot: a workspace whose sidecar never comes back, where no barrier
ever runs. Not available on every endpoint, so it cannot be relied on.

**Standalone, ships regardless:** `conflicts.jsonl` needs a cap and
rotation whether or not any of this lands. Append-only, fully re-parsed
on each read, and filled by the same auto-save path. Bounded by the
pod's lifetime rather than permanent, so it is the smaller half — and a
few lines.

## 10. Phases, each with the control that makes it mean something

**Phase A — cross-key copy (§7), alone.** Control: a copied 6 GiB object
(MPU path) and a copied 1 MiB object (CopyObject path) both read back
byte-identical with a `crc64_b64` matching their own bytes, and the
destination's generation is its own, not the source's. Mutation: make
the copy inherit the source stamps and watch the generation assertion
fail.

**Phase B — the `removals` field and the declared-delete path.**
Controls: (1) a declared removal reaches `classified.deletes` in ONE
barrier, and the mutation that routes it through `first_absence` instead
makes the one-barrier assertion fail; (2) a removal on a locally-dirty
path applies NOTHING and records a conflict — mutation: drop the dirty
check and watch the agent's edit disappear; (3) a failed unlink publishes
no deletion.

**Phase C — rename as one transaction.** Control: kill the sidecar
between the destination write and the source unlink; on restart the
source still exists and the transaction re-applies — an extra file, never
a hole. And the atomicity claim: a manifest reader observes either
{source} or {destination}, never both and never neither, across the
barrier.

**Phase D — the library surface and `path_ok`.** Control: a traversal
path refused identically through the HTTP route and the library
function, driven from ONE table of cases. Two code paths agreeing
because they share the predicate is the point; a second table would
let them drift and still pass.

**Phase D2 — the conflicts cell, its author field and its reaper
(§9a, §9b).** A PREREQUISITE for B and C rather than a follow-on: the
locally-dirty refusal has nowhere to surface without it, and "we refused
your delete" is otherwise silence. Controls: (1) a conflict raised by
author X is readable through the library and names X — mutation: drop
the author field and watch the routing assertion fail; (2) the sweep
collects an acknowledged copy AND a crash-orphan in one pass, and
collects NEITHER a copy a live record still cites — the third is the one
that matters, so mutate the subtraction and watch a live `preserved_key`
404; (3) a second conflict for the same `(path, author)` leaves ONE
record and ONE copy.

**Phase E — the formal side.** `LeanSubtree.tla` gains a declared
removal. The invariant is that a declared removal never deletes an
object the manifest does not cite, and never leaves a citation whose
object is gone. Assert it against a version that routes declared
removals through `first_absence` and confirm TLC finds the rename's
duplicate window first — a green run on the unfixed model means the
model is wrong.

## 11. What this design refuses

- **A tombstone `InboxEntry`.** §3: `consume_inbox` is the single most
  safety-critical function in the crate and it is not being taught a
  second shape of entry.
- **A content-addressed layout to make rename free.** §2: legibility is
  a product feature, and the passthrough drill depends on it.
- **Performing a remote `sync` request.** Unchanged: D14 stands, and a
  removal is carried the same way — recorded remotely, performed
  locally.
- **Deleting the object from the caller.** §9.

## 12. Open

Whether a removal should be `admits_hitl`-gated like a write. A write
races the barrier that is about to publish the tree; a removal touches
no object and no path at the moment it is recorded, which is the same
argument that made `gateway_request` deliberately NOT window-gated.
Leaning ungated, for that reason, but it is the one decision here that
nothing in the existing code settles.
