# F65 — a truncate does not recall held layouts, so the truncate gate only covers clients that don't have one yet

Status: **FOUND BY TLC 2026-07-31; fix landed the same day; a multi-agent
audit hours later found that fix INEFFECTIVE; the four blocking defects are
now FIXED.** F65 itself is closed. `Inv_NoStaleServe` still does not hold,
for one remaining reason that is not F65 — see the table.

| audit finding | run | state |
| --- | --- | --- |
| F65 itself — no recall on truncate | `FlintTruncateHeldLayout.cfg` | **fixed** |
| C1 — layout stateid seqid never incremented (RFC 8881 §12.5.3) | — | **fixed** |
| C2 — CB_SEQUENCE hardcoded slot 0 / seqid 1 (§2.10.6.1) | — | **fixed** |
| C3 — a refused reply scored as an ack | — | **fixed** |
| C6 — grant escapes gate + recall via a publish race | `FlintTruncateGrantRace.cfg` | **fixed** |
| C5 — one back-channel writer per session vs `nconnect=4` | `FlintTruncateLostRecall.cfg` | **open** |
| C4 — LAYOUTCOMMIT re-extends the truncated stub | — | **open** |
| R1 — recalled client refused the MDS fallback during a parked truncate | — | **open** |
| R2 — self-recall stalls the connection read loop for the CB timeout | — | **open** |
| R3 — post-recall LAYOUTRETURN answered SERVERFAULT | — | **open** |
| R4 — the truncate-dirty gate does not survive an MDS restart | — | **open** |

`FlintTruncate.cfg` (the shipped world) still does NOT list
`Inv_NoStaleServe`: server-side revocation cannot bind a client the recall
never reached, and C5 makes that reachable on an ordinary mount.
`FlintTruncateNoStaleServe.cfg` is the conditional green — what closing it
requires.

C3 is the one to understand if you read nothing else. `decode_cb_reply`
returns `Ok` for any NFS4 status once the RPC layer accepts, so
`Ok(_reply) => Acked` scored a *refusal* as a success and every log line
read `1/1 acked`. That is why C1 and C2 — two independent RFC violations
that make a conforming client reject the recall outright — survived a
formal model, a passing gate, a test suite and a review. The instrument was
lying, so nothing downstream of it meant anything.

## The one-sentence version

`truncate_dirty` is checked in LAYOUTGET, and `note_truncate` never touches
the layout manager — so a client that acquired its layout *before* the
truncate reads its stripes directly from the DSes, past the new EOF, without
the MDS ever being consulted.

## Why it was reachable

*(All present tense in this section describes the code BEFORE the fix.)*

The gate does exactly what its comment says, at the only place it is
installed (`mds/operations/mod.rs:171`):

```rust
// Truncate-dirty gate: while a size change is unconfirmed on
// any pinned DS, a fresh layout would let the client read
// stale stripe bytes beyond the new EOF. TRYLATER regardless
// of how long it has been dirty — layouts must NEVER expose
// stale bytes; ...
if self.layout_manager.truncate_dirty_since(&gate).is_some() {
    return Err(LayoutGetError::TryLater);
}
```

Note the word **fresh**. `note_truncate` marks the gate, fans out `set_len`
to every pinned DS, and returns; it calls `mark_truncate_dirty`,
`truncate_fanout` and `clear_truncate_dirty_if`, and nothing else. The only
`CB_LAYOUTRECALL` anywhere in the tree is the device-heartbeat fan-out for a
**dead DS** (`mds/mds/server.rs:982`, `:991` → `fan_out_recalls`). A truncate
is not a DS failure, so that path never fires.

An outstanding layout is therefore untouched by the truncate, and pNFS reads
under a layout do not reach the MDS at all — they go straight to the DS,
which still holds the pre-truncate bytes until its `set_len` lands.

TLC finds it in **three steps** from the initial state:

```
State 1  gate clear, size = 2, both DSes hold {1,2}
State 2  LayoutGet(c1)          — legal: the gate is clear
State 3  SetSize(0)             — gate arms, fanout in flight, DSes unchanged
State 4  Read(c1, d1, 2)        — 2 > size and 2 is still on d1  ⇒ stale bytes served
```

Nothing exotic is required: no DS failure, no retry, no concurrent truncate,
no lost message. It is the ordinary path.

## What is NOT wrong

Worth stating, because the gate's own machinery looks more suspicious than
this and turned out to be sound. `clear_truncate_dirty_if` is a
check-then-act, and the background retry task re-reads the deepest pending
size and then clears with the value it just read — a repair writing its own
guard's input, which is the F62 shape. Modelling the re-read and the clear
as separate steps lets TLC interleave a fresh SETATTR between them, and
`Inv_ClearImpliesFlushed` (mark absent ⇒ no DS holds content past the MDS
size) **holds** across 25,572 states. The `confirmed <= min` predicate is
load-bearing: `FlintTruncateBlindClear.cfg` drops it and TLC immediately
finds a shallower confirm lifting a deeper cut's mark.

One hypothesis was refuted: keeping the *minimum* in `mark_truncate_dirty`
is not what carries the safety. `FlintTruncateMarkOverwrite.cfg` overwrites
instead and still holds — overwriting only ever raises the mark, and the
mark can only rise on a SETATTR that also raised the file size, so the
exposure it would create is unreachable.

## The fix as shipped

`note_truncate`, between `mark_truncate_dirty` and `truncate_fanout`:

