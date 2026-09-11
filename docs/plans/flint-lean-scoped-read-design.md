# Scoped read on lean — design of record

Status: **DESIGN.** Phases 0–2 are being implemented against it; phase 3
is a decision gate, and phases 4–5 are explicitly NOT approved work.
Written 2026-09-11, after the adversarial audit in
`lean/e2e/perf/results/scoped-read-write-audit-2026-09-11.md` and the
scan-split investigation that followed it.

The question that started this: *does lean really need to materialize
everything in the manifest just to make a small file change and write it
back?* The answer is no, and the change that makes it no is smaller than
the one first proposed. This document says which part is needed, which
part is not, and — the expensive half — what the part that is not needed
would cost if the use case ever changes.

## 0. The use case, stated precisely

A gateway or an agent holds a workspace of N files and edits k of them,
k << N. Concretely: 3 of 2001. Today `checkout()` materializes all 2001
before the agent starts, because it iterates the whole manifest
(`checkout.rs:254-256`) and the budget refusals above it
(`checkout.rs:209`) are computed over `m.entries.values()` — the whole
map. The agent pays the full tree in bytes, in wall-clock, and in disk,
to touch three files.

**Non-goal:** partial *reads* of a file. This is about which files are
materialized, never about which bytes within one.

## 1. What exists today

Three scope-shaped things, and none of them scopes a read:

1. `sync_scoped(Some(paths))` (`sync.rs:97`) — D4, the publish side. A
   **per-call argument**: it scopes one sync and persists nothing.
   Remote changes outside the scope are deferred to the inbox
   (`sync.rs:167`, `sync.rs:294`).
2. `Scope` / `Scope::covers` (`sync.rs:52-88`) — a normalized prefix set
   matched on component boundaries, so `"in"` never matches
   `internal/`. Capped at `MAX_SCOPE_ENTRIES = 64` entries of
   `MAX_SCOPE_ENTRY_LEN = 1024`.
3. `checkout()` (`checkout.rs:192`) — the whole manifest. No scope at
   all.

And one thing that matters more than it looks: **the marker records
nothing.** `write_marker` (`state.rs:202`) writes the literal bytes
`ok\n`. `checkout()` early-returns on `marker_present()`
(`checkout.rs:194`) forever after. There is no persisted answer to "what
is this workspace holding?", and nowhere to put one.

## 2. The two constraints the audit established

The audit's full verdict is in its own file; two findings are load-
bearing here and everything below is shaped by them.

**C1. The admission filter is the whole safety argument.** `classify`
(`scan.rs:105-131`) derives deletions by iterating
`baseline.entries.keys()` — not the manifest, not a census. A workspace
that never materialized 1,998 paths never cites them, so it can never
classify them absent, so it can never publish their deletion. `classify`
needs no scope awareness at all. This is why the scan split investigated
separately was refused: it was proposed to buy a safety property that
the admission filter already provides, it measured 0.94–1.10x across
four tree shapes, and it would have made `lstat` the primary delete
oracle — which is exactly the bug fixed in `14b3637c`.

**C2. `inst_base` must stay the WHOLE manifest.** `manifest.rs:996`
treats an entry absent from the installed base as changed. Narrow
`inst_base` alongside the baseline and all 1,998 unadmitted entries read
as foreign, queue into the inbox, and the next barrier downloads the
entire tree — defeating the feature. The fast path and the safe path are
the same choice here, which is the best kind of constraint.

Note also that `barrier.rs:843` rewrites `inst_base` every barrier, so
any argument that depends on its contents surviving a barrier is moot.

## 3. Scoped checkout

### 3.1 Admission

One filter, at the point where admission is already built:

```rust
// checkout.rs:254-256, today
let mut admission: Vec<(&String, &LeanEntry)> = m.entries.iter().collect();
```

becomes a filtered collect against the requested scope. The LPT sort
below it is unaffected — it already states that nothing downstream may
depend on admission order.

**The budget sums must move below the filter.** As shipped they run at
`checkout.rs:209` over the whole map. Left there, a 3-file scoped
checkout is refused for the 2001-file tree's size: a budget is a promise
about what this checkout will write, and after the filter it writes
three files.

