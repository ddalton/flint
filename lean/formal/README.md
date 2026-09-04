# The lean formal models

TLA+/TLC models for **flint-lean** (checkout/publish + gateway — plan of
record: `docs/plans/flint-lean-plan.md`). Model BEFORE code, the
FlintExtents posture: the module was written against plan v2 and the
review's confirmed counterexamples, and the sidecar implementation
(`lean/sidecar/`, the flint-lean crate) is written to it.

**Deliberately separate from `formal/`** (the flint corpus and its
196-run gate): lean is a separate system that consumes `tier::store` as
a library. Nothing here is wired into `scripts/check-tla.sh`.

## Running

```
./check.sh          # the 49-run gate (~4 min)
./gen-cfgs.sh       # regenerate the cfg matrix
```

Seventy-three runs, ALL required: 15 strict (must hold), 30 mutations
(must find their designated counterexample — a model that cannot
rediscover its bug classes proves nothing), 28 probes (must be violated
— each names an ACTION via a ghost only that action writes; probe the
action, never the situation). The three numbers are `grep -c "^strict_run "`,
`grep "^mutation_run " | grep -vc Probe` and `grep "^mutation_run " |
grep -c Probe` in `check.sh` — they had drifted from the script twice,
so they are stated as a recipe rather than a claim. `LeanSubtreeDeep.cfg` is the rich-budget breadth run — an
opt-in overnight job, not in the gate.

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
| `Inv_NoTornRead` | a reader never finds a chunk its pointer named, absent | **not a gate run** — `LeanChunkGCSlowReader.cfg` VIOLATES it, and that is the finding: a reader is safe for `Retain` PUBLISHES, not for a duration (§8.2). Kept as a cfg so the bound stays machine-checked rather than prose |

## The module: LeanSubtree.tla

One subtree; sidecars A (first holder) and B (takeover successor); the
gateway abstracted to its bucket effects; the bucket substrate: lease
cell, manifest (seq + per-path citation), whole-file objects
(generation = ETag), the inbox/window cell. Generations model ETags;
If-Match is equality on them; whole-PUT is atomic.

Invariants:

| Invariant | Claim | Mutation that must violate it |
| --- | --- | --- |
| `Inv_HITLDurable` | an acked HITL write is never silently lost | `LeanAmputation` (direct manifest bump + whole-rewrite writer), `LeanLocalWins` (the inherited flush.rs LOCAL-WINS 412 arm), `LeanGCUnguarded` (unguarded GC delete) |
| `Inv_NoDangling` | every cited manifest entry has a live object | `LeanDanglingOrder` (v1 order: upload→delete→CAS) |
| `Inv_NoStragglerInstall` | a deposed writer's manifest CAS never lands | `LeanNoRotate` (no takeover rotation) |
| `Inv_NoDeposedPut` | a deposed writer's data PUT never lands | `LeanNoEpochCheck` (rotation alone — proves rotation does NOT cover the data path) |
| `Inv_NoResurrection` | a container restart never resurrects an unpublished delete | `LeanRematerialize` (re-checkout over a live tree) |
| `Inv_SyncNeverDestroysDirty` | the sync verb never destroys genuinely-dirty local work without surfacing it | `LeanSyncStaleDirt` (sync judging dirt from the last barrier's snapshot) |

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
1. **The amputation stamp needed a legitimacy term.** A sidecar that
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
   at my last install) does not — collapsing them makes a sidecar
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
  `lean/sidecar/src/barrier.rs` does this), not just the reference.
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
run passed 1.3 GB of TLC scratch without terminating: two live sidecars,
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
