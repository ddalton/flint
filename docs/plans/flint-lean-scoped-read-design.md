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

## 4. The narrow / widen verb — SHIPPED

Approved by the user 2026-09-11 and built as `Sidecar::rescope`
(`checkout.rs`), with the CLI surface `flint-sync rescope <paths…> |
--all`. §4.1–4.4 below are the design as written before the build; §4.5
records what building it changed.

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

### 4.5 What the build changed

**The intent carries the DROP SET, not just the target.** §4.3 said
"persist the target scope as intent, replay on startup" and called the
replay idempotent. It is not, with only the target: replay re-derived
the set to unlink from `baseline.entries`, and after the uncite those
entries are gone — so a crash between the uncite and the unlink left
six uncited files on disk forever, and the replay reported
`uncited 0, unlinked 0`. The design's own third mutation check caught
it on the first run.

Deriving it from the TREE instead is worse, and the reason is the same
indistinguishability §4.2 is about: a present, uncited, out-of-scope
path is either a leftover from a crashed narrow or **a file the agent
just created**, the two have opposite correct answers, and no existing
state tells them apart. So `ScopeIntent` records `{target, drop}`, and
`drop` is read back verbatim — never recomputed.

**Two postures, deliberately.** The DOOR (`rescope`) refuses when a
path leaving the scope has unpublished local changes, names them, and
leaves the old scope untouched — nothing is written, so the caller can
publish and retry. The REPLAY cannot be that strict: the intent gates
every barrier and only a successful apply clears it, so a refusing
replay would wedge the workspace. It KEEPS such a path — cited, on
disk, with a `rescope-kept-locally-dirty` conflict record — and
converges. A narrow may unwatch a file; it may never discard an edit.

Dirt is judged only over paths that are STILL CITED. A path a crashed
run already uncited reads as dirty by the ordinary rule (present in
scan, absent from the baseline ⇒ `uploads`), and keeping it would undo
the very step that crashed.

**The barrier replays at step 0**, before the scan, not at startup.
A half-applied narrow is a tree whose files and citations disagree, and
`classify` reads that disagreement as an upload or as a DELETE
depending on which half landed — so the window has to close before the
scan, not before the process. Cost on the normal path is one `stat` of
a file that is not there. `BarrierReport::rescope_replayed` says when it
fired.

**Order within the apply still matters**, for the window where the
intent itself is lost: uncite first, so the failure mode is a file that
reads as a local add (re-uploaded, recoverable) rather than one that
reads as a local delete (published as a DELETE, not recoverable).

**Mutual exclusion came free, and not from the boundary-verb surface.**
§4.3 argued for riding `Verb` to inherit it. `SidecarState::open` takes
an exclusive `flock` on the state dir, so `flint-sync rescope` against a
tree a `run` loop holds is already refused by name. The CLI surface
needed no new locking.

### 4.6 What is NOT built

- **No agent-facing sentinel.** A third `Verb` variant + `.flint/rescope`
  is still the way an agent asks for one, and it is unbuilt. Today the
  verb is reachable from the CLI only.
- **No gateway verb, and this one is a REFUSAL rather than a gap.**
  D14's argument against performing a remote's `sync` applies to a
  rescope with more force: a rescope unlinks local files by scope, so
  honouring one on a remote's say-so would upgrade what a leaked
  gateway bearer can do to "delete across a running agent's tree, at my
  timing, under a scope I choose". If the UI needs to pull a file in, it
  must be CARRIED to the agent as advisory news, exactly as
  `sync-request` is — never performed.
- **Phase 5's CRD field**, unchanged: still not there.
- ~~The TLA+ Narrow action.~~ **BUILT — see §4.8.**


### 4.8 The model tranche — and the half of the rule it CANNOT check