```rust
self.layout_manager.mark_truncate_dirty(&gate, new_size);
self.recall_layouts_for_truncate(&gate, new_size).await;   // ← F65
let ok = truncate_fanout(...).await;
```

A recall, **not** a wider gate. Widening cannot work, because the read
never asks the MDS anything. Five pieces:

1. **`LayoutState.file_ident`** — the file identity a layout was issued
   against, recorded at grant time as *literally* `truncate_gate_key`'s
   output from the same placement the stripe map came from. The recall
   selector and the gate are therefore keyed identically by construction.
   Deriving the key twice would let them drift, and a drifted key recalls
   nothing while looking like it worked.
2. **`LayoutManager::recall_layouts_for_file`** — returns
   `(session, stateid, filehandle)` per layout. An empty ident never
   matches; see the residual below.
3. **`CallbackManager::send_layoutrecall_range`** — CB_LAYOUTRECALL for
   one FH and one range. The dead-DS path deliberately sends an empty FH
   (Linux reads that as session-wide, which is right when a device dies);
   doing that on every `SETATTR(size)` would drop the client's layouts
   for every unrelated file. The truncate path passes the layout's own FH
   and `[new_size, ..)`.
4. **`callback::recall_layouts_for_truncate`** — the fan-out, with a
   *different* revocation policy from the dead-DS one. There, `Acked`
   gets a soft post-recall deadline because the client may still have a
   legitimate LAYOUTCOMMIT. Here the bytes past `new_size` are going away
   by definition, so a grace period is not politeness — it is exactly the
   exposure window the recall exists to close. Every layout is revoked
   server-side as soon as its recall attempt returns, whatever the
   outcome, and before the next recall is sent.
5. **`PnfsOperationHandler::attach_callback_manager`** — a `OnceLock`,
   because the wiring is a cycle: `CallbackManager` borrows the
   dispatcher's back-channel registry, the dispatcher needs the handler
   as its `PnfsOperations`, and the handler needs the CallbackManager.
   `MdsServer::new` closes the cycle after construction.

Persistence: `LayoutRecord.file_ident`, schema **v7**
(`ALTER TABLE layouts ADD COLUMN file_ident TEXT NOT NULL DEFAULT ''`).
Without it every layout restored across an MDS restart would be
unrecallable and F65 would quietly reopen for them.

The recall blocks the SETATTR/OPEN compound for up to one CB round-trip
per outstanding layout, bounded by `CallbackManager`'s per-call timeout.
That is deliberate: the alternative is returning success while the
client's peers can still read bytes we just promised were gone. The
client *issuing* the truncate is recalled along with everyone else —
excluding it would rest on the assumption that a client never reads past
a size it set itself, which is a claim about client behaviour, not about
this server.

## The residual — NOT closed

Revocation is server-side. It binds the MDS's bookkeeping, not the
client: one behind a dead back-channel (`NoChannel` — never bound, or
`cb_program=0`) or one that does not answer (`TimedOut`) still believes
it holds the layout, and its reads go straight to a DS. The code revokes
through all of those outcomes, which is correct and is *not* sufficient.

`FlintTruncateLostRecall.cfg` is that world and TLC finds the violation.
Closing it needs the **DS** to refuse reads past the pending size — a
DsControl fence issued before the `set_len` fanout, over the same channel
`ds_truncate_one` already uses — not the MDS to ask more politely.

That is a deliberate non-fix for now: it costs an extra RPC on every size
change, and the exposed read was issued before the truncate was
observable, which is defensible under NFS consistency. Revisit if a
workload turns up that truncates under readers with flaky back-channels.

There is a second, narrower residual: layouts restored from a **pre-v7**
row carry no `file_ident` and cannot be matched to a file, so a truncate
does not recall them. `recall_layouts_for_file` logs a WARN naming the
count rather than treating `""` as a wildcard (which would recall every
such layout on every truncate of any file). It closes itself as those
layouts are returned.

## Scope limits — read before citing this

* **Reads are atomic with respect to revocation in the model.** So the
  recall result says the *server* stops handing out the ability to read
  stale bytes. It says nothing about a read already on the wire to a DS,
  which no MDS-side mechanism can reach. Fencing that requires the DS to
  refuse, and this module cannot speak to it.
* **Whether a conforming client would issue the offending read is a
  client-behaviour question the model does not settle.** A Linux client
  would have to read before revalidating the size. The model settles that
  flint does not stop it — which is the only half flint can fix.
* Every DS is modelled as holding the same logical offset set. The stripe
  map changes *which* DS exposes the byte, never *whether* one does.
* `set_len` growth adds zeros, and zeros are not content. So a stale fanout
  re-extending a stripe file is a size disagreement, not a stale read. That
  disagreement is real and unmodelled here — the DS's own EOF would differ
  from the MDS's — but it is a separate question from this one.

## Gate runs

| cfg | claim | required |
| --- | --- | --- |
| `FlintTruncate.cfg` | shipped world: both theorems | PASS |
| `FlintTruncateBlindClear.cfg` | `confirmed <= min` is load-bearing | FAIL (found) |
| `FlintTruncateMarkOverwrite.cfg` | min-keeping is *not* what carries safety | PASS |
| `FlintTruncateHeldLayout.cfg` | **F65 regression** — no recall | FAIL (found) |
| `FlintTruncateLostRecall.cfg` | **open residual** — recall never arrives | FAIL (found) |