### 3.2 The persisted scope, and its write order

A `scope.json` in the state dir, mirroring `load_baseline` /
`save_baseline` (`state.rs:213-224`) — same atomic-via-tmp write, same
compact encoding.

**It is written BEFORE the marker.** The marker is the agent-start gate
and is deliberately written last (`checkout.rs:578-579`). A scope that
landed after it would leave a window where an agent is cleared to start
against a tree whose admitted set is not yet durable — and after a
crash in that window the workspace holds three files and claims, by the
absence of any scope, to hold all 2001.

### 3.3 Refusals

Two, both of which exist because the alternative is a silent widening:

- **Marker present, persisted scope != requested scope → refuse.**
  `checkout()` early-returns on the marker, so honouring the request is
  not possible and ignoring it silently is worse: the caller asked for a
  set and got a different one with a success return. Naming the two sets
  in the error is the whole value.
- **All-rejected scope → refuse.** `Scope::new` silently drops malformed
  entries; a scope of entirely-malformed entries normalizes to empty,
  and an empty scope reads as "no restriction" = the whole tree. This
  rule already shipped on the sync side (`sync.rs`, commit `28be02b7`)
  and the checkout side must use the same rule and the same message
  shape — a typo must never widen.

### 3.4 What is deliberately NOT changed

- `classify` — see C1. No scope awareness, no new branch.
- `inst_base` — see C2. Whole manifest, always.
- `scan()` — unchanged; it walks the live tree, and a scoped tree is
  simply a smaller one.
- The four belt-and-braces scope filters the audit sketched at
  `barrier.rs:850`, `:128`, `:806`, `gated.rs:535/:1047`. One `covers`
  predicate at call sites if and when a defect argues for it; four
  copies of a rule is four places for it to drift.

## 4. The narrow / widen verb — NOT approved work

Recorded here because the cost of *not* having it is the part of this
design most likely to be misunderstood.

### 4.1 The workspace widens anyway, just not under your control

A path outside the scope that changes **remotely** arrives through the
inbox and lands in the tree; from then on it is cited and it is yours.
A path that never changes remotely can never be obtained. So a scoped
workspace's held set drifts by whatever the remote happens to touch, and
the one operation unavailable is *asking for a specific file*. That
asymmetry, not disk reclaim, is the argument for the verb.

### 4.2 Why `rm` is not a narrow

`classify` reads exactly the two states a narrow has to pass through:

| state | classify says |
|---|---|
| present in scan, absent from baseline | **upload** |
| present in baseline, absent from scan and `prev_scan` | **delete** |

So unlink-then-uncite crashes into publishing deletions, and
uncite-then-unlink crashes into re-uploading identical bytes and
silently re-citing everything just dropped. *Neither order is safe on
its own.* A narrow must remove a path from `entries` and `prev_scan` in
the same step it leaves the tree.

The invariant, stated for the model: **a narrow is an unwatch, never an
absence.**

### 4.3 The transaction

The existing intent journal (`state.rs:242-273`) is the mechanism, not a
belt-and-braces addition: persist the target scope as intent, replay on
startup, gate the barrier behind the replay. Replay is idempotent — it
removes citations not in the scope and unlinks tree paths not in the
scope, and running it twice is running it once.

Riding the boundary-verb surface (`Verb`, `sentinel.rs:65`) inherits
mutual exclusion with the barrier for free. A UDS-side or CLI-side
narrow would have to build its own, against a live sentinel.

A third `Verb` variant breaks exactly six exhaustive matches, all in
`sentinel.rs` (`:73`, `:79`, `:85`, `:96`, `:106`, `:770`) — compiler-
caught — plus the `Verb::` uses in `gateway.rs` (6), `inbox.rs` (2) and
`gauges.rs` (1).

### 4.4 Widen, and the manifest floor

Widen cannot reuse `checkout()`: the marker early-return at
`checkout.rs:194` is unconditional. The fetch loop (`checkout.rs:254-380`
— semaphore, LPT, ranged GET, permit clamp) lifts into a
`materialize(admission)` that `checkout()` calls with the whole manifest
and widen calls with the delta. That refactor is the largest diff in the
feature and it should land alone, with no semantics attached.

