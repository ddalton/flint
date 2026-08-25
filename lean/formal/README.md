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
./check.sh          # the 20-run gate (~minutes)
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