Added to `LeanSubtree.tla`: `prevScan` and `scope` as fields of the `sc`
record (so no existing `UNCHANGED` tuple moved), the constants
`TwoScanDelete` / `MaxNarrows` / `NarrowAtomic` / `NarrowUnlinkFirst`,
a `Narrow` action, `Inv_NarrowNeverDeletes`, `Inv_NarrowNeverRecites`,
`ProbeNarrow`, and four cfgs wired into `check.sh` — **83 runs, was
79**. (The prose count in that file said "fifty-five" and its printed
denominator said 79, so the gate had been reporting `83/79 green`: a
line nobody reads as wrong. The total is now ASSERTED, and the
assertion was watched to fail on a deliberately short run.) All four new constants are FALSE/0 in every pre-existing cfg — 67
files gained four lines each and NOTHING else, so earlier state spaces
are preserved by construction. Turning `TwoScanDelete` on globally would
SHRINK every earlier run's delete space, and a pinned mutation that
stops finding its counterexample is the failure this harness exists to
prevent.

**The model earned its green by catching both wrong narrows first:**

| run | required | result |
|---|---|---|
| `LeanNarrowUnlinkFirst` | `Inv_NarrowNeverDeletes` violated | violated ✓ |
| `LeanNarrowUncieFirst` | `Inv_NarrowNeverRecites` violated | violated ✓ |
| `LeanProbeNarrow` | `ProbeNarrow` violated (the action fires) | violated ✓ |
| `LeanNarrowHolds` | green | **green, 10,179 distinct states** |

**THE LIMIT, MEASURED — do not read the green run as more than it is.**
The shipped rule has three conjuncts: a narrow removes the path from
`entries`, from `prev_scan`, and from the tree in one step. **The model
checks two of them.** `Dirty(s)` is `local[p] # baseline[p]`, so
`baseline[p] := 0` alone already makes the path undirty and `prevScan`
cannot change any classification. Deleting the `prevScan` removal from
the atomic arm leaves `LeanNarrowHolds` **green over 10,823 distinct
states**. In `scan.rs` the two are separate structures and `classify`
consults both, so the code carries a conjunct this abstraction
collapses.

The model is therefore WEAKER than the code here. That is the safe
direction for a miss and the wrong direction for a claim, which is why
it is written at the line itself and not only here.

### 4.9 §8's question, answered — and my first answer was too strong

§8 asked whether the `baseline`/`instBase` split can express a scope at
all. Reading the variable declarations, the answer looked like a flat
no: `prevScan` did not exist anywhere in 2,040 lines, and `baseline` is
a TOTAL function `[Paths -> Nat]` whose `0` conflates *never held*,
*held but absent*, and *dropped by a narrow*.

**Building it showed that was overstated.** The conflation is benign
for this invariant precisely because `Dirty` compares `local` to
`baseline`: a correctly narrowed path is `0 = 0` and therefore in
neither the upload nor the delete set, while each naive order leaves
exactly one side at `0` and is caught. So the split DOES carry a scope
well enough to check the narrow's atomicity — and it does NOT carry
enough to check the `prev_scan` conjunct, which needed `prevScan` added
and still cannot be made load-bearing without changing `Dirty` itself.

The precise answer: **two of the rule's three conjuncts are
machine-checked; the third is carried by the code and by
`a_narrow_leaves_the_merge_base_whole` plus the three Rust mutation
checks in §4.7, and by nothing in TLA+.**

### 4.7 The mutation checks, run

Each drops one line and the named test must fail:

| mutation | test that must die | result |
|---|---|---|
| drop `baseline.entries.remove(p)` | `a_narrow_unwatches_without_publishing_a_single_deletion` | FAILED ✓ |
| drop the barrier's step-0 replay | `a_crash_between_the_intent_and_the_unlink_converges` | FAILED ✓ |
| re-derive the drop set from the baseline | `a_crash_between_the_uncite_and_the_unlink_does_not_re_cite` | FAILED ✓ |

Plus the anti-vacuity arm the first one needs:
`the_same_unlink_without_the_uncite_publishes_deletions` removes the
same six files WITHOUT the uncite and asserts the barrier publishes
six deletions. Without it, "a narrow published no deletions" would pass
against a delete rule that never bites.

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
