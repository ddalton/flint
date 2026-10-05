# What lean guarantees about your bytes, and what it does not

This is the safety claim for the lean protocol: the syncer (`lean/syncer`),
the gateway (`lean/gateway`) and the store contract they rest on
(`crates/flint-store`). It exists because "116 model runs are green" is not
a claim anybody can act on. A claim names the property, the thing that
enforces it, the worlds it was checked in, and — the part usually missing —
what is still assumed and what is not covered at all.

Read it with two companions: `lean/formal/COVERAGE.md` (generated: which
invariant is checked in which world) and `lean/FINDINGS.md` (every defect
found so far, with how it was found).

Nothing here is a proof. Every check below is exhaustive over a SMALL
world, or a replay of a run that actually happened. §5 says what that
leaves open.

## 1. The promises

**S1. An acknowledged write is never lost.** Two kinds of acknowledgement:
an agent's publish ack, and the gateway's answer to a UI write.

| | enforced by | in code |
|---|---|---|
| an ok ack means the boundary is INSTALLED, at the seq it names — and that seq carries the declaration; a declared path it does not carry (withheld, deferred, or deleted by a peer after the install that carried it) makes the ack `partial` and is named | `Inv_AckImpliesCited` | `sentinel.rs`, ack after `note_boundary`; `IntentJournal::carrier` and `Carrier::uncited`, recomputed at every consume (H10, §4.15, §3.2) |
| the boundary it names cites everything this workspace integrated | `Inv_AckBoundaryCoherent` | the scan publishes every path that differs from the baseline, and the baseline IS the merge base (P1-lite, §3.2); the citation repair that did this before step 5 is gone |
| a fenced writer never answers ok | `Inv_NoFencedOkAck` | `refuse_if_read`, `verify_not_deposed` |
| a consumed publish request is always answered | `Inv_NoNonceOrphan` | the pending record, `sentinel.rs` |
| the ack and the bucket name the same clock | `Inv_BoundaryNamesItsClock` | `boundary_source` stamped on the install |
| an acked UI write is never destroyed unrecorded | `Inv_HITLDurable` | a UI save is acknowledged only once its CAS landed over the version it read (a refused one is a 412 and records nothing, P2); from then on it is a published version, and R7 and M3 preserve it (`preserve_conflict_copy`) |
| an acked UI write stays tracked until a writer RETIRES it | `Inv_HITLTracked` | since step 5 a UI write is CITED when it is acknowledged (P2): tracked by the document itself. Before it: the inbox, the writer queue, the untracked sweep, and the collector's decision (§4.4) |

**S2. A citation always resolves to the bytes it names.** A reader that
follows the manifest never gets a hole or the wrong version.

