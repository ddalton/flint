# The lean formal models

TLA+/TLC models for **flint-lean** (checkout/publish + gateway — plan of
record: `docs/plans/flint-lean-plan.md`). Model BEFORE code, the
FlintExtents posture: the module was written against plan v2 and the
review's confirmed counterexamples, and the syncer implementation
(`lean/syncer/`, the flint-lean crate) is written to it.

**Deliberately separate from `formal/`** (the flint corpus and its
196-run gate): lean is a separate system that consumes `tier::store` as
a library. Nothing here is wired into `scripts/check-tla.sh`.

## Running

```
./check.sh              # the 110-run gate (runs view-census.py first)
./gen-cfgs.sh           # regenerate the cfg matrix
./trace/trace-check.sh  # the model against syncer traces (below)
```

A hundred and ten runs, ALL required: 28 strict (must hold), 43 mutations
(must find their designated counterexample — a model that cannot
rediscover its bug classes proves nothing), 39 probes (must be violated
— each names an ACTION via a ghost only that action writes; probe the
action, never the situation). The three numbers are `grep -c "^strict_run "`,
`grep "^mutation_run " | grep -vc Probe` and `grep "^mutation_run " |
grep -c Probe` in `check.sh` — they had drifted from the script twice,
so they are stated as a recipe rather than a claim. (And `EXPECT` itself
drifted once: gated mode's removal took 23 runs out and left it at 92
over 69 real calls, so the gate at that commit failed its own count
check — which is what the check is for.) `LeanSubtreeDeep.cfg` is the
rich-budget breadth run — an opt-in overnight job, not in the gate — and
`LeanBarrierLeaseSameBytesDeep.cfg` is the same-bytes write in the richest
barrier-lease world, also opt-in: it does not fit a laptop (it HOLDS —
382,678,936 distinct states, depth 39, two hours on an i4i.2xlarge).

## The module: LeanChunkGC.tla

Chunk garbage collection against a concurrent publisher and a reader
(`docs/plans/flint-lean-chunked-manifest-design.md` §8.1). Separate from
LeanSubtree because the manifest there is ONE object per generation, so
every object had exactly one referent and "delete what the live pointer
does not name" was sound. **Chunks are shared between generations**, and
that single change is what makes the old reasoning not carry over.

Written before the reaper exists, and it earned its keep immediately: it
**refuted the design's own ordering rule on the first run**. §8.1 said
"list candidates, then union the retained pointers"; the counterexample
holds that order and still deletes a live chunk, because what matters is
that the reference set was read before a CAS the delete came after. The
corrected rule has four independently necessary clauses, one mutation
each.

The second finding was subtler: **adoption must rewrite what it adopts**.
An adopted chunk is an aged object no pointer references — exactly what
the orphan sweep hunts — so referencing it without touching it leaves the
age sensor lying. Reaching that config at all required adding a CRASH
action; the first version of the module reported it HOLDING because
without a crash it could not produce an orphan, which is the entire
subject of the section. The abstraction was the bug, again.

| Invariant | Claim | Mutation that must violate it |
| --- | --- | --- |
| `Inv_LiveComplete` | every chunk the live pointer names is present | `LeanChunkGCStaleRefs` / `LeanChunkGCRefsFirst` (refs read before a CAS the delete follows), `LeanChunkGCNoGrace`, `LeanChunkGCRacyGrace` (grace shorter than the publish), `LeanChunkGCAdoptSkips` (adoption without the rewrite) |
| `Inv_RetainedComplete` | every retained generation is still readable | the same set |
| `Inv_NoTornRead` | a reader never finds a chunk its pointer named, absent | `LeanChunkGCSlowReader` (the reader does not revalidate). Holds at an UNCHANGED `Retain = 1`: the fix is that a reader which finds a chunk gone re-reads the POINTER, and restarts onto the current generation if it moved — a hole under an unchanged pointer is the only true corruption (§8.2). No timing assumption, no wider window |

## The module: LeanChunkMerge.tla

The chunked three-way merge (chunked-manifest design §6). The
entry-level merge is LeanSubtree's and is not re-derived; this checks
the level chunking adds. A writer that 412s must merge, and the
tempting optimisation is to reuse the other writer's chunk list and
substitute only the chunks its own change touched — making the merge
O(changed) as well.

It loses foreign entries. `base = {}`, A adds `{1}`, B adds `{1,2}`:
A's chunking is `{{1}}` and B's is `{{1,2}}`, the same range under
different boundaries, so substituting A's chunk for B's drops key 2.

The property is a REFINEMENT — whatever the chunked path publishes must
be exactly the key set the whole-document merge produces — so the
strategy is checked against a definition rather than against a restated
version of itself.

Worth knowing WHY this can happen at all: with boundaries determined by
the key alone, a key's chunk is a function of the key, splicing is
trivially safe, and there is nothing to check. `MinRun` breaks that, and
that is the entire subject. §3 calls min/max "where the pure-function
property leaks"; this module is what that leak costs.

| Invariant | Claim | Mutation that must violate it |
| --- | --- | --- |
| `Inv_ChunkedMergeMatches` | the chunked merge publishes exactly the whole-document merge's key set | `LeanChunkMergeSplice` (reuse theirs, substitute mine) |

## The module: LeanSubtree.tla

One subtree; syncers A (first holder) and B (takeover successor); the
gateway abstracted to its bucket effects; the bucket substrate: lease
cell, manifest (seq + per-path citation), whole-file objects
(generation = ETag), the inbox/window cell. Generations model ETags;
If-Match is equality on them; whole-PUT is atomic.

Invariants:

