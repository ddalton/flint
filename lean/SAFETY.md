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
| an ok ack means the boundary is INSTALLED, at the seq it names | `Inv_AckImpliesCited` | `sentinel.rs`, ack after `note_boundary` |
| the boundary it names cites everything this workspace integrated | `Inv_AckBoundaryCoherent` | citation repair in `barrier.rs` step 4 |
| a fenced writer never answers ok | `Inv_NoFencedOkAck` | `refuse_if_read`, `verify_not_deposed` |
| a consumed publish request is always answered | `Inv_NoNonceOrphan` | the pending record, `sentinel.rs` |
| the ack and the bucket name the same clock | `Inv_BoundaryNamesItsClock` | `boundary_source` stamped on the install |
| an acked UI write is never destroyed unrecorded | `Inv_HITLDurable` | the 412 preserve, `consume-dirty`, `preserve_conflict_copy` |
| an acked UI write stays tracked until a writer RETIRES it | `Inv_HITLTracked` | the inbox, the writer queue, the untracked sweep, and the collector's decision — not its success (§4.4) |

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
| the store honours `If-Match` on DELETE (the file collector, and only it) — **FALSE on two of the three stores we actually run against**: S3 enforces it; Ozone 2.2.x (L-27) and MinIO (L-102, measured 2026-09-15) both accept the header and ignore it | the same probe | the collector could take the version another writer's commit is about to cite — precisely the model's refuted `LeanBarrierLeaseGCUnconditional`. The syncer now turns the COLLECTOR off instead of refusing the workspace: retired objects are left in the bucket, cited by nothing (`leaked=` on the barrier line, one warning per barrier). Ozone 2.2.x is this case (HDDS-14907, L-27) — and so is **MinIO** (RELEASE.2025-09-07, measured: it enforces `If-Match` on GET and `If-None-Match` on PUT, and ignores `If-Match` on DELETE — `lean/e2e/results/minio-conditional-delete-2026-09-15.md`), which is most of the e2e rig. The loss becomes storage growth. **Until 2026-09-18 it was worse than a cost:** the leaked object superseded the peer's own tombstone in every other writer's consume (the rule looked at the key, not at what the delete retired) and their citation repair re-cited it, so one `rm` on a two-writer workspace flapped forever (review 2026-09-18 H1, `LeanBarrierLeaseLeakResurrects`, `Inv_NoDeleteResurrected`). The tombstone now carries the etag the delete retired and applies over it |
| an etag names the bytes (a content hash), so identical bytes share one | modelled (`MaxSameBytes`), which is what makes finding 13 reachable | the same-bytes findings would not apply; a different set would |
| the local filesystem gives atomic rename and honest `lstat` | assumed | the scan's two-scan rule and the temp-then-rename writes lose their basis |
| exactly one syncer owns a workspace tree on a node, and nothing else edits it mid-barrier | the CSI driver's worker-per-volume | a scan can publish a half-written file |
| clocks are never used for ordering | by construction: the cell's epoch and the manifest's seq order everything | — |

## 3. How the claim is checked today

| evidence | what it covers | size |
|---|---|---|
| the formal gate (`lean/formal/check.sh`) | every invariant `COVERAGE.md` lists, exhaustively, per world. Which promises are checked on the SHIPPED shape is §4.11; three enforcers named in §1 are checked by no run at all (§4.12) | 136 runs: 37 strict worlds, 54 mutations, 45 probes (the chunk-module runs among them) |
| refutation | that an invariant CAN fail — a mutation that must violate it | 54 mutations; every invariant `COVERAGE.md` lists has at least one (`refuted by`); the three of §4.12 have none |
| trace validation, phase 1 | the model is the code, on 5 scenario traces, in CI | 5 accepted, 5 mutations + 5 controls rejected |
| trace validation, phase 2 | the model is the code on a REAL 6-writer run on S3, with the invariants checked while replaying | one leg, one path: 3,258 steps, no invariant violated |
| the live drill | the binary is the code: each fix has a control arm that fails | host legs H1-H6, storm legs S0-S5 (2026-09-15) |
| the unit battery | each finding pinned by a test whose control fails it | 232 lean tests |

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
   never collected. On a store without a conditional DELETE, neither are
   the objects the collector gives way on. Not a loss; a cost.
8. **The untracked window.** A writer lost between its upload and its
   commit leaves an object nothing tracks until the sweep runs
   (`FLINT_SYNC_UNTRACKED_SWEEP_SECS`, 3600 s by default) or some writer
   checks out.
9. **Bytes below the protocol.** CRC-64 is verified on consume and on
   checkout; bit-rot inside the store is the store's problem.