| | enforced by |
|---|---|
| every cited path has a live object | `Inv_NoDangling` |
| ~~on a versioned bucket, the exact cited version is still stored~~ retired 2026-09-19 (§3.1, H9: a gated-lane claim over state the code never had; under handles a citation names an immutable object, `Inv_NoDangling` / the core's `Inv_CitationsLive`) | ~~`Inv_CitedVersionLives`~~ |
| ~~the reaper never takes the version a path currently reads as~~ retired 2026-09-19 (§3.1, H9); under handles the collector spares what the installed document cites and the sweep what is cited or named, `Inv_NoDangling` / `Inv_CitationsLive` | ~~`Inv_NoUncitedGC`~~ |
| a commit never cites its own upload over bytes the key no longer holds | `Inv_NoStaleOverride` |
| a reader that loaded the manifest less than the retire age G ago can fetch every object it names (M1, step 5: a commit logs what it stops citing under `.flint/lean/retired/`, and nothing a log younger than G names is deleted; `RETIRE_GRACE_SECS` = 600, `FLINT_SYNC_RETIRE_GRACE_SECS`) | `Inv_ReaderFetches` (`LeanP1.tla`) |
| a gateway read returns the bytes the citation's CRC names, or refuses (502 `corrupt`; M8) | tests only (`a_read_whose_bytes_do_not_match_the_citation_is_refused`) |

**S3. A concurrent write is never dropped silently.** Where two writers
disagree, both versions survive: one in the tree or the manifest, the other
as a conflict copy with a record naming it.

| | enforced by |
|---|---|
| no install drops the last tracked reference to acked bytes | `Inv_HITLDurable` |
| `sync` never destroys genuinely dirty local work | `Inv_SyncNeverDestroysDirty` |
| a merge base never advances past a change neither integrated nor surfaced | `Inv_NoForeignLost`; since step 5 the merge base IS the baseline and what the tree is owed is derived from the document at each consume (`Inv_ShortcutSound`, §3.2) |
| an agent's delete over a version a peer or the UI changed applies, and that version is preserved under a record (`commit-deleted-over-theirs`; M3, step 5) | `Prop_DeleteSettles`, `Inv_AckedNamed` (`LeanP1.tla`) |
| a delete or rename asked for from outside removes the version it was asked about, never a newer one | `Inv_RemovalNamesItsVersion` (review 2026-09-18, H4); since step 5 it is one CAS on the current document, judged against `if_match` when given (`FileChanged`, 412) |

**S4. A deletion is never resurrected; a rename is one generation.**

| | enforced by |
|---|---|
| a restart never republishes a delete the agent made | `Inv_NoResurrection` |
| a rename is never visible under both names, and never under neither | `Inv_RenameAtomic`, `Inv_RenameNoHole` |
| narrowing a workspace unwatches paths, it does not delete them | `Inv_NarrowNeverDeletes`, `Inv_NarrowNeverRecites` |

**S5. One writer commits at a time, and a fenced writer cannot write.**

| | enforced by |
|---|---|
| one commit section at a time among recognised holders | `Inv_CommitExclusive` |
| the cell's holder is the writer that believes it holds it | `Inv_CellHeldByHolder` |
| a deposed writer's manifest CAS never lands | `Inv_NoStragglerInstall` — on the shipped fence position since 2026-09-19 (`FenceAfterLoad`, H2, §4.15) |
| a deposed writer's data PUT never lands | `Inv_NoDeposedPut` |
| ~~a boundary is all-or-nothing~~ retired 2026-09-19 (§3.1, H9: a gated-lane invariant); under the barrier lease a boundary IS one pointer CAS, the merge's whole result or nothing (`EmptyInstall`), which no invariant needs to say twice | ~~`Inv_BoundaryAtomic`~~ |

**S6. Every tree eventually equals the published boundary.** Convergence,
not safety: `Inv_QuiescentConverged` — once nothing can move, every object
at a cited key is that citation or is tracked for a writer to integrate.

## 2. What the protocol ASSUMES

A claim with unnamed assumptions is a claim about nothing.

| assumption | how it is verified | if it is false |
|---|---|---|
| the store honours `If-None-Match` / `If-Match` on PUT (every upload, the manifest CAS, the lease cell) | probed by the syncer before its first verb (`conformance.rs`), and by `flint-sync probe-conditional` | **no guarantee holds** — arbitration degrades silently to last-writer-wins. The syncer now REFUSES the workspace (`EXIT_REFUSED`) rather than run on such a store |
| the store honours `If-Match` on DELETE (the file collector, and only it) — **FALSE on two of the three stores we actually run against**: S3 enforces it; Ozone 2.2.x (L-27) and MinIO (L-102, measured 2026-09-15) both accept the header and ignore it | the same probe | the collector could take the version another writer's commit is about to cite — precisely the model's refuted `LeanBarrierLeaseGCUnconditional`. The syncer now turns the COLLECTOR off instead of refusing the workspace: retired objects are left in the bucket, cited by nothing (`leaked=` on the barrier line, one warning per barrier). Ozone 2.2.x is this case (HDDS-14907, L-27) — and so is **MinIO** (RELEASE.2025-09-07, measured: it enforces `If-Match` on GET and `If-None-Match` on PUT, and ignores `If-Match` on DELETE — `lean/e2e/results/minio-conditional-delete-2026-09-15.md`), which is most of the e2e rig. The loss becomes storage growth. **Until 2026-09-18 it was worse than a cost:** the leaked object superseded the peer's own tombstone in every other writer's consume (the rule looked at the key, not at what the delete retired) and their citation repair re-cited it, so one `rm` on a two-writer workspace flapped forever (review 2026-09-18 H1, `LeanBarrierLeaseLeakResurrects`, `Inv_NoDeleteResurrected`). The tombstone now carries the etag the delete retired and applies over it |
| an etag names the bytes (a content hash), so identical bytes share one | modelled (`MaxSameBytes`), which is what makes finding 13 reachable | the same-bytes findings would not apply; a different set would |
| the local filesystem gives atomic rename and honest `lstat` | assumed | the scan's two-scan rule and the temp-then-rename writes lose their basis |
| exactly one syncer owns a workspace tree on a node, and nothing else edits it mid-barrier | the CSI driver's worker-per-volume | a scan can publish a half-written file |
| clocks are never used for ordering | by construction: the cell's epoch and the manifest's seq order everything | — |

## 3. How the claim is checked today

| evidence | what it covers | size |
|---|---|---|
| the formal gate (`lean/formal/check.sh`) | every invariant `COVERAGE.md` lists, exhaustively, per world. Which promises are checked on the SHIPPED shape is §4.11; the three enforcers no run checked are retired, and what stands in their place is the core's `Inv_CitationsLive` (§4.12) | 182 runs: 51 strict worlds, 74 mutations, 57 probes (the chunk-module runs among them, the core model's own thirteen gate worlds, and its refinement of the queue and probe worlds; `formal/README.md`'s recipe). Twenty-six of them are the handles arm (`ImmutableObjects` on `IMPL`: the shape the code has had since 2026-09-19, §3.1), thirteen are the core model — the shipped shape with one rule removed at a time, its other four worlds needing a third path and tens of millions of states, so they are decided on the box — two are the core's refinement of it, and the other 141 are the slot shape the code had before (`docs/plans/flint-lean-immutable-objects-design.md`) |
| refutation | that an invariant CAN fail — a mutation that must violate it | 74 mutations; every invariant `COVERAGE.md` lists has at least one (`refuted by`), the core's four included; the three of §4.12 are retired rather than left unrefuted |
| trace validation, phase 1 | the model is the code, on 5 scenario traces, in CI | 5 accepted, 5 mutations + 5 controls rejected |
| trace validation, phase 2 | the model is the code on a REAL 6-writer run on S3, with the invariants checked while replaying | one leg, one path: 3,258 steps, no invariant violated |
| the live drill | the binary is the code: each fix has a control arm that fails | host legs H1-H6, storm legs S0-S5 (2026-09-15) |
| trace validation on the shipped shape (step 5) | the code's conformance traces are behaviours of `LeanP1.tla` (`formal/trace/TraceCore.tla`) | 41/41 on 2026-09-25: 15 traces accepted, 16 mutations and 10 controls rejected |
| the unit battery | each finding pinned by a test whose control fails it; every step-5 line mutation-checked | 257 syncer tests; gateway 24 unit, 31 verbs, 5 battery (2026-09-25) |

### 3.1 What certifies each promise, on which shape (H8/H9, 2026-09-19)

**Superseded as the description of the shipped shape by §3.2 (step 5,
2026-09-25).** This table is the handles shape before step 5 — the inbox
cell's entries, the writer-local queue, citation repairs — which the code
no longer has. It stays as the record of what was checked on that shape.

The code that ships is the handles realisation (§5, H6: built 2026-09-19).
Its model is `LeanSubtree.tla`'s `ImmutableObjects` arm on the `IMPL`
constants — the twenty-six handles runs — and the core model
`LeanCore.tla`, which the handles worlds refine (`LeanRefine.tla`, §3). The
other 141 runs model the SLOT shape the code had until that day: still the
only worlds that carry the ack, sync, narrow and convergence invariants,
and so the only checks those rows have. Read the table as three columns of
evidence, weakest first: a row certified by nothing, a row certified on the
previous shape only, a row certified on the shipped shape — and, where the
core states the same claim over state, the core's name for it.

| promise (§1) | invariant | on the shipped shape (handles worlds) | the core's claim (through the refinement) | on the previous shape only |
|---|---|---|---|---|
| S1 an ok ack means the boundary is installed, at the seq it names | `Inv_AckImpliesCited` | since 2026-09-23: `LeanImmutableSentinelImpl1` (one path, the sentinel on the code's shape with handles, every ack invariant; 56,491,448 distinct, depth 40); mutation `LeanImmutableSentinelOutrankedOk` fires (10,645,182 distinct) | — (the core has no ack) | the sentinel worlds, `BarrierLease = TRUE` but slots |
| S1 the boundary cites everything integrated | `Inv_AckBoundaryCoherent` | since 2026-09-23: `LeanImmutableSentinelImpl1` (one path, the sentinel on the code's shape with handles, every ack invariant; 56,491,448 distinct, depth 40); mutation `LeanImmutableSentinelQueueDropped` fires | — | the sentinel worlds |
| S1 a fenced writer never answers ok | `Inv_NoFencedOkAck` | **cannot fail on this shape — nor in ANY barrier-lease world** (2026-09-23): `DeposedHolder` under the barrier lease needs the writer inside its commit section, and an ack is written only at idle, so the stamp is never set (r47: the refusal off, a stall world with handles, 561,039 distinct, clean). Every barrier-lease world that lists it certifies nothing by it — review 2026-09-18's L3, reconfirmed here by a run rather than by reading. What the promise means here — a deposed writer's ok names no uninstalled boundary — is `Inv_AckImpliesCited`: the ack follows the writer's OWN install, and a deposed writer's CAS is fenced | — | the life-lease sentinel worlds (`LeanSentinelFencedAck` fires there) |
| S1 a consumed publish request is answered | `Inv_NoNonceOrphan` | since 2026-09-23: `LeanImmutableSentinelImpl1` (one path, the sentinel on the code's shape with handles, every ack invariant; 56,491,448 distinct, depth 40); the clobbering consume (`FoldPending = FALSE`, `LeanImmutableSentinelOrphan`) violates it in 1,076 distinct | — | the sentinel worlds |
| S1 the ack and the bucket name the same clock | `Inv_BoundaryNamesItsClock` | since 2026-09-23: `LeanImmutableSentinelImpl1` (one path, the sentinel on the code's shape with handles, every ack invariant; 56,491,448 distinct, depth 40); mutation `LeanImmutableSentinelUnstamped` fires | — | the sentinel worlds |
| S1/S3 an acked UI write is never destroyed unrecorded | `Inv_HITLDurable` | every handles world (`IOINV`) | `Inv_AckedNamed`: an acknowledged handle that is gone is named by the state — a conflict record, a citation or tombstone derived from it, a later acknowledged write at its path, or a tree that integrated it and deleted or still derives from it | the slot worlds too |
| S1 an acked UI write stays tracked until a writer retires it | `Inv_HITLTracked` | every handles world | `Inv_AckedNamed` (the same harm, over state instead of the collector's stamp) | the slot worlds too |
| S2 every cited path has a live object | `Inv_NoDangling` | every handles world | `Inv_CitationsLive` | the slot worlds too |
| S2 the exact cited version is still stored (versioned bucket) | `Inv_CitedVersionLives` | — | — | **nothing**: a gated-lane invariant over state the code never had; retired from §1 with this table (H9) |
| S2 the reaper never takes the version a path reads as | `Inv_NoUncitedGC` | — | `Inv_CitationsLive` says it for handles: a cited handle is never collected (the collector spares what the installed document cites, the sweep what is cited or named) | **nothing** on the slot shape; retired (H9) |
| S2 a commit never cites its upload over bytes the key no longer holds | `Inv_NoStaleOverride` | every handles world (the ghost's handles form: an upload or a repair cited over a version the tree never integrated, with no record) | R7 is a rule constant of the core (`CommitSurfacesForeign`) whose mutation world violates `Inv_AckedNamed`; the peer-vs-peer half has no state form yet | the slot worlds too |
| S3 `sync` never destroys dirty work | `Inv_SyncNeverDestroysDirty` | since 2026-09-23: `LeanImmutableSyncHolds` (the scoped sync on the code's shape with handles and a UI write; 2,497,753 distinct, depth 34), and its mutation `LeanImmutableSyncStaleDirt` fires | — | the sync worlds, slots |
| S3 a merge base never advances past a change neither integrated nor surfaced | `Inv_NoForeignLost` | held in `LeanImmutableSyncHolds`, but **not refuted there**: its mutation (`LeanImmutableSyncOverlayStale`, the overlay route) cannot fire on the code's shape — the writer-local queue took peers' changes out of the inbox the overlay reads (bisected 2026-09-23: fires with `WriterQueue = FALSE`, clean with it). `SyncKeepsHiddenBase` is not shown load-bearing there | — | the slot worlds |
| S3 a removal removes the version it was asked about | `Inv_RemovalNamesItsVersion` | the rename world | — (the core's `over` and `handle` on a removal record carry the rule; no state invariant yet) | the removal worlds, slots |
| S2/S6 a peer's published version is never reverted without a record | `Inv_ConsumeNeverRegresses` (2026-09-23) | — in the gate yet: sandbox `LeanImmutableSyncQueueHolds` holds (2,497,941 distinct) and `…Regress` fires as shipped; **every other invariant was blind to the revert** (2,522,269 distinct clean), which is how L-123 shipped. Waits on the module edit (`formal/pending/2026-09-23-sync-queue-overtaken-L123.patch`) | **`Prop_NoSilentRevert`** (no lost update, an action property over `base` and `conflicts`); R7 off violates it (`LeanCoreCommitBlindReverts`, 2,863,351 distinct); read through the refinement as `CoreNoSilentRevert` | — |
| S4 a restart never republishes a delete the agent made | `Inv_NoResurrection` | listed in every handles world, but a tautology there (the ghost is written only under the mutation constant) | out of scope: the core has no restart | the same tautology; open: an action property that no step but an edit, a consume or a checkout creates a local file (H9) |
| S4 a rename is never under both names, never under neither | `Inv_RenameAtomic`, `Inv_RenameNoHole` | the rename world | `Inv_OneName`, unconditionally — and in every handles world, not only the rename one | the removal worlds, slots |
| S4 narrowing unwatches, never deletes | `Inv_NarrowNeverDeletes`, `Inv_NarrowNeverRecites` | — : the module ASSUMEs `MaxNarrows = 0` under handles, and the verb ships. Lifted in a sandbox (2026-09-23) both invariants were WRONG for two writers — a narrow is one workspace's scope, and they fired on a peer's legitimate upload and delete; restated per writer they hold (`LeanImmutableNarrowHolds`, 2,676,330 distinct, depth 34) with both mutations firing on both shapes. Waits on the module edit (`formal/pending/2026-09-23-narrow-over-handles.patch`) | — | the narrow worlds, slots |
| S5 one commit section at a time | `Inv_CommitExclusive` | every handles world | `Inv_OneHolder` | the slot worlds too |
| S5 the cell's holder is the writer that believes it holds it | `Inv_CellHeldByHolder` | every handles world | `Inv_OneHolder` | the slot worlds too |
| S5 a deposed writer's manifest CAS never lands | `Inv_NoStragglerInstall` | every handles world | out of scope (crash-free: no deposal) | the slot worlds too |
| S5 a deposed writer's data PUT never lands | `Inv_NoDeposedPut` | every handles world | out of scope | the slot worlds too |
| S5 a boundary is all-or-nothing | `Inv_BoundaryAtomic` | — | — | **nothing**: gated-lane; retired (H9) |
| S6 every tree eventually equals the boundary | `Inv_QuiescentConverged`, `Inv_TreesConverged` | `Inv_TreesConverged` in the leak world only; `Inv_QuiescentConverged` deliberately not under handles (§3's note) | — (convergence is not a state predicate) | the slot worlds |

What the table says plainly: the ack rows, the sync row, the narrow row and
the convergence rows are certified on a shape the code no longer has; the
three H9 rows were certified on no shape and are now retired from §1's
tables as promises (the reader promise the code keeps is stated in item 12);
`Inv_NoResurrection` is a tautology on every shape until it is restated.
The handles rows are certified on the shipped shape by exhaustive runs of
small worlds, and three of them are also the core's state-based claims,
carried over by the refinement.

### 3.2 The shipped shape since step 5 (2026-09-25)

Simplification step 5 changed the protocol under every row above. Its
model is `formal/LeanP1.tla` (`PROTOCOL.md` is written from it):

- **P2.** The gateway COMMITS each UI verb: a save, a delete, a rename or
  a folder of them is one manifest CAS, judged against the version the UI
  read (a save that lost is a 412 and records nothing). The gateway
  deletes no object and never waits for the writers' lease (G1). The
  inbox cell's entries and declared removals are gone; the cell carries
  only the two verb requests.
- **P1-lite.** A writer's baseline IS its merge base. What its tree is
  owed is DERIVED from the document at each consume — the document
  differs from the baseline, the tree is clean there, and the path is held
  or in scope — never stored. The writer-local queue, its L-123 prune,
  L-126's parked base and the journal's installed document are gone. A
  consume whose pointer names the document it last derived against, and
  whose skipped paths (the agent's work) are all still dirty, derives
  nothing: one pointer GET (the scan trigger, `derived_etag` / `skipped`;
  only the consume writes it, and the commit advances it only over the
  derived document). `Inv_ShortcutSound` is that claim. A restart after the CAS settles by
  content convergence (the tree's bytes ARE the document's), not by a
  journal.
- **M3.** An agent's delete over a version theirs changed applies, and
  theirs is preserved under `commit-deleted-over-theirs`. No delete is
  outranked any more (`Prop_DeleteSettles`).
- **M1, the retire age G.** Every commit, a writer's or the gateway's,
  logs what it stopped citing (`.flint/lean/retired/`); the sweeps spare
  anything a log younger than G names, and a writer reaps a log once it
  is G old, sparing a handle cited again. A superseded manifest
  generation is kept until its successor is G old. G defaults to 600 s
  (`FLINT_SYNC_RETIRE_GRACE_SECS`). `Inv_ReaderFetches`: a reader that
  loaded the document less than G ago can fetch everything it cites. Not
  covered: a crash between the CAS and its log (those handles fall to the
  write-age rule), and the gateway's lease-held `cas_manifest`, which logs
  nothing.
- **M8.** The gateway caches the manifest by the pointer's etag and
  checks each read's bytes against the citation's CRC (502 `corrupt`).
  Tests only; no model.

What checks it: the trace check replays the code against `LeanP1.tla`
(41/41 at f6f6a892, §3), and the model's own gate (`gen-leanp1.sh`,
expectations in `WORLDS-LeanP1.tsv`, written before any run). The gate
**passed 2026-10-04**: 49 of 49 decided worlds as expected — 9 claims
hold, 19 mutations and 21 probes fire, 0 mismatches
(`formal/results/2026-10-04-leanp1-gate/SUMMARY.txt`). LeanP1Holds holds
at full bounds (1,669,205,616 distinct, depth 39;
`formal/results/2026-10-03-ec2-deep/`), and so do the two worlds that had
never completed: AllHolds (reader + scope + a rescope + a restart, 756M
distinct) and DeleteOverrideOff (1.69B). Its first run (2026-09-25) had
reported `Inv_ShortcutSound` violated in both shipped worlds; that was
settled before this gate. **What that run does not give:** the three
large HOLDS verdicts are from tlc-rs alone (the compiled checker, main
129adc8b) — TLC was waived for LeanP1Holds and not run for the other two;
where both ran (`Holds1p3b`) the distinct counts agree exactly. Liveness
holds only at a reduced bound (`LiveHoldsSmall`: one path, no removals;
the full `LiveHolds` cannot finish). The two RECORD worlds were stopped
then and decided on 2026-10-05 (`formal/results/2026-10-05-ec2/`):
NoConvergence HOLDS (652,173,065 distinct), so content convergence is an
economy, not a safety rule; CollectorGreedy is LeanP1Holds' world state for
state (`CollectorSparesCited` is read only where `RetireAge` discards it),
and even with the retire age off the guard changes no reachable state —
NoAgeNoReader and CollectorGreedyNoAgeNoReader both hold at 713,160,844
distinct, identically, because the retired set is what the commit stopped
citing. "Spare what the install cites" is redundant by construction in the
model; whether a crash or restart path in the code needs it is not modelled.

The model gained scope and rescope on 2026-10-01 (4da62d4e; it found
L-131, a narrow that unlinked the agent's write), a failed fetch
(978d910a) and a reader (694248f5); the scope filter is no longer pinned
by tests alone.

**Writers in this model.** Every LeanP1 world has at most TWO syncers
(`Writers = {A, B}`) plus the gateway, whose UI verbs (`GPut`/`GCas`, a
delete, a rename) are a third party writing the same document. The UI
is not a third syncer: it holds no tree, takes no lease, never consumes
and never sweeps, so what only a third syncer exercises — a lease passed
among three, a sweep while two others hold handles, a conflict naming
two losers — was unmodelled here until 2026-10-05: three-syncer worlds at
Holds1p3b's invariants (one path, no syncs, one copy) HOLD at three rungs —
449,595 and 39,330,288 distinct (Mac), and 1,421,211,248 at MaxMint 3, a UI
save and two barriers (EC2, depth 44). The next rung (three barriers) died
for memory at 983M distinct with no violation. A third syncer multiplies
these worlds 27-133x, so the full Holds bounds with three syncers are out
of reach. §4.2's three-writer world is the pre-step-5 model
(`LeanSubtree.tla`).

A behaviour change that follows from the scope filter: a scoped tree no
longer receives a peer's change, or a UI promote, outside its scope.

## 4. What is NOT claimed

1. **Unboundedness.** Every world is small — one to three writers, one or
   two paths, two barriers, a handful of generations. TLC exhausts the
   world, not the protocol. There is no inductive proof, so nothing here
   rules out a failure that needs a fourth writer or a third path.
2. **Three writers, only in one world — and only in the OLD model.** The
   LeanSubtree gate runs a three-writer world (2026-09-15); it carries 9 of
   its 21 invariants. The shipped shape's model (`LeanP1.tla`, §3.2) has no
   three-syncer world at all: two syncers and the gateway's UI verbs, which
   commit but do not hold, consume or sweep.
3. **Refutation is now complete, and that is recent.** Every invariant in
   §1 has at least one mutation that must make it fail. Until 2026-09-15
   two did not (`Inv_CommitExclusive`, `Inv_CellHeldByHolder`), and their
   nine green worlds each proved nothing.
4. **`Inv_HITLTracked` judged supersession by DESTRUCTION; repaired
   2026-09-16 to judge it by the collector's DECISION.** The clause for
   "legitimately superseded" was `objects[p] # gen` — the object is
   GONE. That is not what retires a write. A writer retires it by
   recognising the bytes and taking the path out of its boundary; whether
   the store can then carry the delete out is the store's business. So on
   a store without a conditional DELETE (§2), where the collector gives
   way and destroys nothing, the invariant flagged every leaked object
   forever — and in the crash world it flagged a state the code recovers
   from, a checkout adopting an object newer than its citation.

   Both arms now read the mechanism instead of the rubble: retirement is
   recorded where the collector DECIDES, and bytes sitting at a path the
   manifest still cites count as tracked — which is exactly the untracked
   sweep's condition and the checkout's S3-wins arm. The delete branch
   was deliberately left alone: after a real delete `objects[p] = 0` and
   the first clause already covers it, so widening a sticky excuse there
   would have been a relaxation nothing needed.

   The three known-bad worlds that require this invariant to fail still
   find it violated (`LeanEarlyInboxDropLosesHitl`,
   `LeanEarlyInboxDropLosesRename`,
   `LeanBarrierLeaseQueueTombstoneOverHitl`) — the repair kept its teeth.
   `LeanBarrierLeaseCollectorOff` now carries all ten invariants
   exhaustively (19,526,764 states, 6,802,540 distinct; since the
   2026-09-18 review it carries eleven, with `Inv_NoDeleteResurrected`, at
   33,982,392 states, 12,447,478 distinct, depth 39 on the evening's final
   arms — 12,325,522 before H1c–H1f), so the
   collector-give-way design no longer has a recorded exception, and the
   run that pinned one (`…CollectorOffHitlTracked`) is gone.

   The second arm is gated on `OrphanTrack`: a world with no sweep has no
   recovery to credit, and crediting one there would be the relaxation
   this clause is trying not to be. **Open, and load-bearing for how the
   sweep is described:** the crash world
   (`LeanBarrierLeaseSentinelImplCrash1`, a manual world — not in the
   gate) sets `OrphanTrack = FALSE`, so this arm deliberately does not
   excuse it and it should still violate. If it holds at `OrphanTrack =
   TRUE` and violates at `FALSE`, then the untracked sweep is load-bearing
   for S1 in that world rather than the churn control that §3's
   `LeanBarrierLeaseOrphanTracked` note calls it — which is a change to
   what the sweep IS, not a footnote. Both arms were run on 2026-09-16
   (`results/2026-09-16-crash1-orphantrack/`, commit `a42c3d40`): at
   `OrphanTrack = FALSE` the invariant is VIOLATED as it should be (66M
   states, 20 steps); at `TRUE` the run was INCONCLUSIVE — the disk guard
   stopped it at 158M states with 17M queued, no violation to depth 21.
   So the FALSE half is answered and the TRUE half is open for disk, not
   for lack of a run. **Re-run on the Linux box, 2026-09-19, on the
   review's final module** (`results/2026-09-18-crash1-orphantrack/`):
   FALSE violates `Inv_HITLTracked` at 21 again; TRUE ran 52 minutes to
   148,870,390 distinct states (549,890,823 generated) with no
   `Inv_HITLTracked` violation to depth 25 — and stopped THERE on a
   different invariant, `Inv_AckImpliesCited` (H10 in §5: an ok ack
   written by a restarted incarnation, honoured by a pull-only install of
   a peer's later delete). No sweep action is in that trace, so it says
   nothing about the sweep; the TRUE half of this question is still not a
   hold. "Not violated to depth 25" is all the run says.
5. **Replays are still open, and further along.** `churn/p47.txt` cleared
   two blockers on 2026-09-15 (the model gained the inbox-snapshot
   window; the GC's outranked branch gained a trace event) and now
   stops at a third: the model's manifest does not cite the path
   where the code's installed manifest does, so they had already
   diverged. `R2-S2-churn-ui` stops elsewhere again, at an `abandon`.
   See `lean/formal/results/2026-09-15-p47/`.
6. **Liveness.** Two runs, and the ticket's fairness is weaker in the code
   than in the model (a waiting writer does not always hold a ticket). No
   starvation has been observed; none is ruled out.
7. **Storage growth.** Conflict copies under `.flint/lean/conflicts/` are
   never collected. Not a loss; a cost. (Until immutable handles this row
   also carried the objects the collector gave way on where the store had
   no conditional DELETE. It no longer does: a handle the installed
   document stopped citing can never be cited again, so the collector's
   DELETE has nothing to guard and runs unconditionally on every store —
   `a_store_without_a_conditional_delete_collects_just_the_same`.)
8. **The untracked window.** A writer lost between its upload and its
   commit leaves an object nothing tracks until the sweep runs
   (`FLINT_SYNC_UNTRACKED_SWEEP_SECS`, 3600 s by default) or some writer
   checks out.
9. **Bytes below the protocol.** CRC-64 is verified on consume and on
   checkout; bit-rot inside the store is the store's problem.
10. **Ozone.** See §2.
11. **Coverage on the SHIPPED shape (review 2026-09-18, H8; §3.1 is the
    table).** Since 2026-09-19 the shipped shape is the handles realisation,
    and its worlds are the `ImmutableObjects` arm on `IMPL` (twenty-six
    runs) plus the core model and its refinement; the 141 slot-shape runs
    now certify the PREVIOUS shape, and they are still the only checks the
    ack, sync, narrow and convergence rows have. Before that day: the gate
    certified the ten barrier-lease invariants and `Inv_NoDeleteResurrected`
    on the code's shape (`IMPL` in `gen-cfgs.sh`: the writer queue, the
    empty install, the tombstone rules, the commit's re-reads) in eight
    strict worlds. The five ack invariants of S1 rows 1-5, the two sync
    invariants of S3, and the rename and narrow invariants of S4 are
    certified only in worlds with `BarrierLease = FALSE` — the life lease
    the code lost in v1.52.0 — or opt-in on one path
    (`LeanBarrierLeaseSentinelImpl1`, a laptop run). Those rows are
    checked by a model of a protocol that is not the shipped one. Open:
    `IMPL` variants of the sentinel, removal, narrow and sync worlds.
12. **Three enforcers §1 names are checked by nothing (H9).**
    `Inv_CitedVersionLives` and `Inv_NoUncitedGC` (S2 rows 2-3) and
    `Inv_BoundaryAtomic` (S5 row 5) are gated-lane invariants over state
    the shipped code does not have — every `LeanEntry` is resolved by etag,
    never by version id — and no cfg lists them. The rows stand as stated
    promises without a check; the reader promise the code actually keeps is
    etag-resolved reads (an object off its citation is refused for a sole
    writer and adopted otherwise). **Done 2026-09-19 (§3.1), and the
    definitions left the module on 2026-09-20:** the three rows are retired
    from §1 as promises, and what the shipped shape does promise in their
    place is the core model's `Inv_CitationsLive`, checked on the core's own
    worlds and, through the refinement, on the history model's shipped ones. And `Inv_NoResurrection` (S4 row
    1) is a ghost written only under the mutation constant, so its strict
    runs are tautologies; the code's rule (never re-materialise over a live
    tree, `checkout.rs`) is in no model action. Its restatement, drafted
    the same day for the next module edit: an ACTION property, not a
    stamp — no step that is a restart creates a local file
    (`Prop_NoResurrection == [][\A s, p : local absent then present =>
    the restart count did not move]_vars`), listed as a `PROPERTY` in every
    world that carried the invariant, and the rematerialise mutation world
    must violate it.
13. **The delete request carries no epoch — and under handles it needs none.**
    S3's DeleteObject has no fencing token. On the slot shape the collector
    renewed before a delete whose last cell write was older than
    `renew_within_secs` (C2, 2026-09-18). Handles removed the reason: the
    collector deletes only handles the installed document RETIRED, which no
    document can cite again, so a straggler deposed after its CAS deletes
    exactly what it retired (`LeanImmutableStragglerGC` holds;
    `barrier.rs` step 6, "no fence, no renew"). **Corrected 2026-09-23:**
    this item still described the renew; the code has not called it since
    handles landed. `lease::renew` is reached only from tests, and
    `renew_within_secs` and `Syncer::cell_written_at` are written but never
    read — dead, like `NfsConfig.read_only` (F71). What that leaves open is
    AVAILABILITY, not safety (review M3): nothing moves the cell token
    between the claim and the CAS, so a live holder whose commit section
    outlasts the deposal threshold (60 s; a 264 MiB entries document took
    27 s to load on the 0b rig) is deposed, its CAS is refused, and two such
    writers can depose each other every floor.

### 4.14 The evening's routes: H1b–H1f, and what the manifest now says about a delete

H1's invariant, once it reached the untracked sweep's world and once its
third contest was tightened, found four more ways a published delete came
back through a citation repair, and `Inv_TreesConverged` — written on the
way — one way it never arrived (`lean/formal/README.md`, "Review
2026-09-18, evening"; FINDINGS L-106 to L-109). Each has a test that fails
on the tree before its fix and a world that fails without its arm:

- the sweep's entry outlived the citation it was judged against (H1b) —
  the entry carries that citation and a consume drops it once the
  citation moves; an adoption made while it stood is provisional and its
  repair is withheld once the citation moves; and a leak (an object the
  manifest does not cite and no surviving entry names) supersedes no
  tombstone, so the delete reaches the tree — `Inv_TreesConverged` is
  the promise, new, checked on the four-barrier orphan worlds. (The
  tombstone of the next bullet closes this route on its own too; the
  sweep-entry rules remain as the sweep's policy, pinned by tests);
- a UI write adopted while pending and then knowingly deleted by the
  writer that cited it — the adopter itself after a restart between its
  CAS and step 7 left its merge base behind (H1c), a citer that restarted
  before its window clear (H1d), or plainly another writer (H1e) — the
  DOCUMENT now names, per published delete, the generation it retired
  (`LeanManifest::tombstones`), and a repair whose generation the
  tombstone names is void. Without it, the adopter's view is the same as
  after a delete that merely raced the write, where modify rightly wins.
  Two arms written on the way (the base persisted at the CAS; an adopted
  entry the manifest already cites taken as the merge base) were dropped
  as redundant: their known-bad worlds would not go red with the tombstone
  in. **The first of those came back on 2026-09-21, and the word for it
  was never "redundant" but "covered":** the model excused a writer's own
  install after that restart by asking whether the GENERATION was in one
  flat set per writer, which is not what the code does — the code reads
  the intent journal's `installed_etag` and takes the document it
  installed as the merge base. The moment the merge was made to ask per
  path, as the code does, `LeanSentinelRestart` failed `Inv_AckImpliesCited`
  at depth 18. The mechanism is now the code's (`MergeBase`), and the
  world holds at 218,762 states;
- a queued deletion superseded by a re-cite settled without restoring the
  merge base, and a second delete before this writer's install left the
  clean copy of the retired generation in its tree forever (H1f, the last
  world of the evening) — superseding a deletion now restores the merge
  base to the generation the delete retired, so the next install queues
  the deletion again, or the upsert if the re-cite still stands.

**What remains, named:** a tombstone lives until the path is cited again
or `TOMBSTONE_KEEP_SEQS` (10,000) generations pass; a writer whose merge
base is older than that can still re-cite what it holds. The consume pays
one manifest GET when an entry or a tombstone asks it to. And the sweep
(`untracked.rs`) still ships with `IMPL` keeping `OrphanTrack = FALSE`:
its promises are checked on the orphan worlds, not on every IMPL world
(§4.11's gap, one row wider).

### 4.15 The review's other routes, 2026-09-19: H10, H2, H3, M7, H4, H5, H7

Each was re-verified at HEAD first, then got a test that fails on the tree
before its fix and, where the model can say it, a world that fails
without its arm (`docs/plans/flint-lean-protocol-review-2026-09-18.md`,
"Applied 2026-09-19"; FINDINGS L-110 to L-116):

- **H10, the ack.** The barrier that honours a pending is not always the
  one that carried it (a restart past step 7, an ack write that failed, an
  honor that failed before the floor's cadence barrier published the
  declaration). Each carrying install now journals the paths it published
  with its CAS, and a carried path whose deletion by a peer waits in the
  queue makes the ack `partial`. No single document satisfies both halves
  of an ok ack once a peer has deleted a declared path — the first cut,
  which named the carrying document, broke `Inv_AckBoundaryCoherent` — so
  the answer is the latest document, honest about what it lacks. The
  sentinel world with a restart, on the code's shape, is in the gate now
  (`LeanBarrierLeaseAckCarried`), which narrows §4.11's gap by one world.
- **H2, the fence's position.** The commit read the cell before its HEAD
  fan-out and window, and CASed with no fence; a holder deposed in between
  CASed onto its successor's rotated document. The cell is read after each
  manifest load: deposed before that load, the read sees it; after, the
  successor's rotation moves the pointer the CAS expects. No timing
  assumption remains in this step. A restarted container that releases a
  deposal it never finished now rotates first — **not modelled**: the
  model's `Claim` is one action, so the acquire/rotate split is argued
  from the code and pinned by a test. **What a long commit still costs:**
  nothing renews between the claim and the CAS (M3), so a live holder
  with a fan-out past the deposal threshold is deposed; since H2 that is
  an abandoned barrier and a retry, not a straggler install.
- **H3 and M7, the inbox cell.** S3's 409 is a lost race in every loop on
  the cell, and the entry that survives a same-path race names the object
  at the key.
- **H4, declared removals.** A removal carries the version it was judged
  against and removes only that one; `Inv_RemovalNamesItsVersion` is the
  promise (the amputation stamp called the loss ordinary editing).
- **H5, symlinks.** Upload reads resolve every component without following
  a link. **Open:** the multipart compose path hands `flint-store` a path
  it re-opens per part; a short read fails a part, so only a swapped-in
  target at least one part long could be read. The fix is a `ComposeSpec`
  that reads from the syncer's descriptor, a change across the hub and
  forge.
- **H7, the write windows.** The licence to overwrite is re-checked with
  the temp written, immediately before the rename, and the baseline
  records the inode that was written — in the consume, and in `sync`,
  whose window was its whole run (dirt judged by one scan at its start).
  **What remains:** the rename itself, and `sync`'s identical-bytes
  recovery arm, which reads and then stats.

**H6 is not applied** — a multi-writer checkout that adopts a peer's
mid-barrier uploads path by path, and the sweep's one-path-at-a-time
tracking of a lost writer's set, deliver a tree no barrier declared. Either
the checkout refuses such an adoption and the sweep tracks a set as a unit,
or the contract says a multi-writer checkout may not be a boundary; that
is a decision about the contract, and a test
(`a_writer_killed_after_its_upload_does_not_leave_the_trees_diverged`)
encodes today's answer.

## 5. The open list

| # | what would close it | cost |
|---|---|---|
| 1 | ~~run the three-writer world in the gate~~ **done 2026-09-15** | — |
| 2 | ~~a refutation for `Inv_CommitExclusive` and for `Inv_CellHeldByHolder`~~ **done 2026-09-15**: a claim that reuses the cell's epoch breaks the first, a claim that does not stamp it breaks the second | — |
| 3 | ~~correct `Inv_HITLTracked` in BOTH arms~~ **done 2026-09-16** (§4.4; the TRUE arm re-ran 2026-09-19 to depth 25 without violating it and stopped on H10's route — still not a hold): supersession now follows the collector's DECISION, not physical destruction, and bytes at a still-cited path count as tracked. Re-run on all three mutations that require this invariant to fail — each still finds it violated — and the collector-off world now holds with all ten. The count in `COVERAGE.md` went 4→3 refutations because the fourth, `…CollectorOffHitlTracked`, never refuted the protocol: it pinned this invariant's own overreach, and the repair is what retires it. **Still open:** the crash world at `OrphanTrack = TRUE` — the FALSE arm violated as it should (2026-09-16), the TRUE arm was stopped for disk at 158M states — see §4.4 | half done; the TRUE arm needs a box |
| H10 | ~~an ok ack after a restart names a boundary a peer's delete already moved past~~ **done 2026-09-19** (§4.15): the carrying installs are journalled with their CAS and the ack is `partial` for a carried path a peer's queued delete took; two more code routes (a failed ack write; the floor's cadence barrier after a failed honor) found tracing it | — |
| 4 | finish the replays: bisect the earliest step where the model's manifest disagrees with the leg's for a path (the `cas` seqs and `observed` etags make it mechanical), then the `abandon` in R2-S2. Two blockers closed 2026-09-15 | a day |
| 5 | replay every path of every storm leg in CI, invariants on | a day, then free |
| 6 | exhaust the two-path sentinel world (box-scale) | a TLC box, ~$5-20 |
| 7 | ~~refuse a store that fails `probe-conditional` instead of documenting it~~ **done 2026-09-15**: the syncer probes before its first verb — a broken conditional PUT refuses the workspace; a broken conditional DELETE turned the collector off until immutable handles, and is now reported and nothing more (`conformance.rs`; the posture field it set is gone, 2026-09-21) | — |
| 8 | an inductive invariant (TLAPS) for S2 and S5 — of whose rows only `Inv_NoDangling`, `Inv_CommitExclusive` and `Inv_CellHeldByHolder` are stated over states today; the rest are ghost stamps | weeks; the only route to a claim that does not say "in this world" |
| 9 | `IMPL` variants of the sentinel, removal, narrow and sync worlds, so S1 rows 1-5, S3 rows 2-3 and S4 rows 2-3 are certified on the shipped shape (§4.11) | a day on a laptop for the one-path worlds; the two-path sentinel needs a box |
| H6 | ~~the structural fix: immutable object handles~~ **built and shipped**: committed with the simplified protocol (f6f6a892, 2026-09-26), released in 1.57.0 — every write to a handle nobody else writes, the manifest cites handles, unconditional batched collection of what an install retired, the sweep under the lease (`docs/plans/flint-lean-immutable-objects-design.md`). The model found L-117, L-118 and L-119 in its code before it shipped. The bucket-protocol suites re-derived for handles are green on MinIO and RustFS (verbs 16/16, chaos 12/12; deb91d71, `lean/e2e/results/2026-10-03-store-swap/`). **Still open:** the writers drill's host legs on AWS S3 and on Ozone (the ingress leg, the lost-writer leg, the collector leg) | the drill on S3 + Ozone: days |
| 10 | ~~retire or restate the three enforcers no run checks, and `Inv_NoResurrection` over state (§4.12)~~ **done 2026-09-20**: the three definitions are out of `LeanSubtree.tla` (their rows retired from §1 on 2026-09-19), and S4 row 1 is now the action property `Prop_NoResurrection` — no step that is a restart creates a local file — which the rematerialise mutation must violate, in place of a ghost only the mutation wrote | — |
| 11 | ~~the review of 2026-09-18's other routes~~ **H2, H3, M7, H4, H5, H7 done 2026-09-19** (§4.15); H6 shipped (row H6); H5's compose path done in f6f6a892 (`ComposeSpec` takes the opened file, across flint-store, forge packio and the CSI tier). **Open:** the review's M3 (a renew inside the pre-CAS phase, availability only since H2) | a day |
| 12 | a TLC confirmation of one large LeanP1 HOLDS (LeanP1Holds, AllHolds or DeleteOverrideOff), so the headline verdicts are not one checker's word | an EC2 box, hours |
| 13 | a three-SYNCER LeanP1 world — **partly done 2026-10-05**: three rungs HOLD (up to 1.42B distinct, one path, a UI save, two barriers; §3.2); the three-barrier rung was OOM-killed at 983M with no violation and needs a disk-spilling rerun; Holds1p3b's full bounds with three syncers are tens of billions of states | the three-barrier rung: a box with more disk, ~2 h |

Regenerate `COVERAGE.md` with `python3 lean/formal/coverage.py` and check
it with `--check`.