| Invariant | Claim | Mutation that must violate it |
| --- | --- | --- |
| `Inv_HITLDurable` | an acked HITL write is never silently lost | `LeanAmputation` (direct manifest bump + whole-rewrite writer), `LeanLocalWins` (the inherited flush.rs LOCAL-WINS 412 arm), `LeanGCUnguarded` (unguarded GC delete); with TWO LIVE WRITERS `LeanBarrierLeaseHitlOverUncited` (a UI write over a writer's uncited upload — a FINDING, tranche 6) |
| `Inv_NoDangling` | every cited manifest entry has a live object | `LeanDanglingOrder` (v1 order: upload→delete→CAS); under the barrier lease `LeanBarrierLeaseGCUnconditional` (the shipped HEAD-then-DELETE GC against a second writer's supersede) and `LeanBarrierLeaseAdoptBlind` (an adopted entry cited without re-verification under the lease) — both FINDINGS, tranche 6; and `LeanBarrierLeaseSameBytesUnverified` (a LANDED upload of identical bytes, whose etag the other writer's GC recognises — finding 13, found by the live drill first) |
| `Inv_NoStaleOverride` | no commit replaces a citation the key still holds with a generation of its own upload the key no longer holds (barrier lease) | `LeanBarrierLeaseSameBytesOverride` (finding 13's second route: a peer's new bytes land If-Match an identical-bytes upload's etag and are committed; nothing dangles, so `Inv_NoDangling` cannot see it) |
| `Inv_NoStragglerInstall` | a deposed writer's manifest CAS never lands | `LeanNoRotate` (no takeover rotation); `LeanBarrierLeaseNoRotate` (the same, deposal mid-commit) |
| `Inv_NoDeposedPut` | a deposed writer's data PUT never lands | `LeanNoEpochCheck` (rotation alone — proves rotation does NOT cover the data path). Under the barrier lease this is vacuous by design: uploads precede the claim and are never fenced (protocol of record, straggler rules) |
| `Inv_NoResurrection` | a container restart never resurrects an unpublished delete | `LeanRematerialize` (re-checkout over a live tree) |
| `Inv_SyncNeverDestroysDirty` | the sync verb never destroys genuinely-dirty local work without surfacing it | `LeanSyncStaleDirt` (sync judging dirt from the last barrier's snapshot) |
| `Inv_NoForeignLost` | a sync never advances the merge base for a path it did not integrate or surface (D4) | `LeanScopedSyncWholeBase`; and with TWO LIVE WRITERS the shipped rule itself: `LeanBarrierLeaseSyncOverlayStale` (a FINDING, tranche 6; its control `LeanBarrierLeaseSyncOverlayHolds` moves `SyncKeepsHiddenBase` alone) |
| `Inv_CommitExclusive` | one writer in the commit section at a time (D1, the lease's safety half) | none needed yet: it is what every deposal run checks, and it held in every world |
| `Inv_CellHeldByHolder` | model coherence: a HELD cell's holder is in its commit section at the cell's epoch | — (pins the claim arms; a wrong arm fails it first) |
| `NoStarvation` (liveness, `FairSpec`) | every live writer that queued for the cell eventually holds it | `LeanBarrierLeaseRandomArbitration` (no ticket: one writer claims forever), `LeanBarrierLeaseDeadHandoffWedge` (no dead-handoff skip: a crashed waiter wedges the cell) |

Deliberate strict-HOLDS runs (machine-checked design findings, the
FlintClaimsNoLeader idiom):

- `LeanNoWindowHolds` — with the inbox + the 412-park + the GC guard
  in place, the WINDOW carries no safety at whole-PUT atomicity: its
  value is availability/UX (refuse-vs-queue semantics) and defense in
  depth below the model's atomicity floor. Do not cite the window as a
  safety mechanism.
- `LeanEpochOnlyHolds` — per-request epoch validation alone fences the
  straggler's manifest CAS; rotation remains as defense for any write
  path that bypasses the gateway's validation.

## What the model already caught (worth not re-learning)

0. **THE INBOX IS LOAD-BEARING — merge alone is NOT sufficient.** The
   planned `LeanDirectMergeHolds` strict run was REFUTED (now the
   `LeanDirectMergeInsufficient` mutation): a merge-capable writer
   preserves a direct-bumped foreign entry, but preservation without
   INTEGRATION is one barrier deep — Finish absorbs the entry into the
   merge base, and a later local delete then destroys the user's
   citation with no record (depth-12 trace: bump → preserve → absorb →
   delete). Only the inbox's consume path (integrate-or-surface) makes
   HITL durable against subsequent local operations.
1. **The amputation stamp needed a legitimacy term.** A syncer that
   CONSUMED a user's upload and then published a delete of it is doing
   integration + ordinary editing, not amputation. Legitimacy rides on
   the `known` set (generations learned via checkout, own mints, and
   surfaced consumes — blind adoption deliberately does not extend it).
2. **The GC delete set must be derived from the INSTALLED manifest, not
   the scan** — "keys the NEW manifest no longer references." After a
   delete/modify conflict the merge re-cites the foreign entry, and the
   scan-time delete set would GC a key the manifest still references.
   The v1 order structurally cannot make this check (no new manifest at
   delete time) — that asymmetry is part of what `LeanDanglingOrder`
   pins.
3. **`baseline` and `instBase` are different objects.** The If-Match
   baseline (what I believe the bucket objects hold AND have
   integrated) advances at consume; the merge base (the manifest view
   at my last install) does not — collapsing them makes a syncer
   mistake its own consumed adoption for a foreign entry.

## Tranche 2 (2026-08-25): the sync verb × the barrier

`Sync(s)` joins the module behind `SyncEnabled` (FALSE in every
tranche-1 cfg, and `lastDirty` is only tracked under it, so those state
spaces are preserved by construction). The verb is modelled as the
implementation serializes it: harness-invoked, at `pc = "idle"`,
against remote truth = the manifest overlaid by live inbox entries.

`SyncScanFirst` is the arm. TRUE = the shipped rule (dirt is judged by
sync's OWN scan); FALSE = the refuted design (dirt is whatever the last
barrier's scan froze). **The A/B is genuinely attributive**, which the
corpus insists on before believing a mutation: with scan-first ON the
destroying case is *unsatisfiable* — `applicable = changed \ trueDirty`,
so `p ∈ applicable ∧ p ∈ trueDirty` cannot hold — and
`LeanSyncHolds` carries the invariant green in exactly the same world
where `LeanSyncStaleDirt` violates it. The counterexample is the
review's finding in four steps: agent writes p after the last barrier
(or before any barrier), a HITL write lands on p, sync judges p
"clean" from the stale snapshot, and the remote version overwrites the
agent's un-scanned latest work with no conflict record.

Two probes keep the strict run honest: `ProbeSyncApplied` (sync really
does apply a remote change) and `ProbeSyncConflict` (it really does
surface a dirty-path conflict) — both must be violated.

## Tranche 3, product 4 (2026-08-25): the SCOPED sync verb × the merge base

The boundary-verbs plan's D4 rewrites the per-path semantics of
`instBase` — the object this model has refuted naive designs on twice —
so it is modelled before the rule is trusted. `SyncScope` is FALSE in
every tranche-1/2 cfg (scope collapses to `Paths`), so those state
spaces are preserved by construction.

`ScopedInstBase` is the arm. TRUE = D4: a scoped sync advances the merge
base only for paths it applied or verified in scope. FALSE = the
mutation: it advances the whole base to bucket-current, so every
out-of-scope foreign entry reads as already-integrated at the next
merge, `foreign(p)` is FALSE forever after, and the entry is never
queued into the inbox again. `Inv_NoForeignLost` is the stamp; the loss
is *silent and permanent*, which is why it is a safety invariant rather
than a staleness note.

**The world note, and it cost a wrong cfg before it was written down.**
The D4 loss needs an out-of-scope change that lives in the MANIFEST, not
in the inbox: an inbox-overlaid change survives a wholesale `instBase`
advance untouched, because the entry itself is still queued. In this
design the only legitimate foreign manifest installer is a takeover
successor — so these runs need `AllowStall` and a second barrier. With
`MaxBarriers=1` and no stall arm the hazard is UNREACHABLE and the
mutation runs green against a state space that never contained the bug.
The first hand-written Rust test for this rule was vacuous for exactly
the same reason: it used a HITL inbox entry as the out-of-scope change
and passed with the hazard reintroduced.

**Budget, verified as a pilot before being locked in** (the plan's
affordability obligation): `MaxGen=2` + `MaxHitl=0`, the takeover cfgs'
depth-buying trick. At that budget the strict run completes in ~9 s AND
both the mutation and the probe still fire — the strict run is not
checking a smaller world than the bug lives in. At `MaxGen=3`/`MaxHitl=1`
the strict run passed 30M states without terminating.

`ProbeScopedDeferral` is action-written (Sync's own ghost counts the
paths it deliberately deferred), per the house rule that a probe names
the ACTION and never the situation.

## Tranche 3, product 2 — gated citation × version GC × the backstop

Nine runs (27 → **36**, at the time), behind `GatedCitation`, which is FALSE in every
pre-existing cfg: the gated actions are disabled and `versions`, `stage`,
`stageBase` and `withheldDel` are frozen at their Init values, so those
state spaces are preserved by construction. `VersionsFollow` composes the
version-minting rule once at the `Next` level rather than threading it
through twenty actions — under gated, *any* action that moves an object
mints a version, which is what a versioned bucket does.

**The substrate is the point.** Generations are unique mints, so a
generation already IS a version id and `manifest[p]` already cites one.
What the product adds is `versions[p]` — what is still STORED, which on
a versioned bucket is a different question from what the key reads as.
`Inv_NoDangling` ("the object exists") was the right question until D7;
gated staging makes the CITED version noncurrent, so an object can
exist, read as newer uncited bytes, and have nothing behind its
citation. `Inv_CitedVersionLives` is the corrected question.

**It found a live defect on its first strict run, in shipped code.**
The reaper's rule was *"delete every version of a touched key except the
one the installed manifest cites"*. The upload lane opens no HITL window
— deliberately, since a lane that fenced HITL out every floor tick would
refuse admission essentially forever between citations — so a UI write
can land on an already-staged path. The citation's base-version check
cannot see it (that check reads the BASELINE, and the citation lane
consumes nothing), so the citation cited our staged version, and the
reaper then deleted the user's version. **It was current, it was acked,
and the inbox entry 412s on its next consume and is dropped as
superseded.** Two rules close it, both now in the code and both modelled:
the reaper never reclaims the CURRENT version, and a staged path with a
live inbox entry is dropped from the boundary rather than cited over
(the window CAS has already loaded the inbox — zero added requests).

**And it retired a guard that protects nothing.** D7 also specifies a
base-version re-validation: drop a staged entry whose baseline moved
under it. The model showed that arm is UNREACHABLE given the lane's own
discipline — the lane never advances the baseline, so a staged path is
by construction locally-dirty, and every route that could move a
baseline (consume, sync) refuses dirty paths and surfaces a conflict
instead. It stays in the implementation as defence in depth; what the
model says is that it is not what protects anything today, and that the
hazard it was written for arrives by a route it could not see. That is
the second-order return on modelling after the fact: not only "here is a
bug" but "here is a guard you believed in for the wrong reason".

**Defence in depth, pinned as such.** `LeanGatedReapsCurrent` turns off
BOTH the keep-current rule and the inbox guard, because with the inbox
guard in place removing keep-current changes nothing — the reaper's
scope never reaches the path. The Rust battery hit exactly the same wall
and needed a second leg (an out-of-band writer, §3 residual 11's
population) to isolate the rule. A one-arm cfg here would have been a
green mutation dressed as a passing test.

**Budget:** `MaxGen=3`, `MaxHitl=1`, `MaxBarriers=2`, no crashes or
restarts — ~19k distinct states, ~2 s for the strict run. `MaxHitl=1` is
load-bearing rather than breadth: the whole product turns on a foreign
write arriving between the lane and the citation, and at `MaxHitl=0`
that interleaving does not exist and every mutation checks a state space
its bug cannot live in.

**Not modelled, named rather than omitted silently:**
`Inv_ManifestKeysUnderFiles` (this module has no control namespace; D0.2
is carried by the Rust battery's scan/classify/checkout legs), and the
citation's own crash matrix (product 1's territory — the citation and
its reaper are ONE step here, which is faithful only because the real
code holds the HITL window across both).

## Deliberate abstractions (tranche 1 — residuals, not coverage)

- The scan is ATOMIC: the rename-vs-walk race and the
  two-consecutive-scans deletion rule are UNREPRESENTABLE here. The
  implementation carries the rule; only the drill can exercise it.
- The 6-quiet-poll takeover observation is one `ClaimB` action; the
  poll protocol itself is machine-checked in flint's
  `FlintTierEpoch.tla`, and lean's claim loop mirrors it.
- Checkout ignores hydrate's 412/S3-wins divergence arm.
- Multi-subtree layout (P2/P3), partial checkout, preStop timing, and
  every perf axis (Phase 0b/0c) are out of scope. (The sync verb moved
  IN — see tranche 2 above; it is atomic there, which is faithful to
  the quiescent contract but leaves an agent writing DURING sync
  unmodelled.)
- `conflicts` is a set of records: the implementation obligation is
  that a conflict record preserves the BYTES (conflict-suffixed key —
  `lean/syncer/src/barrier.rs` does this), not just the reference.
- Multi-gateway is collapsed into the cell semantics: replicas are
  stateless by design, so the window cell IS the coordination — a
  per-replica model adds states, not behaviors, at this abstraction.

## Tranche 3, product 1 — the boundary VERB × the barrier × the inbox

Thirteen runs (36 → **49**, at the time), behind `SentinelEnabled`, FALSE in every
pre-existing cfg: every sentinel action is disabled, the skip-on-no-diff
fast path is unreachable, and the seven new `sc[s]` fields stay at their
empty Init values, so those state spaces are preserved by construction.

**It found two defects in shipped code, both on strict runs, before a
single mutation was applied** — and it rejected two of my own invariant
formulations first, which is the more useful lesson.

**The promise took three tries to state, and each wrong try was a real
behaviour.** An ok ack asserts that the coherent point the agent
declared is INSTALLED. Stating that as *snapshot equality* is wrong,
because D1's guarantee is at-LEAST — "the published state may include
later bytes for a racing file, never earlier ones". TLC's answers, in
order: (1) an agent that deleted a path, declared, re-created it, let
the barrier publish the re-creation and deleted it again, so the
consume-time snapshot matched the tree again at ack time while the
manifest legitimately cited later bytes — hence `pendMint`, a
generation watermark; (2) an inbox adoption of a HITL write that landed
before checkout, where the agent had no work on the path at all — hence
`pendDirty`, so the promise covers only what was locally dirty at the
consume; (3) an agent deleting its own declared file after declaring,
which supersedes while minting nothing — hence the tree-comparison
clause the watermark cannot replace. All three exemptions are in
`BoundaryBroken` with the counterexample that forced them.

**Two invariants, not one.** Counterexample (2) also showed that "the
agent's own work survived" and "the point the ack names is coherent at
all" are different claims: a citation repair still owed at ack time
risks no work of the agent's and still means a reader — or this
workspace's own next checkout — resolves to bytes already superseded
here. `Inv_AckImpliesCited` and `Inv_AckBoundaryCoherent` each have
their own mutation, and neither fires the other's.

**Defect one: a restart between the manifest CAS and step 7 ate the
agent's delete.** The merge base and the baseline are both rewritten at
step 7 — after the CAS, after the GC deletes. A container restart in
that window leaves the bucket holding a document this workspace wrote
and the persisted merge base one generation behind it, so at the next
merge our own entries read as somebody else's change; delete/modify
resolves conservatively by design, so the agent's delete is dropped
from the boundary it is about to be acked for and the path is queued
into the inbox as a conflict nobody else ever touched. TLC produced it
by two different routes — an adopted inbox write and our own upload —
which is what killed the first fix (an entry-`epoch` test, fooled by an
in-place foreign edit that leaves the epoch field alone; the battery's
`local_delete_loses_to_foreign_modify` said so within seconds). The fix
that holds is document identity: `IntentJournal::installed_etag`,
written immediately after the CAS. Pinned as `MineIsNotForeign`.

**Defect two was found by reading for the model, not by running it**:
the D12 heartbeat renewal arm returned on `Fenced` without settling
owed acks. Both honor arms settle; the heartbeat — decoupled from
publish cadence, and therefore usually the FIRST arm to discover
deposal — did not. `RenewDiscover` models the fixed rule.

**§10.1's deliberate deviation is now machine-checked rather than
argued.** §2.1 prescribes that a pending sentinel defeat the
skip-on-no-diff fast path; the shipped code lets it through, because
defeating it would cost a manifest CAS at up to 720/hour/workspace. The
argument was that the fast path only fires when every local byte is
already cited. `FastPathGuards=FALSE` drops the two guards that carry
it — no citation repair owed, and the remote manifest where we left it
— and `Inv_AckBoundaryCoherent` must fall. `ProbeFastPathHonor` is what
keeps the strict side from holding because the fast path never ran.

**A budget note that cost a pilot.** The fast path must charge the
barrier budget. Without it `Consume → FastPath → Consume` is a free
cycle that consumes nothing, and the state graph's DIAMETER grows
without bound — the pilot ran to depth 148 and 17M states before that
line existed. And `MaxGen=3` with `MaxRestarts=1` does not fit: the two
worlds are split — `MaxGen=3/MaxRestarts=0` for breadth,
`MaxGen=2/MaxRestarts=1` for the crash matrix, and `MaxGen=2/MaxHitl=0`
with one touch for the stall/takeover world (at `MaxGen=3` the deposal
run passed 1.3 GB of TLC scratch without terminating: two live syncers,
each with its own sentinel, pending record and ack, is a different scale
from one) — and every mutation runs in the smaller world its
counterexample needs.
`MaxTouches=2` is load-bearing exactly as `MaxHitl=1` was for product 2:
the orphan hazard needs a second consume landing on a live pending
record, and `ProbeCoalescedAck` is what proves that is reached.

**The harness earned its keep on this tranche.** One mutation's world
lost the arm its counterexample needs — a cfg override that silently did
not apply, leaving `MaxRestarts=0` on the run whose whole subject is a
crash between the CAS and step 7. It completed a full 1.3M-state search
and reported no error, which reads exactly like a pass. `mutation_run`
treats rc=0 as a FAILURE for precisely this reason: a mutation that
cannot rediscover its bug proves nothing, and the only way to tell that
from a fix is to demand the counterexample by name.

Not modelled, named rather than omitted: the two-consecutive-scans
delete rule (still unrepresentable here — the battery isolates it with
five mutations after it produced its own shipped defect this session),
the bare touch, the min-interval and hourly budget (rate limiting stays
out of the safety gate), and an agent restoring byte-identical content,
which unique mints cannot express.

## Product 1 × product 2: the sentinel over the citation lane (6 runs)

Both products were already green, each in a world where the other was
switched OFF — so the citation-lane honor, the one path where a boundary
can be *installed* and still not carry what its ack claims, had never
been evaluated at all. `CiteFinish` sets `honored` under
`SentinelEnabled`; the module could always express it, the cfg matrix
never asked.

What the pairing cost and what it bought, in order:

- It refuted a fix that was two hours old. `LaneCancelsStaged` — a
  withheld delete cancels the version the stage still holds, and vice
  versa — went in because delete-then-recreate amputated a live file;
  the model showed that resolving the overlap the other way cites a file
  the agent DELETED. Neither set carries the ordering, so `merge` cannot
  arbitrate it in either direction; the lane can, and does.
- It found C2's gap as a model artifact (`GatedRepair`): the citation
  lane had no citation-repair, so an ok ack named a manifest that did
  not cite a HITL write the workspace had already integrated.
- Three of the four defects it reported first were **in the model**, and
  saying so is the point of writing them down: `dels` guarded the UNCITE
  with the GC's own guard (the code uncites in the CAS and lets the GC
  refuse the object separately); `Consume` could interleave between the
  citation and the ack, which a single-threaded honor cannot; and the
  adopt-own arm staged any recognized generation, where `upload_one`
  adopts only when the object holds the bytes it is uploading and
  otherwise supersedes them knowingly.
- `BoundaryBroken`'s conflict exemption had to be NARROWED: a conflict
  record is the ack's `report.parked` in the fused path, which is why it
  excuses a path there — a correspondence the gated honor cannot
  maintain, because the drop happens inside the citation and the honor
  writes one ack for the lot. Written as a plain conjunct it excuses
  exactly the case it exists to catch; it is a disjunct with the
  exemption for a reason.
- And `ProbeDeclaredDrop` found a hole in the runs that came BEFORE it.
  The in-flight drop needs four mints (a second staged path — without
  one no citation fires at all — the dropped path's generation, the HITL
  generation, and the declaration's watermark), and no gated world had
  the budget. So **`CiteDropsInflightHitl`, product 2's rule, has never
  had a positive reachability probe**: its mutation fires through an
  unrelated shape, and the state the rule actually guards was
  unreachable in its own world. One probe, in an already-green gate.

Not modelled here, named rather than omitted: what the drop-inflight
rule guards in shipped code — a HITL write landing between the lane's
consume and the citation's window — is not expressible, because the
gated lane reuses `Scan`, which OPENS the window, while the shipped lane
deliberately opens none. Making the gated lane window-free is the
fidelity fix and it is not free.

## Tranche 6 (2026-09-13): the PER-BARRIER lease × the FIFO ticket

Design of record: `docs/plans/flint-lean-writer-lease-and-gated-assessment.md`
§4–§8 and the lease-v2 protocol note. The cell is held for ONE barrier's
commit section — claim after the uploads, release after the baseline —
with a FIFO ticket, instead of for the pod's life. Twenty-one runs
(69 → **90**), behind `BarrierLease`, FALSE in every pre-existing cfg:
`StartA`/`ClaimB` keep the life lease, `Scan` opens the window, every
fence kills, and `cellQueue`/`cellHandoff`/`cellReleased` stay frozen at
Init. **Preserved by construction, verified by number**: four strict
worlds re-run on the new module against the HEAD module's counts —
`LeanRemovalHolds` 332,847, `LeanSentinelHolds` 1,208,901,
`LeanSubtreeTakeover` 1,365,619, `LeanSentinelDeposal` 3,484,752 distinct
states, all identical.

What the module gained, and which action writes each thing:

| new | kind | meaning |
| --- | --- | --- |
| `cellQueue`, `cellHandoff`, `cellReleased` | variables | the ticket: waiters in FIFO order, who the last release named, and whether the cell is HELD (`cellEpoch > 0 /\ ~cellReleased`) or RELEASED |
| `StartLease(s)` | action | both syncers start by checking out, holding nothing (B no earlier than A: symmetry breaking only) |
| `Claim(s)` | action | fresh cell, released cell whose handoff is nobody/me, or the DEPOSAL of a quiet holder (stalled/dead — the 60 s rule, the same abstraction `ClaimB` uses); only the deposal rotates; the window opens HERE |
| `Enqueue(s)` | action | a syncer that cannot claim takes a ticket (pc `waiting`) — separate so the queue is observable |
| `SkipDeadHandoff(s)` | action | the 20 s rule: a quiet handoff is dropped and the released cell is anybody's |
| `Finish(s)` | changed | releases as its LAST step (handoff = queue head); a crash between CAS and Finish leaves the cell HELD by a dead holder, which the deposal covers; a deposed holder releases nothing |
| `Restart(s)` | changed | a restarted container that finds the cell held by its own id releases it |
| the fence arms | changed | `FencedSc`: under the barrier lease a fence ABANDONS the barrier and the syncer keeps running; no `refused-fenced` ack exists (`AckRefused` is life-lease only) |
| `Holding(s)`, `DeposedHolder(s)`, `Fenced(s)` | helpers | `Deposed` means something only inside the commit section; every fence and every straggler stamp now reads `DeposedHolder`, which collapses to `Deposed` under the life lease |
| `Upload(s, p)` | changed | a NON-holder action, never fenced; `gh.interleaved` is the required-reachable probe |
| `GCHead(s, p)`, `gcHeaded`/`gcSeen` | action/fields | the shipped two-request GC, under `~ConditionalGC` (finding 1) |
| `adopted`, `VerifyAdoptedCitations` | field/arm | an adopted entry is re-verified inside the commit section (finding 2) |
| `AgentWriteSame(s, p)`, `MaxSameBytes`, `touched` | action/budget/field | the agent writes a generation it RECOGNISES — identical bytes, one etag; back to the baseline's own, the path is dirty by its stat alone (`Dirty` reads `touched`; Consume does not adopt over it; Sync and Finish clear it where they rewrite the baseline) (finding 5) |
| `VerifyUploadedCitations` | arm | CASInstall re-verifies every citation its own LANDED uploads add, not only adoptions (finding 5, the 79e7dac9 fix) |
| `gh.staleOverride`, `Inv_NoStaleOverride` | ghost/invariant | stamped in CASInstall when a commit cites its own upload's generation over a citation the key still holds (finding 5's second route) |
| `AckedDoc(s)`, `AckedSrc(s)`, `instSrc` | helpers/field | under the barrier lease the three ack stamps (`BoundaryBroken`, `BoundaryIncoherent`, `srcMismatch`) are judged against the document the ack NAMES (`instSnap` at `instSeq`, seeded at checkout for a never-installed writer) and its clock, never against the live manifest — see "the ack under two writers" below |
| `InfiniteBarriers`, `FairSpec`, `NoStarvation` | liveness | see below |
| `gh.claimed/interleaved/handoffs/deposals/deadSkips/enqueues/abandoned/adoptWithheld/uploadWithheld` | ghosts | one probe each, each written by exactly one action |

**The liveness property, and what it took.** `NoStarvation ==
\A s : [](Waiting(s) => <>(Holding(s) \/ ~Running(s)))` under `FairSpec
== Spec /\ \A s : WF_vars(BarrierStep(s))` — weak fairness on each
writer's OWN barrier step, never on "some writer steps". The first
obstacle was not the two symmetric writers, it was the budgets: every
counter a no-change barrier moves (`gh.barriers`, `manSeq`, the epoch,
`gh.done`) is monotone, so under budgets the state graph has no cycle
and "claims forever" is not a behaviour TLC can exhibit — the FALSE arm
was GREEN, because after the last budgeted barrier the waiter's claim is
continuously enabled and WF fires it. `InfiniteBarriers` is the
abstraction: those counters saturate instead of stopping the world, one
path, no writes, no crashes. No third syncer and no biased choice were
needed once the loop could cycle. Results, each ~5 s:

- `LeanBarrierLeaseLive` (`Ticket`): HOLDS, 24,465 states.
- `LeanBarrierLeaseRandomArbitration` (`Ticket = FALSE`): VIOLATED —
  the lasso is A `Claim → CASInstall → Finish(release, handoff none) →
  Consume → Scan → Claim …` ("Back to state 33") with B parked at
  `waiting` in `cellQueue = <<"B">>` throughout: B's `Claim` is enabled
  only between A's release and A's next claim, WF asks nothing of an
  action that is not continuously enabled, and A claims forever. With
  the ticket, A's release names B and A's own next claim is DISABLED
  until B holds, so B's claim is continuously enabled and WF forces it.
- `LeanBarrierLeaseLiveCrash` (a crash, `DeadHandoffSkip`): HOLDS,
  103,448 states — a dead holder is deposed, a dead handoff is skipped.
- `LeanBarrierLeaseDeadHandoffWedge` (`DeadHandoffSkip = FALSE`):
  VIOLATED — B queues, crashes; A's release names the dead B; A queues
  behind a handoff that will never claim, and stutters there forever.

**Five findings, all two-writer, none reachable under the life lease** —
four found by the model, and a fifth it could not see until an
abstraction was taken back (below).
The rule was: a real counterexample in the protocol is not "fixed" in
the model; it is pinned as the must-fail it is, and the fix — where one
belongs to this protocol — is modelled as an arm so the rest can be
checked against it (the `MineIsNotForeign` pattern).

1. **The GC delete is a HEAD then an unconditional DELETE**
   (`barrier.rs` step 6). Under the life lease the lease covered that
   window; under the barrier lease the other writer's uploads hold no
   lease, and its supersede landing between A's HEAD and A's DELETE
   leaves B's citation dangling. The module's atomic `GCDelete` had
   always hidden this; `ConditionalGC = FALSE` makes it two steps and
   `LeanBarrierLeaseGCUnconditional` finds it in 86,771 states, no stall,
   no crash, no HITL. The fix is a conditional delete (If-Match on the
   recognised etag — S3 supports it on DeleteObject; `flint-store`'s
   trait has only `delete(key)` today). Strict runs use the conditional
   delete; that is what they certify.
2. **An adopted entry is cited blind.** A barrier that restarts between
   its CAS and its baseline re-uploads next time and finds its own bytes
   already there; `upload_one` adopts (CRC match, no PUT — the correct
   rule, and the model's ungated arm was made to follow it under the
   barrier lease: it used to adopt ANY recognised generation and cite
   the walk's bytes, which is how the first four strict runs dangled
   before the real race was reached). Between the adopt and the
   adopter's CAS the other writer's commit uncites the path and its GC —
   HEAD-guarded on an etag it learned at checkout — deletes the object;
   the adopter's merge then upserts a citation over nothing. TLC found
   it at depth 28 of the first stall run (5.0M states); the restart
   world (`LeanBarrierLeaseAdoptBlind`) reaches it without a stall. A
   same-bytes re-PUT would not help — a real etag is the content hash,
   so the recognised-etag guard cannot tell the re-PUT from the
   original. The one race-free place to look is the commit section,
   because GCs run only under the lease: `VerifyAdoptedCitations`
   re-verifies adopted entries at the CAS and withholds what is gone
   (parked, with a record; the path stays dirty). `LeanProbeAdoptWithheld`
   proves the arm fires; `LeanBarrierLeaseAdoptVerified` is the control.
3. **D4 does not survive a second live writer.** The sync verb's remote
   truth is the manifest overlaid by live inbox entries (`sync.rs` step
   2: "an inbox entry is a write the manifest has not re-cited yet").
   With two writers the entry B's own merge queued (A's install) can be
   OLDER than the manifest: A deletes the path afterwards while the
   entry's object still exists, so B's scoped sync "verifies" the path
   unchanged against the overlay and advances the merge base to the
   manifest — the silent, permanent loss `Inv_NoForeignLost` names. One
   writer cannot produce it: the only party that could move a manifest
   past its own queued entry is dead. Pinned as
   `LeanBarrierLeaseSyncOverlayStale` (817,765 states, 25 s). The fix is
   the sync verb's — advance the base only to what was VERIFIED, never to
   a manifest the overlay hid — and was built the same day (`sync.rs`
   step 5, with a repro test that fails without it). Modelled as
   `SyncKeepsHiddenBase` (FALSE in every earlier cfg, so their state
   spaces are unchanged); `LeanBarrierLeaseSyncOverlayHolds` is the same
   world with that one constant moved, and HOLDS (1,493,045 states,
   104 s).

   Two things the code now does that this module does NOT yet model,
   named so the gap is not mistaken for coverage. The code no longer
   queues merge-preserved entries in the SHARED inbox — a peer's consume
   dropped them as "already integrated" and the writer that needed them
   never converged — but keeps them, with the peer's DELETIONS as
   tombstones, in a writer-local queue; the module's `foreignQ` still
   joins the shared `inbox` at install. And a barrier whose merge adds
   nothing to theirs installs nothing (two idle writers traded empty
   generations). Both are convergence properties, invisible to the
   safety invariants here; modelling them is a liveness tranche of its
   own. Until then the overlay finding's trace (a stale merge-preserved
   entry) is a path the code no longer takes — the stale overlay it now
   guards against is a consumed HITL entry between a commit's CAS and
   its window clear, which the repro test drives.
4. **A UI write over an uncited upload is lost** — found by the gate,
   not by review: the first full run under the three fixes above
   violated `Inv_HITLDurable` in the two-writer sentinel world. The
   window now opens at the claim, so nothing holds the gateway off
   while a writer uploads, and the gateway's PUT is If-Match the key's
   CURRENT etag — which may be A's upload, not yet cited. B consumes the
   UI write, cites it and drops the entry; A's commit then re-cites its
   own generation over it. The UI's acked write is uncited, untracked
   and preserved nowhere, and the manifest cites bytes the key no longer
   holds. No writer-side rule can see this: A's upload and the UI's PUT
   both succeeded on their own conditions. The fix is the gateway's — a
   HITL write overwrites only a version the workspace TRACKS (the
   manifest's citation, or one an inbox entry names) and otherwise gets
   a retryable 409; an untracked object older than
   `UNTRACKED_GRACE_SECS` is fair game (a second escape, "no live
   writer", went with the writer heartbeat)
   (`flint_lean::inbox::hitl_may_overwrite`, used by `put_file` and
   `promote_draft`, with a syncer repro and a gateway test, both failing
   without it). Modelled as `HitlOverwritesTrackedOnly`, a guard on
   `HitlWrite` (`objects[p] = 0 \/ objects[p] = manifest[p] \/
   <<p, objects[p]>> \in inbox`), TRUE in `BLWORLD` and `BLSENT`
   (`LeanBarrierLeaseHolds`, `LeanBarrierLeaseSentinel`) and FALSE in
   every other cfg. `LeanBarrierLeaseHitlOverUncited` is `BLSENT`
   with that constant alone moved: VIOLATED, 18,939,222 distinct states,
   622 s. The grace and no-live-writer escapes are not modelled: the
   guard refuses every untracked overwrite, stricter than the code,
   which is the safe direction for a durability claim and says nothing
   about whether those escapes are themselves safe.

   **They are, since finding 5's fix** (2026-09-14). The commit re-reads
   every citation its own uploads add, so a UI write over an uncited
   upload no longer lets the uploader re-cite over it: its commit
   withholds. `LeanBarrierLeaseHitlOverAnyVerified` lets the gateway
   overwrite ANY current object — a superset of both escapes — in
   `BLSENT` with the sentinel off, and HOLDS `Inv_HITLDurable`,
   `Inv_NoDangling`, `Inv_NoStaleOverride` and `Inv_HITLTracked`:
   15,528,749 distinct states, depth 36, six minutes on a laptop.
   `LeanBarrierLeaseHitlOverAnyUnverified`, the same world without the
   re-read, is finding 4 again (depth 16). So the tracked-only rule is
   now defense in depth, and the writer heartbeat — which the
   no-live-writer escape read — carried no safety; it was removed the
   same day, escape and all, leaving only the grace.
5. **Identical bytes share an etag — found by the LIVE DRILL, not the
   model** (runcv A3, `churn/p14.txt`; syncer finding 13). S3's etag for a
   whole PUT is the MD5 of the bytes. A deletes a path; B rewrites it with
   the SAME bytes and uploads, lease-free, If-Match its baseline — which
   is that very etag, so the PUT lands and the object reads exactly as
   the version A's GC recognises. A's GC deletes it; B's commit cites it.
   This module minted a unique generation for every write, so no GC could
   ever recognise another writer's upload and `Inv_NoDangling` held over
   a race the product had — finding 2's text above even named the fact
   ("a real etag is the content hash") and drew only the adopt arm's
   conclusion from it. The abstraction was the bug, again.
   `AgentWriteSame` (budget `MaxSameBytes`) now writes a generation the
   writer RECOGNISES; back to the baseline's own, the path is dirty only
   by its stat, which `touched` carries. `LeanBarrierLeaseSameBytesUnverified`
   is `BLADOPT` with one such write and the adopt verification ON:
   VIOLATED in 18 steps, in seconds — the trace is the
   drill's, with the writers' names swapped. The fix (79e7dac9) re-verifies
   EVERY citation the commit adds, uploads as well as adoptions, under the
   lease where no GC runs, and withholds what is gone or moved:
   `VerifyUploadedCitations`. `LeanBarrierLeaseSameBytesVerified` is the
   same world with that constant alone moved: HOLDS, 5,498,470 distinct
   states, depth 38, under two minutes — exhaustive on a laptop.

   `LeanProbeUploadWithheld` proves the withhold fires, and its shortest
   trace was a route the drill never showed: B's same-bytes upload lands,
   then A's upload of NEW bytes — If-Match the same etag, which B's PUT
   did not move — lands over it. In that trace B committed first and would
   only have re-cited what the manifest already cited. Ordered the other
   way it is a loss: A COMMITS its edit, and B's commit then cites its own
   generation over A's — the manifest names a version no key holds and A's
   committed bytes sit at the key, cited by nothing. Nothing dangles, so
   no invariant here could see it; `Inv_NoStaleOverride` (a stamp in
   `CASInstall`, barrier lease only) now does. `LeanBarrierLeaseSameBytesOverride`
   is `BLSAME` pre-fix: VIOLATED in 18 steps, and the trace is exactly the
   syncer test written from it,
   `a_peer_put_over_an_identical_bytes_upload_is_not_cited_as_the_old_version`
   — green with the fix, and failing "seq 3 cites x.txt at <seed etag>" with
   the re-read limited to adopted citations. The re-read covers both routes
   because it compares etags, not presence. The invariant is also listed
   in every barrier-lease strict SAFETY run; re-run on the new module,
   `LeanBarrierLeaseAdoptVerified` (641,858), `LeanBarrierLeaseDeposal`
   (2,047,621) and `LeanBarrierLeaseSyncOverlayHolds` (1,493,045) hold with
   their HEAD distinct-state counts unchanged, so the stamp never fired
   there, and `LeanBarrierLeaseEpochOnly`/`RotationOnly` hold.
   `LeanBarrierLeaseHolds` (two paths, HITL, crash, restart) holds with it
   too, exhaustively on the Mac: 21,754,734 distinct states, depth 35,
   eight minutes. The same world WITH a same-bytes write and the fix
   (`LeanBarrierLeaseSameBytesDeep`, opt-in) stopped on the Mac for disk at
   depth 19 (30,265,184 distinct states, 11.6M queued, no violation) — after
   two false positives it surfaced in `Inv_HITLTracked` (below) — and then
   HOLDS exhaustively on the TLC box (i4i.2xlarge, 8 workers, 40 GB heap,
   TLC 2.19, 2026-09-14): 1,169,317,179 states generated, 382,678,936
   distinct, depth 39, two hours and one minute, every `BLINV` invariant
   including `Inv_NoStaleOverride` and `Inv_HITLTracked` (fingerprint
   collision estimate 0.0069 from the actual fingerprints).

**The ack under two writers — a refinement, not a finding.** The first
sentinel run under the barrier lease violated `Inv_AckBoundaryCoherent`
in 13 steps: B honored a sentinel on the fast path (tree clean, manifest
unmoved since its checkout), A's commit then deleted a path, and B wrote
its ok ack. `BoundaryIncoherent` compared B's baseline with the LIVE
manifest, which with one writer nothing could move between the honor
and the ack. An ok ack names a seq (`remote.seq`); the promise is about
the document at that seq, and a later install that merges from theirs
and preserves this workspace's entries is the two-writer rule working.
So under the barrier lease the three ack stamps read `AckedDoc(s)` —
`instSnap`, which now also records the checkout for a never-installed
writer — and `AckedSrc(s)` for the clock. The durability invariants keep
reading the live manifest. FALSE world untouched.

**The ack under two writers, second refinement — a declined repair.** With
`HitlOverwritesTrackedOnly` in place, the two-writer sentinel world
violated `Inv_AckBoundaryCoherent` again, in 16 steps: a HITL write is
consumed by both writers; A's agent edits on top of it and A uploads,
lease-free and uncited; B's commit owes a citation repair for the HITL
generation it integrated, finds the key holding A's upload, and rightly
declines (`repair`'s HEAD guard, `barrier.rs` "moved again or gone: the
next consume reconciles it"); B's ok ack then names a document citing the
pre-HITL generation while B's baseline holds the HITL one. The stamp's
stated harm cannot follow: a reader of that document is sent to a key
that holds neither the superseded generation nor the cited one, so its
conditional read fails into the newer object — and nothing acked is at
risk (`Inv_HITLDurable` held in the same run). So `CASInstall` records the
repairs it declined BECAUSE THE KEY MOVED (`repairMoved`, BarrierLease
only) and `BoundaryIncoherent` exempts exactly those paths, at that CAS. A
repair skipped while the key still held the integrated generation is not
exempt: `LeanSentinelFastPathUnguarded` is still VIOLATED (4,210 states).
With the exemption `LeanBarrierLeaseSentinel` found no violation through
41,958,554 distinct states at depth 18 (the counterexample was at 16) and
was STOPPED, not exhausted — its state queue passed 19 GB on a machine with
10 GB of disk left. Its exhaustive run needs a larger box; until then this
world is "no violation to depth 18", not "holds".

**The larger box answered: VIOLATED at depth 19** (i4i.2xlarge, 2026-09-13,
62,235,430 distinct states). The gate prints only the last 40 lines of a
failing run, so the invariant's name was lost with the spot instance;
`-simulate` reproduced it on the Mac in 21 minutes as
`Inv_AckBoundaryCoherent` (BFS needs more than 35 GB of queue at that depth;
`-dfid` works only with one worker). Analysed, NOT fixed — a third
refinement: B's ok ack names a document AHEAD of B's tree by a peer's change
queued for B's next consume. The ack is honest; the stamp's `#` fires in
both directions while its harm runs in one. It did expose a small contract
bug in the code: step 7 sets `baseline.seq = installed.seq` while the
writer's foreign queue is non-empty, so `remote.seq` reports "no news" for
up to one floor while the tree lags — FIXED in code (2026-09-14:
`integrated_seq` holds while anything waits in the queue;
`remote_seq_reports_news_while_a_peers_change_waits_in_the_queue`).
Until the invariant's refinement lands,
`LeanBarrierLeaseSentinel` is a known red in the gate (91/92 when the box
ran it).

**The two 2026-09-14/15 box runs.** An i4i.2xlarge ran this world again with
its full log kept: `Inv_AckBoundaryCoherent` at depth 19, the name now
observed rather than inferred, and the same world WITHOUT that invariant
violated `Inv_AckImpliesCited` at depth 20 — an ok ack over a delete another
writer's edit outranked (`results/2026-09-14-sentinel-box/`; fixed in the
syncer, 231cff00, and mirrored here as `AckHonest` over CASInstall). An
i4i.4xlarge then ran the mirrored world, two paths, every invariant except
`Inv_AckBoundaryCoherent`, to EXHAUSTION: **no violation, 4,416,800,243
distinct states**, depth 44, queue empty, exit 0 (15,839,180,692 generated,
16 workers, 13 h 59 min; `results/2026-09-15-outranked-box/p2-twopath.log`).
**Read it with its collision estimate:** TLC puts the expected number of
fingerprint collisions at 2.7 (optimistic) and 1.0 (from the actual
fingerprints). A collision merges two distinct states and skips the second
one's successors, so at this size "exhausted" does not rule out one skipped
branch. The 2026-09-14 SameBytesDeep run, at 383M states, estimated 0.0069.
The successor world runs with the view and symmetry (well under half the
states) and a different fingerprint seed. The run left the gate on
2026-09-15: its successor is tranche 7's `LeanBarrierLeaseSentinelImpl`.

**Budgets.** `BLWORLD` is MaxGen=2/MaxBarriers=2 with crash + restart +
HITL — not `LeanSubtree`'s MaxGen=3: two live writers with a crash AND a
restart passed 2.8M states at depth 16 in the first minute with the
queue spilling to disk (the `SENTRESTART` split, for the same reason).
`BLSENT` is `SENTWORLD` (two touches, HITL, no crash), `BLSTALL` is
MaxGen=2/MaxBarriers=2 (A freezes inside its commit section; a third
barrier is only needed for the adopt race, which has its own world),
`BLADOPT` is one path, MaxGen=2/MaxBarriers=3/MaxRestarts=1 (with two
paths the mutation sat past 1M states at depth 20 and its strict control
would have had to exhaust the lot). `BLSAME` is `BLADOPT` with
`MaxSameBytes=1`: the adopt race and the same-bytes race in one world,
so the control certifies both verifications together. The stall world at MaxBarriers=3 was
4–5M states and three to four minutes per run; at 2 it keeps every stamp
site the deposal needs.

**Not modelled, named rather than omitted.** The window closes at the
CAS here and after the GC deletes in the code, so a HITL write inside
the two-request GC window would be a false positive — the GC finding's
world runs MaxHitl=0 and the writer race is the real one. The 412 arm
parks on the FIRST foreign 412 where `upload_one` preserves and
supersedes and parks on the second; and a vanished base (HEAD → 404) is
a create in the code and a park here (review inbox-5) — both make the
model more conservative than the code, and neither is this tranche's.
Generations are unique mints where real etags are content hashes — except
under `MaxSameBytes`, which lets the agent write a generation it already
RECOGNISES (finding 5). Bytes that coincide with a version the writer
never saw are still not modelled: they would have to extend `known`,
which is the amputation stamp's witness. And a same-bytes rewrite after
the scan is absorbed at Finish rather than kept dirty; the next barrier's
own `AgentWriteSame` reaches the same upload. Aliasing cuts the other way
too: the HITL invariants name a UI write by (path, generation), and an
agent that re-creates a UI write's exact bytes carries its generation. The
first same-bytes run in `BLWORLD` stopped on `Inv_HITLTracked` for exactly
that — A consumed a UI write, its agent deleted it, A published the delete
and collected the object, the agent wrote the same bytes back, and A's
upload put them at the key before its commit cited them: nothing lost, and
nothing the invariant counted as tracking. A first repair (accept a LIVE
tree holding those bytes) failed the next run the same way with A's pod
replaced after the upload — finding 10's loss of the agent's own
re-creation, not of the UI write, whose life had ended at A's published
delete. So the invariant now says that directly: a UI write deleted or
overwritten by a party entitled to (a writer that integrated it, or the
UI's own later write) is RETIRED (`gh.hitlRetired`, sticky), which is what
`objects[p] # pr[2]` always meant with unique generations. The ghost is
written only under `MaxSameBytes > 0`, so no other state space moves. The
relaxation was re-run on a known-bad world — `BLWORLD` with the same-bytes
write and `EarlyInboxDrop = TRUE` — and still VIOLATES: a UI write
consumed, then the pod replaced, nothing retired.
Gated mode is asserted off under the barrier lease (`ASSUME`), per D3.
`Inv_NoDeposedPut` and `Inv_NoFencedOkAck` hold vacuously under the
barrier lease — uploads precede the claim and no refused-fenced ack
exists — and are listed in its strict runs for the FALSE-world reader,
not as coverage.

## Ghost-state reduction (2026-09-15)

Most of `gh` is non-vacuity bookkeeping: counters a probe reads and nothing
else does. In a strict or must-fail run, two states that differ only there
have the same successors and the same invariant values, and TLC explored
both. Two fingerprint reductions, neither of which changes an action:

- **`VIEW StrictView`** keeps every `gh` field an action or an invariant
  reads (29) and drops the rest (44), plus `sc`'s `pendReRun` and
  `stageCarried`. `gen-cfgs.sh` adds it to every safety cfg that checks no
  probe; a probe cfg never gets it.
- **`SYMMETRY PathSym`** (`Permutations(Paths)`): nothing in the module names
  a path. It is added wherever the paths start interchangeable (two or more,
  `FreePaths` empty), and never on a `FairSpec` cfg, because TLC's symmetry
  is unsound for temporal properties.

Measured before wiring:

| cfg | before | VIEW | both |
|---|---|---|---|
| `LeanSentinelHolds` | 1,208,901 | 1,018,269 | 512,322 |
| `LeanBarrierLeaseAdoptVerified` (one path) | 641,858 | 320,184 | — |

**Counts under BOTH are not exact, and verdicts are unaffected.** TLC's
`TLCStateMut.fingerPrint` picks the symmetry representative by comparing
FULL states, dropped counters included, and only then fingerprints that
representative's view. Two states with equal views can therefore pick
different permutations and be counted twice. That is an under-merge: two
states are never merged unless their views are permutations of each other,
so nothing reachable is skipped. But the distinct count depends on
exploration order. Measured on `LeanScopedSyncHolds`:

| setting | distinct states |
|---|---|
| no reduction | 623,431 |
| `VIEW` only | 471,015 at 1 and 4 workers |
| `SYMMETRY` only | 318,985 at 1 and 4 workers |
| both | 246,067–246,183 across four runs |

So a count comparison between two runs that use both is not a preservation
check. Compare verdicts there, and compare counts with the reductions
removed (the tranche 7 check below does that).

A view is sound only while no dropped field is read, so that is checked,
not claimed. `view-census.py` runs first in `check.sh` and fails the gate
when any field outside `StrictGh` is read anywhere except a `Probe*`
definition or the update of another dropped field. It also fails when `sc`
is read as a whole record, or when a variable is added to `vars` and not to
the view. `--selftest` applies five edits that each make the view unsound,
and each must fail the census.

## Tranche 7 (2026-09-15): model the implementation

The CHANGELOG named three shapes the code has had since v1.52.0 and the
module did not have. Each changes which interleavings exist.

- **`WriterQueue`: the writer-LOCAL foreign queue.** A merge's foreign
  upserts AND deletions go to `sc[s].fq`, keyed by path. `Consume` drains
  the queue before the shared inbox's entries, settles an entry the
  baseline already holds first ("already"), and applies queued deletions
  after the entries. A restart keeps the queue; a pod replacement takes it.
- **`EmptyInstall`: a barrier that adds nothing installs nothing.** Three
  routes:
  - the skip-on-no-diff fast path runs on every barrier;
  - `PullOnly` queues theirs and takes it as the merge base, with no claim,
    no window and no CAS;
  - a commit whose merge equals theirs skips the CAS, keeps the seq and
    marks no boundary.
- **`Inv_AckBoundaryCoherent`, the third refinement.** An ok ack whose
  document is ahead of the tree by exactly a change waiting in this
  writer's queue is excused: its reader gets newer bytes, not bytes the
  workspace superseded. The known-bad run for the relaxation is
  `LeanBarrierLeaseQueueDropped`: the merge base moves past a peer's change
  and nothing queues it. It is violated in 14 steps.
  The unguarded fast path was to be a second known-bad and is not one on
  this shape. `LeanBarrierLeaseImplFastPathUnguarded` (one path,
  `FastPathGuards = FALSE`) HOLDS exhaustively: 37,058,304 distinct states,
  depth 36, 26 min (2026-09-15, `results/2026-09-15-local/`). That makes it a
  machine-checked redundancy (the 5q method):
  - *Why the guards are redundant here:* the shipped fast path also
    requires an empty consume, and under the barrier lease the ack is
    judged against the writer's own install, not the live manifest. Neither
    the "no repair owed" guard nor the "manifest unmoved" guard is what
    keeps an ack coherent.
  - *Scope:* one path, and `Inv_AckBoundaryCoherent` only. The guards stay
    in the code, where they save a CAS and back other promises.

**The first run of the queue found a shipped defect, in 19 steps
(`LeanBarrierLeaseQueueTombstoneOverHitl`, `Inv_HITLTracked`):**
1. A deletes p1 and publishes.
2. B's pull-only boundary queues the deletion.
3. The UI writes p1 again and is acked.
4. B's next consume ADOPTS the UI write, then applies the queued deletion
   over it.
5. B's window clear drops the write's inbox entry. The acked bytes stay at
   their key, cited by nothing and tracked by nothing.

`a_ui_write_over_a_peers_delete_survives_the_queued_tombstone` failed on the
1.54.0 syncer that way. The fix, in code and here as `TombstoneHeadsKey`: a
queued deletion applies only while the key is absent. That is the rule the
queue's upserts already followed. `LeanBarrierLeaseQueueHolds` is the
control, and `ProbeTombstoneSuperseded` shows the fix fires.

Three more constants came from trace validation (below): each is a step the
first syncer traces took that the module could not.
- `CommitLoadsCurrent`: the commit merges onto the manifest it loads after
  the claim.
- `Upload412Preserves`: a foreign version at upload is preserved as a
  conflict copy, then superseded; the path parks only when that races.
- `DeclaredConfirmsAbsence`: a sentinel honor deletes on a confirmed first
  absence; the fast path refuses a pending one and still advances the
  two-scan clock.

`IMPL` in `gen-cfgs.sh` is all of these, plus `VerifyUploadedCitations`. The
first box run of the one-path sentinel world on this shape omitted that last
one, and stopped in 16 steps on `Inv_NoStaleOverride`: a cfg error, not a
finding (`results/2026-09-15-outranked-box/p1-onepath.log`).

Every earlier cfg keeps all of these FALSE, and that is checked, not
claimed. Every gate run gives the same verdict on this module as on the
module before tranche 7. Every strict run that uses the view without
symmetry gives the same distinct count. The two strict runs whose reduced
counts differed (`LeanScopedSyncHolds`, `LeanBarrierLeaseSyncOverlayHolds`,
both with view and symmetry; see above) were re-run with the reductions
removed, and give the pre-tranche counts exactly: 623,431 and 1,493,045.

**Wider worlds (opt-in, box-scale).** `Writers` is now a constant: a sequence
in start order, substituted in cfgs as `Writers <- TwoWriters`. It generalises
the "B starts no earlier than A" symmetry break to a chain. On the code's
shape:
- `LeanBarrierLeaseSentinelImpl1` and `LeanBarrierLeaseSentinelImpl`: the
  sentinel world, with one and two paths, and every invariant including the
  refined one;
- `LeanBarrierLeaseSentinelImplCrash` (and `...Crash1`, one path): that world
  with a pod replacement and a restart;
- `LeanBarrierLeaseImplThreeWriters`: one path, a UI write, three writers.
  **HOLDS**, exhaustively on a laptop: 5,086,371 distinct states, depth 41,
  12 min 31 s (2026-09-15), fingerprint-collision estimate 5.2E-6. The first
  three-writer world this module has checked;
- `LeanBarrierLeaseImplFastPathUnguarded`: the redundancy question above
  (HOLDS: the guards are redundant for the ack's coherence, one path).

A box started these on 2026-09-15 at 19:27Z and was stopped after 14 minutes
(no more cloud spend; they run locally now). Neither lane had a violation:
- `LeanBarrierLeaseSentinelImpl1` reached depth 28, 29,178,473 distinct
  states;
- `LeanBarrierLeaseSentinelImpl` (two paths) reached depth 18, 27,715,096
  distinct states, with 11.4M queued.

Logs: `results/2026-09-15-impl-box-aborted/`.

**`LeanBarrierLeaseSentinelImpl1` HOLDS, run to exhaustion on a laptop**
(2026-09-15): the one-path sentinel world under the barrier lease, on the
code's shape, with every invariant, including the refined
`Inv_AckBoundaryCoherent`.
- 36,184,256 distinct states, depth 36, 39 min 50 s, 6 workers.
- Fingerprint-collision estimate 1.7E-4 (optimistic), 0.0023 from the
  actual fingerprints.
- The world that has carried the gate's standing red since 2026-09-13 is
  green on the code's shape, for one path.
- Log: `results/2026-09-15-local/impl1.log`.

**`LeanBarrierLeaseSentinelImplCrash1` violates `Inv_HITLTracked`**
(2026-09-15, laptop): 19 states, 8 min 42 s. `Crash1TwoScan.cfg` (the
two-scan rule on) gives the same shape in 21 states, 22 min 29 s. The trace:
1. A publishes x; B runs no barrier, so B's merge base stays at the seed.
2. The UI writes x over A's publish.
3. B's declared barrier consumes the UI write, and the agent deletes x
   between the consume and the scan. `confirm_absences` puts the delete in
   the delete set.
4. A's older publish outranks the delete. The window clear drops the UI
   write's inbox entry.
5. B's pod is replaced before its queue is applied. The acked write is now
   only the object at its key.

The code reaches step 5, and then converges; the model's invariant stops
there. Pinned by two unit tests,
`an_adopted_ui_write_deleted_under_an_older_peer_publish_converges_when_*`:
- *Pod replaced:* the new incarnation's checkout adopts the current object
  (S3-wins) and its barrier cites it. The UI write survives and the agent's
  delete is lost, the safe direction. Refusing the S3-wins arm fails this
  test at the checkout, so that arm is the load-bearing one.
- *Writer survives:* the queued version is superseded, the delete publishes,
  and GC collects the object by the etag the consume integrated.
- *No later checkout and no survivor:* the untracked sweep (finding 10)
  re-tracks the object.

So this is a model-versus-code gap, not a shipped loss: `Inv_HITLTracked`
does not count S3-wins adoption at checkout. A stricter code rule would close
the window itself: do not drop a consumed entry that the commit neither cited
nor retired. It is not built. Logs: `results/2026-09-15-local/crash1*.log`.

## Finding 10 (2026-09-15): convergence after a lost writer

A writer lost for good between its upload and its commit leaves bytes at a
cited key that nothing tracks. That is not a safety violation (nothing
acked is lost), and no invariant in this module could see it. It is a
state property only once nothing can move: `Inv_QuiescentConverged`.
- *Quiescence* is `~ENABLED SyncerProgress`, not `~ENABLED Next`: an agent
  can always delete a file.
- *The claim:* then every object at a CITED key is the citation or is
  tracked by the inbox. An object at an uncited key is a delete whose GC
  never ran: garbage no checkout serves, a leak, not this.

The first three runs of the check each failed for a model reason, and each
is recorded where it was fixed:
1. An unstarted writer counted as quiet (`StartLease` belongs in progress).
2. The fix's append waited forever behind the window of the writer that
   died holding the cell (no window guard: safety never rested on it).
3. A budget of one tracking spent itself on a live writer's in-flight
   upload.

After those, the results:
- `LeanBarrierLeaseOrphanDiverges`: violated in 12 steps.
- `LeanBarrierLeaseOrphanTracked`: `TrackOrphan`, allowed at ANY time,
  holds with every barrier-lease invariant (2,357,000 states, 5 minutes).
- `ProbeOrphanTracked` fires.

The code's sweep is that rule (`lean/syncer/src/untracked.rs`). A
measurement before it: a NEW writer's checkout already healed the
divergence, because its citation repair cites what it adopted, but live
writers that never check out again did not.

## Trace validation (2026-09-15): the model against the code

The gate says the model is internally sound. It cannot notice the Rust
changing underneath, and findings 12 and 13 both lived in exactly that gap.
`trace/` checks the model against what the syncer DID. The method follows
Cirstea, Kuppe, Loillier, Merz and Ranzato, *Validating Traces of Distributed
Programs Against TLA+ Specifications* (2024).

- `lean/syncer/src/tests_conformance.rs` drives two real syncers over the
  in-memory store with the protocol event trace on. It logs what the trace
  cannot see (agent writes and deletes, UI writes, sentinel touches) as
  `conf_*` events into the same stream.
- `trace/ndjson2tla.py` turns a trace into a sequence of model steps.
  - Etags map to generations; equal bytes get one generation.
  - Seqs are offset from the checkout.
  - Budgets fit the trace exactly.
  - The event-to-action table is in its docstring.
- `trace/TraceLean.tla` advances a cursor only through the action each step
  names, with the reported values bound: adoptions, removals, counts, the
  installed seq, the GC's result, the ack's status. One step is silent: the
  GC skips a delete the merge outranked without an event.
- **Accepted** means TLC reached the end (`TraceIncomplete` violated).
  **Rejected** means it exhausted every way to follow the trace;
  `TRACE-REACHED` names the step it could not take.

`trace-check.sh` requires three things:
- every committed trace in `trace/traces/` is accepted (5);
- five mutations, each one fact of a real trace corrupted by `mutate.py`,
  are rejected at the corrupted event: an upload's etag, a merge's foreign
  count, a tombstone's action, a consume's action, an ack's status;
- five controls are rejected. Each turns off one model correction a trace
  forced (the commit token, the 412 arm, declared deletes, the tombstone
  fix, the writer queue), and the trace that forced it must stop exactly at
  the step that correction governs.

**What it found on its first run** (before those corrections): of five
traces, three were rejected, at the commit's CAS (step 17), an upload that
superseded a foreign version (13) and a declared barrier's scan (14). Each
was the model lagging the code, fixed as the three constants above. The
queued-tombstone finding came from modelling, not from a trace; its
scenario's trace was captured on the fixed syncer and passes.

Limits, named:
- The traces are sequential: each barrier runs to completion before the
  other writer's starts, so they check what the actions DO, not every
  interleaving. Phase 2, below, is the concurrent case.
- Regenerate with
  `FLINT_SYNC_CONFORMANCE_DIR=$PWD/lean/formal/trace/traces cargo test --lib conformance_`
  in `lean/syncer`. Traces from a changed syncer that the model rejects are
  the point of the exercise.

## Trace validation, phase 2 (2026-09-15): a LIVE leg of the storm drill

Phase 1 replayed the conformance harness: two writers, one barrier at a
time, on a double. Phase 2 replays what six writers and a UI actor did to
real S3 — `lean/e2e/writers-live/results/2026-09-15-drill/`, round 3, the
churn leg with the fixed binary.

`trace/drill2tla.py` projects a leg onto ONE PATH and writes the NDJSON
phase 1 already validates, so the checked converter does the rest. Nothing
is inferred that the drill logged: the agents' writes come from their
journals, the UI's from its own (with the etag the gateway answered).
`trace/drill-check.sh` runs it and then corrupts it.

**The leg is a behaviour of the model.** `churn/p23.txt` of `R3-S2-churn-ui`:
5,940 trace lines, 3,258 model steps, six writers, accepted to the end
(depth 3,259, 20,311 distinct states, 3 s). Three mutations of that same
projection — a consume that adopted reported as superseded, a withheld
citation reported as still there, a skipped GC reported as a delete — are
each rejected. Logs: `results/2026-09-15-trace-phase2/`.

Getting there took four corrections to the MODEL, each a place where the
model described something the code does not do:

1. **The handoff names the waiters the holder read at its CLAIM**
   (`HandoffAtClaim`). `epoch_handoff` hands the cell to the head of
   `lease.waiters` — the list as of the claim — and writes the REST OF THAT
   LIST back as the queue. The model handed off to the head of the queue as
   it stands at the release. **This breaks the ticket's whole purpose:**
   with the shipped rule modelled, `NoStarvation` — "with the ticket, every
   queued writer eventually holds the cell" — is VIOLATED. TLC's
   counterexample is a two-state cycle: B holds the cell having read an
   empty waiter list, A takes a ticket while B runs, B's release writes the
   stale (empty) list back and names nobody, B claims again, A waits
   forever. Not observed in any drill (the storm's waits are tens of
   milliseconds and O5 requires zero claim deadlines), and the code's
   handoff DOES re-read on a 412, so a waiter whose enqueue moved the token
   is normally honoured. The fix belongs in `release`: name the head of the
   list as of the handoff, not as of the claim. Log:
   `results/2026-09-15-trace-phase2/liveness-handoff-at-claim.log`.
2. **A commit section can end without installing** (`AbandonOnStoreError`).
   S3 answered a window-open PUT with 409 ConditionalRequestConflict; the
   barrier returned the error, released the cell and kept its pending
   sentinel. The model had no such step — its only abandon was a fence.
   **The event trace does not record this at all**: only the prose line
   `Publish honor failed (pending kept, retrying)` says so, which is why
   the projector has to infer it from a barrier that never ends. A trace
   event for it is the obvious rig fix.
3. **A published delete clears the baseline only if the GC COLLECTED the
   object** (`BaselineKeepsUncollected`). `report.deleted` is pushed by the
   GC's collect arms, and step 7 clears the baseline from that list — so a
   delete whose GC skipped (the key held bytes this writer does not
   recognize) keeps its baseline entry, the tree reads as locally deleted at
   the next consume, and an incoming version is PRESERVED rather than
   adopted. The model cleared the baseline for every published delete.
4. **A claim, a wait and the counts are facts about every path**
   (`ProjectedTrace`). A projected replay cannot recompute a scan's upload
   count, a merge's foreign count, an ack's status, or why a barrier
   claimed, so those checks are dropped WHERE THE TRACE CANNOT CARRY THEM
   and only there; everything path-scoped is checked as phase 1 checks it.

Open: `churn/p47.txt` — a path with deletes, a skipped GC and a preserve —
is still rejected, now at a consume that adopts after that skipped GC
(`results/2026-09-15-trace-phase2/p47-open.log`). Either the model's
consume or the code's is wrong there; the trace says exactly which step to
read. Constants 1-3 are FALSE in every gate cfg, so the gate's 110 runs
explore the state spaces they always did.

## Tranche 3 candidates (in review-priority order)

1. Layout/multi-subtree (P2/P3): root-owner designation, foreign
   subtree entries at checkout.
2. Window liveness: the HITL starvation bound (needs fairness — keep
   it OUT of the safety gate; the WF ping-pong trap lives here).
3. Refine ClaimB into the poll protocol with a torn heartbeat task
   (the self-recognition + rotation composition).
4. ~~Product 1 — boundary × barrier × inbox with the deposal arm.~~
   **DONE** (above). Two shipped defects, and three rejected invariant
   formulations before the promise was stated correctly.
5. **Pair the other products that share an action.** "Every arm is
   modelled" is not "every pair of arms that meet in one action is
   modelled" — product 1 × product 2 proved the difference. `SyncScope`
   with `GatedCitation` is the obvious next one: a scoped sync and a
   citation lane both advance `inst_base`, by different rules.
