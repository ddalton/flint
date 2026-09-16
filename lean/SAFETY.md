# What lean guarantees about your bytes, and what it does not

This is the safety claim for the lean protocol: the syncer (`lean/syncer`),
the gateway (`lean/gateway`) and the store contract they rest on
(`crates/flint-store`). It exists because "113 model runs are green" is not
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
| an ok ack means the boundary is INSTALLED, at the seq it names | `Inv_AckImpliesCited` | `sentinel.rs`, ack after `note_boundary` |
| the boundary it names cites everything this workspace integrated | `Inv_AckBoundaryCoherent` | citation repair in `barrier.rs` step 4 |
| a fenced writer never answers ok | `Inv_NoFencedOkAck` | `refuse_if_read`, `verify_not_deposed` |
| a consumed publish request is always answered | `Inv_NoNonceOrphan` | the pending record, `sentinel.rs` |
| the ack and the bucket name the same clock | `Inv_BoundaryNamesItsClock` | `boundary_source` stamped on the install |
| an acked UI write is never destroyed unrecorded | `Inv_HITLDurable` | the 412 preserve, `consume-dirty`, `preserve_conflict_copy` |
| an acked UI write stays tracked until legitimately superseded | `Inv_HITLTracked` | the inbox, the writer queue, the untracked sweep |

**S2. A citation always resolves to the bytes it names.** A reader that
follows the manifest never gets a hole or the wrong version.

| | enforced by |
|---|---|
| every cited path has a live object | `Inv_NoDangling` |
| on a versioned bucket, the exact cited version is still stored | `Inv_CitedVersionLives` |
| the reaper never takes the version a path currently reads as | `Inv_NoUncitedGC` |
| a commit never cites its own upload over bytes the key no longer holds | `Inv_NoStaleOverride` |

**S3. A concurrent write is never dropped silently.** Where two writers
disagree, both versions survive: one in the tree or the manifest, the other
as a conflict copy with a record naming it.

| | enforced by |
|---|---|
| no install drops the last tracked reference to acked bytes | `Inv_HITLDurable` |
| `sync` never destroys genuinely dirty local work | `Inv_SyncNeverDestroysDirty` |
| a merge base never advances past a change neither integrated nor surfaced | `Inv_NoForeignLost` |

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
| a deposed writer's manifest CAS never lands | `Inv_NoStragglerInstall` |
| a deposed writer's data PUT never lands | `Inv_NoDeposedPut` |
| a boundary is all-or-nothing | `Inv_BoundaryAtomic` |

**S6. Every tree eventually equals the published boundary.** Convergence,
not safety: `Inv_QuiescentConverged` — once nothing can move, every object
at a cited key is that citation or is tracked for a writer to integrate.

## 2. What the protocol ASSUMES

A claim with unnamed assumptions is a claim about nothing.

| assumption | how it is verified | if it is false |
|---|---|---|
| the store honours `If-None-Match` / `If-Match` on PUT (every upload, the manifest CAS, the lease cell) | probed by the syncer before its first verb (`conformance.rs`), and by `flint-sync probe-conditional` | **no guarantee holds** — arbitration degrades silently to last-writer-wins. The syncer now REFUSES the workspace (`EXIT_REFUSED`) rather than run on such a store |
| the store honours `If-Match` on DELETE (the file collector, and only it) | the same probe | the collector could take the version another writer's commit is about to cite — precisely the model's refuted `LeanBarrierLeaseGCUnconditional`. The syncer now turns the COLLECTOR off instead of refusing the workspace: retired objects are left in the bucket, cited by nothing (`leaked=` on the barrier line, one warning per barrier). Ozone 2.2.x is this case (HDDS-14907, L-27) — the loss becomes storage growth |
| an etag names the bytes (a content hash), so identical bytes share one | modelled (`MaxSameBytes`), which is what makes finding 13 reachable | the same-bytes findings would not apply; a different set would |
| the local filesystem gives atomic rename and honest `lstat` | assumed | the scan's two-scan rule and the temp-then-rename writes lose their basis |
| exactly one syncer owns a workspace tree on a node, and nothing else edits it mid-barrier | the CSI driver's worker-per-volume | a scan can publish a half-written file |
| clocks are never used for ordering | by construction: the cell's epoch and the manifest's seq order everything | — |