`manifest::load` (`manifest.rs:305`) fetches the pointer **and every
chunk**, regardless of scope. Scoping the read does not scope the
manifest load. So widen's floor is one whole-manifest load per call, and
the verb must therefore take a **set**, never a path.

## 5. Performance

Analytic unless marked measured. The audit ran nothing; its probes are
preserved as `lean/e2e/perf/results/scoped-audit-probes.patch` and phase
0 exists to run them.

**The win is entirely phase 2** and it is proportional: k of N admitted
means the checkout transfers k files. For 3 of 2001, that is the
feature, and the number to report is bytes-not-transferred, measured.

**Steady state also gets cheaper**, which is the underrated half.
`scan()` walks the live tree and `symlink_metadata`s every file; a
scoped tree is smaller, so every barrier shrinks with it — 3 stats per
barrier rather than 2001 — and `classify` iterates a 3-entry baseline.

**Carrying a scope costs nothing measurable.** `covers` is a linear scan
over <=64 entries with a `strip_prefix` each: worst case ~128k prefix
compares per barrier, against a barrier that already does a full readdir
walk plus a stat per file.

**Narrow** costs one baseline rewrite (already paid every barrier), N
unlinks, one scope write. No network. **Widen** costs one whole-manifest
load plus a materialize of only the added paths.

**The one cliff is C2**, and it is correctness-shaped rather than
perf-shaped: narrowing `inst_base` turns the next barrier into a
whole-tree download.

## 6. Phases, each with the control that makes it mean something

**Phase 0 — verify the audit's spine.** Apply `scoped-audit-probes.patch`
and run `probe_` on real Linux. The load-bearing one is the H1 mutation
control: mutate `manifest.rs:999` to `if false &&` and watch
`foreign_queued` go 1 -> 0. That is what establishes C2. The audit's two
"fix regardless" defects are already shipped (`14b3637c`, `28be02b7`).

**Phase 1 — refactor, zero behaviour change.** Lift the fetch loop into
`materialize()`. Nothing else in the commit. Control: the door drill,
n=3, arms interleaved — same ranges, or the refactor moved something.

**Phase 2 — scoped checkout.** §3, entire. Control: delete the filter
line and the 3-file checkout must materialize 2001. The positive control
must run the load-bearing line, not a paraphrase of it. Plus a real-
cluster leg for the bytes number.

**Phase 3 — a decision, not work.** The verb is justified only if a live
workspace's admitted set actually changes. Phase 2 in production answers
that empirically. If the gateway never needs a fourth file, §4 is
correctly never built.

**Phase 4 — the verb** (§4), only if phase 3 says yes. Mutation checks,
in order of what they would catch: drop the baseline-removal step and a
narrow must publish deletions; crash between intent and unlink must
converge, not delete; crash between uncite and unlink must not re-cite.
Plus `LeanSubtree.tla` — a Narrow action, the §4.2 invariant, and a
probe cfg that **fails** on the naive version. The abstraction has been
the bug three times in this repo; the model earns trust by catching a
deliberately-wrong narrow first, not by passing.

**Phase 5 — surfaces.** A CRD field (`flintleanworkspaces.yaml`, 468
lines, no scope surface today) and `lean_operator/crd.rs`. Adding a CRD
field is safe; removing one prunes it.

## 7. What this design refuses

- **The scan split.** C1: the safety it was proposed to buy already
  exists. Measured 0.94–1.10x, and worse on the cadence mass-delete
  path, where the absence check is an in-memory lookup against a map the
  walk already built.
- **Four scope filters.** One predicate, at call sites, on evidence.
- **Narrowing `inst_base`.** C2. Ever.

## 8. What nobody has opened

The CSI/node side under H1's refill; whether `LeanSubtree.tla`'s
`baseline`/`instBase` split can express a scope at all — given that
collapsing those two states is already a bug the model caught once, the
narrowing rule is exactly what to prove there first.

Unrelated but open, from the same drill: the publish path reads every
file **twice** (`upload_compose` hashes the whole file, then re-reads it
to upload) — roughly 11 s of the `mixed` workload's 24.3 s. The fix is
per-part CRCs combined by length.
