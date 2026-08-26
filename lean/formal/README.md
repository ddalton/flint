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
./check.sh          # the 27-run gate (~75 s)
./gen-cfgs.sh       # regenerate the cfg matrix
```

Twenty-four runs, ALL required: 5 strict (must hold), 11 mutations (must find
their designated counterexample — a model that cannot rediscover its bug
classes proves nothing), 8 probes (must be violated — each names an
ACTION via a ghost only that action writes; probe the action, never the
situation). `LeanSubtreeDeep.cfg` is the rich-budget breadth run — an
opt-in overnight job, not in the gate.

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

Nine runs (27 → **36**), behind `GatedCitation`, which is FALSE in every
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

## Tranche 3 candidates (in review-priority order)

1. Layout/multi-subtree (P2/P3): root-owner designation, foreign
   subtree entries at checkout.
2. Window liveness: the HITL starvation bound (needs fairness — keep
   it OUT of the safety gate; the WF ping-pong trap lives here).
3. Refine ClaimB into the poll protocol with a torn heartbeat task
   (the self-recognition + rotation composition).
4. **Product 1 — boundary × barrier × inbox with the deposal arm.**
   Still owed. `Inv_NoNonceOrphan` under coalesce + crash + restart is
   an interleaving property unit tests sample rather than search, and
   `settle_pending_at_startup` is justified today by one ordering out of
   many. Product 2's return (a live defect on the first strict run)
   is the argument for doing it.