10. **Ozone.** See §2.
11. **Coverage on the SHIPPED shape (review 2026-09-18, H8).** The gate
    certifies the ten barrier-lease invariants and `Inv_NoDeleteResurrected`
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
    writer and adopted otherwise). And `Inv_NoResurrection` (S4 row 1) is
    a ghost written only under the mutation constant, so its strict runs
    are tautologies; the code's rule (never re-materialise over a live
    tree, `checkout.rs`) is in no model action. Open: restate over state,
    with a mutation that fails it.
13. **The delete request carries no epoch (C2's residual).** S3's
    DeleteObject has no fencing token. Since 2026-09-18 the collector
    renews before a delete whenever its last cell WRITE is older than
    `renew_within_secs` (20 s; a deposal needs the token still for 60 s),
    which fences a holder deposed after its CAS landed
    (`LeanBarrierLeaseStragglerGC`, the mutation; `…GCFenced`, the
    control). A stall between that renew and the DELETE it licenses is
    the window that remains; the model's per-delete observation is its
    bound, and only a store-side fencing token would close it.

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
  in;
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

## 5. The open list

| # | what would close it | cost |
|---|---|---|
| 1 | ~~run the three-writer world in the gate~~ **done 2026-09-15** | — |
| 2 | ~~a refutation for `Inv_CommitExclusive` and for `Inv_CellHeldByHolder`~~ **done 2026-09-15**: a claim that reuses the cell's epoch breaks the first, a claim that does not stamp it breaks the second | — |
| 3 | ~~correct `Inv_HITLTracked` in BOTH arms~~ **done 2026-09-16** (§4.4; the TRUE arm re-ran 2026-09-19 to depth 25 without violating it and stopped on H10's route — still not a hold): supersession now follows the collector's DECISION, not physical destruction, and bytes at a still-cited path count as tracked. Re-run on all three mutations that require this invariant to fail — each still finds it violated — and the collector-off world now holds with all ten. The count in `COVERAGE.md` went 4→3 refutations because the fourth, `…CollectorOffHitlTracked`, never refuted the protocol: it pinned this invariant's own overreach, and the repair is what retires it. **Still open:** the crash world at `OrphanTrack = TRUE` — the FALSE arm violated as it should (2026-09-16), the TRUE arm was stopped for disk at 158M states — see §4.4 | half done; the TRUE arm needs a box |
| H10 | **an ok ack after a restart names a boundary a peer's delete already moved past** (review 2026-09-18 doc, H10; found 2026-09-19 by the crash world at `OrphanTrack = TRUE`, depth 25): the writer's declared write is published, the writer restarts before the ack is written, a peer deletes the path, and the re-run honours the pending with a pull-only install of the peer's document and acks ok while the tree still holds the declared bytes. `AckHonest` cannot see a pull-only. The ack should name the boundary the pre-restart barrier installed, or refuse | open; model-confirmed, code untraced |
| 4 | finish the replays: bisect the earliest step where the model's manifest disagrees with the leg's for a path (the `cas` seqs and `observed` etags make it mechanical), then the `abandon` in R2-S2. Two blockers closed 2026-09-15 | a day |
| 5 | replay every path of every storm leg in CI, invariants on | a day, then free |
| 6 | exhaust the two-path sentinel world (box-scale) | a TLC box, ~$5-20 |
| 7 | ~~refuse a store that fails `probe-conditional` instead of documenting it~~ **done 2026-09-15**: the syncer probes before its first verb — a broken conditional PUT refuses the workspace, a broken conditional DELETE turns the collector off (`conformance.rs`) | — |
| 8 | an inductive invariant (TLAPS) for S2 and S5 — of whose rows only `Inv_NoDangling`, `Inv_CommitExclusive` and `Inv_CellHeldByHolder` are stated over states today; the rest are ghost stamps | weeks; the only route to a claim that does not say "in this world" |
| 9 | `IMPL` variants of the sentinel, removal, narrow and sync worlds, so S1 rows 1-5, S3 rows 2-3 and S4 rows 2-3 are certified on the shipped shape (§4.11) | a day on a laptop for the one-path worlds; the two-path sentinel needs a box |
| 10 | retire or restate the three enforcers no run checks, and `Inv_NoResurrection` over state (§4.12) | half a day |
| 11 | the review of 2026-09-18's other routes — a restart that releases a deposal without the rotation (H2), the inbox helpers' 409 arm (H3), removals without an etag (H4), the directory-symlink swap (H5), the multi-writer checkout's half boundary (H6), the consume's re-stat window (H7) — each with a test first: `docs/plans/flint-lean-protocol-review-2026-09-18.md` | two to three days |

Regenerate `COVERAGE.md` with `python3 lean/formal/coverage.py` and check
it with `--check`.
