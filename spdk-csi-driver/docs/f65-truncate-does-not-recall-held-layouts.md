# F65 — a truncate does not recall held layouts, so the truncate gate only covers clients that don't have one yet

Status: **FOUND BY TLC 2026-07-31** while modelling the truncate gate
(`formal/FlintTruncate.tla`). **Not yet fixed.**
`formal/FlintTruncateHeldLayout.cfg` is a gate run that must keep FAILING
until it is.

## The one-sentence version

`truncate_dirty` is checked in LAYOUTGET, and `note_truncate` never touches
the layout manager — so a client that acquired its layout *before* the
truncate reads its stripes directly from the DSes, past the new EOF, without
the MDS ever being consulted.

## Why it is reachable

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

## The fix, and why it is the small one

`FlintTruncateRecall.cfg` sets `RecallOnTruncate = TRUE`: revoke every
outstanding layout for the file when the gate arms. It holds. That is the
whole change — a recall, **not** a wider gate. Widening the gate cannot work,
because the read never asks the MDS anything.

Concretely, in `note_truncate`, between `mark_truncate_dirty` and
`truncate_fanout`: take the file's layouts (`layouts_for_client` has the
index; a per-file lookup is the missing piece), `send_layoutrecall` each one
through the existing `CallbackManager`, and apply the same revocation policy
matrix `fan_out_recalls` already uses — `TimedOut`/`NoChannel`/`Transport`
revoke immediately, `Acked` gets the post-recall deadline. All of that
machinery exists; it is currently reachable only from the heartbeat monitor.

Ordering matters: the recall must precede the fanout, for the same reason
the mark does. A recall issued after the DSes are cut is decoration.

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
| `FlintTruncate.cfg` | the gate's own claim holds | PASS |
| `FlintTruncateBlindClear.cfg` | `confirmed <= min` is load-bearing | FAIL (found) |
| `FlintTruncateMarkOverwrite.cfg` | min-keeping is *not* what carries safety | PASS |
| `FlintTruncateHeldLayout.cfg` | **this defect** | FAIL (found) |
| `FlintTruncateRecall.cfg` | recall closes it | PASS |
