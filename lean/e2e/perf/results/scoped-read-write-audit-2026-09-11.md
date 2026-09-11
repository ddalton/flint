# Scoped read/write — what an adversarial audit did to the design

Six tracers over the code paths a PARTIAL baseline would touch, each
one's most severe finding handed to an independent skeptic told to
refute it. **This audit read code and ran nothing** — no build, no
tests, no cluster.

## The claim it was given

That a scoped checkout needs no new scope record, because
`checkout.rs:554` builds `baseline.entries` only from what it processed,
`scan.rs:114` derives deletes from `baseline.entries.keys()` rather than
the manifest, and `checkout.rs:560` leaves `inst_base` full as the merge
base.

## The verdict: qualified — it holds until the first barrier

| link | verdict |
|---|---|
| scoped checkout ⇒ scoped baseline | **holds** — `:552` and `:554` fill from the same result |
| scoped baseline ⇒ no spurious deletes | **holds, conditionally** — requires `prev_scan ⊇ entries.keys()`, an invariant stated nowhere and asserted by no test |
| the baseline STAYS scoped | **FALSE** |

Three doors insert arbitrary out-of-scope paths into `baseline.entries`
with no scope test, because there is nothing to test against:

1. **merge → inbox → consume.** A sibling changes out-of-scope `P`;
   `manifest.rs:998` marks it foreign, `barrier.rs:850-859` writes it
   into our own cell as an `InboxEntry`, and the next `consume_inbox`
   adopts it clean — `barrier.rs:233` inserts into `entries`, `:243`
   into `prev_scan`. The scope is gone for `P`, permanently.
2. **Gateway HITL.** `gateway.rs:110-118` bars traversal and the control
   dir only; any workspace path is legal.
3. **Whole-tree `sync()`.** `sync.rs:99` `in_scope` is
   `.unwrap_or(true)` when scope is `None`, so the D4 guard is inert.

And the premise inverts a rule the crate already relies on:
`sync.rs:13-22` names merge → inbox → consume as **the designed
destination** for out-of-scope foreign changes.

## Two more corrections to the proposal

- **"Widening is free" is false.** `checkout.rs:194-197` returns early
  forever on `marker_present()`, the marker records no scope
  (`state.rs` — it is literally `b"ok\n"`), and nothing removes it. A
  widened scope cannot be materialised by re-running checkout.
- **The `inst_base` argument is moot.** `barrier.rs:843-844` rewrites
  `inst_base` to the full installed manifest at every barrier
  unconditionally, so whatever checkout wrote is erased at first publish.
- **Narrowing has NO grace period**, not the one scan assumed:
  `checkout.rs:561` REPLACES `prev_scan`, so out-of-scope survivors are
  in `entries` but not `prev_scan` and route straight to `deletes`; and
  `confirm_absences` (`barrier.rs:347-354`) collapses the two-scan rule
  to one lstat on a declared boundary.

## What it costs to build anyway

A scope field on `Baseline` and four filters — at the queue where it is
BUILT (`barrier.rs:850`), at consume (`:128`), and at both delete sites
(`barrier.rs:806`, `gated.rs:535/:1047`) as a belt-and-braces clamp so a
future scope bug costs zero objects rather than the whole out-of-scope
set. Plus an explicit NARROW verb: a narrow must remove paths from
`entries` and `prev_scan` in the same step they leave the tree — a
narrow is an *unwatch*, never an *absence*. Design that verb first.

Two defects worth fixing whether or not scoping ships:
- `barrier.rs:216-230` consumes an inbox entry that failed on
  ENOSPC/EACCES/EIO as `consume-refused-containment`; nothing re-offers
  it.
- `Scope::new` (`sync.rs:59-68`) silently drops malformed entries and
  `sync.rs:98` turns an all-rejected scope into `None` = whole tree.
  On a checkout path that must be an error, not a widening.

## What this audit cannot say

It ran nothing. The probes its tracers wrote are preserved as
`scoped-audit-probes.patch` and are **UNRUN** — `git apply` it and
`cargo test probe_` on Linux before treating any "verified" above as
more than a report of a report. The H1 mutation control (mutate
`manifest.rs:999` to `if false &&` and watch `foreign_queued` go 1 → 0)
is the single strongest piece of evidence and the one most worth
re-running.

Nobody opened: the CSI/node side under H1's refill, the operator CRD
surface (there is no field in which to express a scope, and nothing
checked what adding one prunes), or `lean/formal/` — whether
`LeanSubtree.tla`'s `baseline`/`instBase` split can express a scope at
all. Given that collapsing those two states is already a bug the model
caught once, the narrowing rule is exactly what to prove there first.

Unresolved and measurable: whether the fleet's buckets are versioned.
On an unversioned bucket the widen-then-`rm` path is unrecoverable; on a
versioned one it is a delete marker plus a reapable noncurrent version.