## 3. How the claim is checked today

| evidence | what it covers | size |
|---|---|---|
| the formal gate (`lean/formal/check.sh`) | every invariant above, exhaustively, per world | 113 runs: 27 strict `LeanSubtree` worlds, 72 mutations, 14 chunk-module runs |
| refutation | that an invariant CAN fail — a mutation that must violate it | 72 mutations; **every invariant in §1 has at least one** (`refuted by` in `COVERAGE.md`) |
| trace validation, phase 1 | the model is the code, on 5 scenario traces, in CI | 5 accepted, 5 mutations + 5 controls rejected |
| trace validation, phase 2 | the model is the code on a REAL 6-writer run on S3, with the invariants checked while replaying | one leg, one path: 3,258 steps, no invariant violated |
| the live drill | the binary is the code: each fix has a control arm that fails | host legs H1-H6, storm legs S0-S5 (2026-09-15) |
| the unit battery | each finding pinned by a test whose control fails it | 222 lean tests |

## 4. What is NOT claimed

1. **Unboundedness.** Every world is small — one to three writers, one or
   two paths, two barriers, a handful of generations. TLC exhausts the
   world, not the protocol. There is no inductive proof, so nothing here
   rules out a failure that needs a fourth writer or a third path.
2. **Three writers, only in one world.** The gate now runs the
   three-writer world (2026-09-15); it carries 9 of the 21 invariants. The
   other 12 are still checked with two writers only.
3. **Refutation is now complete, and that is recent.** Every invariant in
   §1 has at least one mutation that must make it fail. Until 2026-09-15
   two did not (`Inv_CommitExclusive`, `Inv_CellHeldByHolder`), and their
   nine green worlds each proved nothing.
4. **The crash world disagrees with the code.** `Inv_HITLTracked` flags a
   state the code recovers from (a checkout adopting an object newer than
   its citation); the invariant does not credit that arm.
5. **One replay is open.** `churn/p47.txt` — deletes, a skipped GC and a
   preserve — is still rejected mid-trace.
6. **Liveness.** Two runs, and the ticket's fairness is weaker in the code
   than in the model (a waiting writer does not always hold a ticket). No
   starvation has been observed; none is ruled out.
7. **Storage growth.** Conflict copies under `.flint/lean/conflicts/` are
   never collected. On a store without a conditional DELETE, neither are
   the objects the collector gives way on. Not a loss; a cost.
8. **The untracked window.** A writer lost between its upload and its
   commit leaves an object nothing tracks until the sweep runs
   (`FLINT_SYNC_UNTRACKED_SWEEP_SECS`, 3600 s by default) or some writer
   checks out.
9. **Bytes below the protocol.** CRC-64 is verified on consume and on
   checkout; bit-rot inside the store is the store's problem.
10. **Ozone.** See §2.

## 5. The open list

| # | what would close it | cost |
|---|---|---|
| 1 | ~~run the three-writer world in the gate~~ **done 2026-09-15** | — |
| 2 | ~~a refutation for `Inv_CommitExclusive` and for `Inv_CellHeldByHolder`~~ **done 2026-09-15**: a claim that reuses the cell's epoch breaks the first, a claim that does not stamp it breaks the second | — |
| 3 | correct `Inv_HITLTracked` to credit checkout's S3-wins adoption, then re-run the crash world | an afternoon |
| 4 | finish the `churn/p47.txt` replay: decide whether the model's consume or the code's is wrong | unknown until read |
| 5 | replay every path of every storm leg in CI, invariants on | a day, then free |
| 6 | exhaust the two-path sentinel world (box-scale) | a TLC box, ~$5-20 |
| 7 | ~~refuse a store that fails `probe-conditional` instead of documenting it~~ **done 2026-09-15**: the syncer probes before its first verb — a broken conditional PUT refuses the workspace, a broken conditional DELETE turns the collector off (`conformance.rs`) | — |
| 8 | an inductive invariant (TLAPS) for S2 and S5, the two that are stated over states rather than ghosts | weeks; the only route to a claim that does not say "in this world" |

Regenerate `COVERAGE.md` with `python3 lean/formal/coverage.py` and check
it with `--check`.
