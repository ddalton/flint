# Formal models — the replica-lifecycle machine, the snapshot protocol, the multi-process claims layer, the pNFS truncate gate, the block-layout extent allocator, and the block admission layer

Six spec modules plus two probe modules (`FlintA2Probe`, `FlintExtentsProbe` —
ghost-witness overlays on `FlintReplication` / `FlintExtents`), one gate
(`scripts/check-tla.sh`, **one hundred and nineteen** TLC runs).

Both counts here have drifted before, in both files, because nothing
regenerated them. They do now:

```
ls formal/*.tla | wc -l
awk '/^(strict_run|mutation_run|liveness_mutation_run)[ ]/ {print $2}' \
  scripts/check-tla.sh | sort | uniq -c | sort -rn
```

`FlintReplication.tla` models the durability core every flint orchestrator
mutates: leg lifecycle states, the writer set, epoch cuts, raid superblock
generations, the F36c freshness gate, the failure taxonomy the P4 work
made explicit (crash-stop vs **silent omission** vs verified death) —
since tranche 2: **hot rejoin** (kept-payload readmission, the shared-base
ancestry check, the Scrub demotion), **torn writes** (crash between
replica write and client ack), and the **F48 zombie head** (a partitioned
server still acking writes until admission severs it) — and since
tranche 3: **LastResortServe**, the stale-only-survivor runbook override
(operator, risk surfaced).  **2026-07-29 audit correction:** the claim
that stood here — "the code itself Defers" — was FALSE.  The shipped
NodeStage has two AUTOMATIC availability arms, now modeled behind
constants with their own teeth runs: the freshness gate's 180s defer
deadline (`GateDeadline`: serve-with-risk on transient evidence — so
`Inv_NoFalseRisk` is a theorem only of the idealization) and the
2-base-floor forced-stale admission (`StaleFloor`: a record-Stale leg
auto-admitted, serving reads, `StaleReplicaAdmitted` event only).  The
same audit exposed that the "SPDK raid1 examine" generation belt was
fiction — flint creates raids `superblock:false` — so `NewestOf` is now
documented as the `MonitorCurrent` TIMING AXIOM (the monitor's
stale-mark lands before a reassembly reads the record), with the
`MonitorLag` run proving what its absence costs. The S2
model-first tranche added the **R2 claim arbitration** (the F43
machinery): catch-up and admission run under a leased per-volume claim,
with admission priority — `AdmissionNotStarved` is the theorem, and the
F43 mutation must rediscover the starvation lasso. This is the formal
half of the S2 bounce-free-RWX-admission design
(`spdk-csi-driver/docs/s2-bounce-free-rwx-admission.md`). The
**maintenance tranche** models the csi-node roll landmine ahead of its
fix (`spdk-csi-driver/docs/maintenance-drain-csi-node-roll.md`): planned
tgt restarts are indistinguishable from failures to the data plane
(`Responsive`), and the fix is three separately-necessary guards —
drain-before-restart (`MaintFence`), a readmission barrier
(`MaintBarrier`; pod-readiness is NOT it), and a leased suppression mark
(`MaintLease`). `Inv_PlannedRollNeverCausesOutage` (zero real failures ⇒
a roll alone never downs the volume) and `MaintenanceEventuallyLifts`
(no mark outlives its purpose on a live leg) are the theorems; three
roll mutations must each rediscover their loss.

`FlintClaims.tla` models the **multi-process claims/window layer** — the
F50/F53 axis, which `FlintReplication`'s single `claim` variable
deliberately assumes away (it IS the single-process assumption).
`volume_claims.rs` is in-process: two controller-shaped processes (a
helm rolling-upgrade overlap, the vestigial operator pod — F50; the
dashboard backend — F53) each have a private registry, and neither can
see the other's in-flight work. The module decomposes the hot-rejoin
window into open/commit — an atomic `Admit` cannot be raced, and the
F50 loss lives BETWEEN intent and flip — and models the two PROTOCOL
layers of the fix stack: the marker grace (as an ordering axiom: a
marker outlives the grace only once no live window owns it — the
quantitative content of the 300s) and the P1 kube-Lease (with honest
tick-granularity semantics: it gates starting ops, never in-flight
ones, and can spuriously depose a live holder). `Inv_NoColdAdmission`
is the theorem. The mutation and the no-leader strict run together
state the layering exactly: the grace carries scrub-vs-live-window
safety (its mutation finds F50's E_f-scrub collision WITH the Lease
on, via a deposed-but-alive leader's in-flight dispatch); the Lease
carries ownership determinism and churn-freedom, NOT safety (the
no-leader run holds — which is what the runaj A/B showed live: the
dashboard's window was correct, just unowned). The role grant itself
(F53's CSI_MODE conflation) is configuration, killed by
`orchestrator_role.rs`, not modeled.

`FlintSnapshots.tla` (tranche 3) models the **snapshot protocol** at
block-content level — the layer where content is `[Blocks -> version]`
and the hazards are about which version survives a chain walk, which the
write-set abstraction above deliberately cannot express: epoch cuts onto
a retained chain, shallow per-epoch deltas, oldest-first walk order,
based sessions from a shared base vs full rebuilds, retention drops with
blobstore cluster absorption. Its theorem (`Inv_SessionFaithful`): every
completed copy session delivers exactly the cut. Sessions are atomic —
crash *inside* a session is the crash-sweep sim harness's job.

`FlintTruncate.tla` (2026-07-31) models the **pNFS truncate gate** — the
one correctness invariant the pNFS layer holds in its own hands. The rest
of pNFS has a referee: layout op sequencing is RFC 8881 with pynfs
adjudicating, single-client data integrity is fsx/fsstress's job, and DS
failure is not re-placed at this layer at all (`layout.rs` mutates
`placements` only on load, delete and rename — placements are **pinned**),
so a dead DS is an availability event handled by the lvol underneath,
which is `FlintReplication`'s machine. What is left unrefereed is the
window between the MDS stub's size changing and N data servers being cut,
and `truncate_dirty` is the gate that is supposed to make it
unobservable.

Two theorems. Both hold as shipped; one of them did not when the module
was written, and the run that proves it now must keep failing:

* `Inv_ClearImpliesFlushed` — the gate's own claim: whenever the mark is
  absent, no DS holds content past the MDS size. **HOLDS.** This matters
  because `clear_truncate_dirty_if` looks unsafe on inspection: the retry
  task re-reads the deepest pending size and clears with the value it
  just read, which is a repair writing its own guard's input (the F62
  shape). Separating the re-read from the clear as distinct steps lets
  TLC interleave a fresh SETATTR between them, and the `confirmed <= min`
  predicate survives it. The `BlindClear` mutation must rediscover the
  loss, so the predicate is load-bearing rather than defensive.
* `Inv_NoStaleServe` — no client is ever served content past the MDS
  size. **Still does not hold on shipped code**, and the shipped cfg
  does not list it. F65 (no recall at all) is fixed, and so are the three
  encoding defects a 2026-07-31 audit found in the recall itself — the
  stateid seqid is now incremented per RFC 8881 §12.5.3, the
  back-channel slot sequence advances per §2.10.6.1, and a refused reply
  is no longer counted as an ack. What remains is the residual: server-
  side revocation binds only clients the recall actually reached
  (`FlintTruncateLostRecall.cfg`).

  **Two lessons about this module, not just about the code.**

  First, the module once asserted `RecallReaches = TRUE` for shipped
  code. That constant means "delivered AND honoured", and the shipped
  recall was malformed enough that no conforming client would honour it.
  Having the constant was right; asserting it was a green that proved
  nothing. **A constant that encodes an assumption must be justified
  against the code every time the code moves, or it silently becomes a
  lie.**

  Second, `LayoutGet` was a single atomic guard-and-effect action, and
  the implementation reads the gate at `operations/mod.rs` and publishes
  at `layout.rs` with no lock between — `LayoutManager` has no `Mutex`
  or `RwLock` at all. That atomicity was the model's, not the code's, and
  it hid a real escape (`FlintTruncateGrantRace.cfg`, now the regression
  run for the publish-time recheck that fixes it). **A single TLA+ action
  is a claim that the code holds a lock.**

  `FlintTruncateNoStaleServe.cfg` is the conditional green — what closing
  the theorem requires. Cite it as a goal, never as a property of shipped
  code.

One hypothesis was **refuted** and is kept as a run so it cannot be
quietly re-asserted: `MarkKeepsMin=FALSE` (mark_truncate_dirty
overwriting the pending size instead of keeping the smaller) still HOLDS.
Overwriting only ever *raises* the mark, and the mark can only rise on a
SETATTR that also raised the file size, so the exposure it would create
is unreachable. Safety is carried by `clear_truncate_dirty_if` alone; the
min-keeping is an ordering property, not this invariant.

Scope limits, stated because this module's abstraction is its own biggest
risk: every DS holds the same logical offset set (the gate is per-file
and its fanout is all-DSes-or-nothing, so the stripe map changes *which*
DS exposes a byte, never *whether* one does); `set_len` growth adds zeros
and zeros are not content, which is why a stale fanout re-extending a
stripe file is a size disagreement and not a stale read; reads are atomic
with respect to revocation, so even the strict run's green covers only
reads not yet on the wire to a DS; and whether a conforming Linux client would *issue* the
offending read is a client-behaviour question the model does not settle —
it settles that flint does not stop it, which is the only half flint can
fix.

`FlintExtents.tla` (2026-08-09) models the **block-layout extent
allocator** — tranche 1: the grant lifecycle, recall/fence, physical
reuse, and target-side reservation state.  It is the corpus's first
module written **before its code exists** (spec:
`docs/plans/pnfs-block-layout-design.md` §9): the allocator must be
implemented against these runs, not vice versa.  The theorem is the
**write** — a block-layout client holds raw LBAs and writes them with the
MDS nowhere on the path, so a stale holder after physical reuse corrupts
the *new* owner's bytes; `Inv_NoStaleExtentWrite` is co-equal with the
read theorem, not a corollary.  Both are deliberately absent from the
shipped cfg while `FenceReaches = FALSE` (spdk-tgt reservation
enforcement unproven on real hardware — `LostFence` keeps that honest),
exactly the `FlintTruncate.cfg` pattern.  Tranche 1's finding arrived
before any code: the design's grant-side belts (PublishRecheck /
RecallBlocksGrant) **cannot** close the grant-vs-reclaim races — a grant
published after the reclaim's holder snapshot escapes it, the reclaim can
complete-and-free between that grant's insert and any recheck, and
grant-time validation cannot see freed blocks at all (the free destroys
the very rows the transaction validates against).  Safety belongs to the
**free side** (`FreeRevalidates`: the free transaction re-validates the
grants table, sqlite-native); `FlintExtentsStaleSnapshotFree.cfg` pins
the refuted sketch permanently, MarkOverwrite-style.  Owed in later
tranches: MdsCrash + durable, the MDS fallback lane, Split/Merge +
`Inv_NoPhysicalAliasing`, and the grant-side arms as *liveness* belts.
Six non-vacuity probes (`FlintExtentsProbe.tla`) ship under the A2Probe
standing rule — reuse, fence, tgt-restart, resnapshot, commit and
truncate all witnessed as actions.

**Tranche 2** (same day, behind `CommitEnabled` — tranche-1 state spaces
bit-identical, verified by flagship distinct-count match): LAYOUTCOMMIT
with newsize, the committed state, TruncateStart, and the provisioning
scrub.  Three more spec corrections, each a harm the sketch misnamed or a
predicate legal behaviour violates: `Inv_SizeCommitCoupled` is
**transactional** (a size-advance never applies without its range
promotion — the sketched "no provisional extent within fsize" is false on
legal hole-filling; the mutation is the half-stub, F67's silent-zeros
lineage); ForgedCommit violates its own `Inv_NoForgedCommit` (a commit
writes no bytes — its harm is bookkeeping corruption, never
NoStaleExtentWrite); BlindCommit is renamed **BlindProvision**
(`ProvisionalInvisible` is scrub-at-allocation; the disclosure is
deleted-data resurrection, intra-volume by construction).  Plus one
self-correction TLC forced: the committed state broke tranche 1's
`occupied` predicate ("provisional ∧ held" no longer saw
committed-and-held blocks — two live grants overlapped in 6 states); it
now reads "not free ∧ held", because a predicate enumerating states goes
stale the day the state set grows.  The Rust transaction had it right
already.

Verification of snapshots is layered deliberately:

1. **SPDK blobstore internals** — not modeled; audited by citation (the
   axioms section below) and enforced at runtime by the sim harness's
   faithful mock + `assert_chains_are_trees` shadow.
2. **flint's copy protocol over those primitives** — `FlintSnapshots.tla`.
3. **the record-level lifecycle that consumes the copies** —
   `FlintReplication.tla` (its atomic `CatchUp`/`Admit` steps are exactly
   what `Inv_SessionFaithful` licenses).

Run the gate: `scripts/check-tla.sh` (fetches tla2tools.jar — pinned
v1.7.4, the version the pass/fail phrase-greps were validated against —
on first use).  It runs one hundred and nineteen configs, ALL required:

1. `FlintReplication.cfg` — the shipped design, 3-leg breadth
   (GateStrict, RejoinGuard, FenceZombie all TRUE): all invariants plus
   post-storm convergence hold. ~44k distinct states, seconds.
2. `FlintReplicationDeep.cfg` — the shipped design, 2 legs at a deeper
   failure budget (3 events) so torn-write divergence and the Scrub
   demotion are actually **reachable** (verified via TLC action coverage:
   every new action fires). Invariants + liveness hold.
3. `FlintReplicationF36c.cfg` — the pre-F36c bug (`GateStrict=FALSE`):
   TLC **must find** an `Inv_NoSilentLoss` violation. The 6-state
   counterexample is the split-lineage loss in its purest form.
4. `FlintReplicationRejoin.cfg` — hot rejoin without the ancestry check
   (`RejoinGuard=FALSE`): TLC **must find** an `Inv_NoDivergentServing`
   violation — a torn write strands a block on one leg, the leg goes
   stale and returns, and an unguarded delta rejoin (union copy — a block
   copy never erases) smuggles the dead-lineage block into a serving
   raid: a split-read surface.
5. `FlintReplicationF48.cfg` — no zombie sever (`FenceZombie=FALSE`):
   TLC **must find** a zombie-head violation — the partitioned old head
   keeps acking client writes after the new assembly serves (silent loss
   and/or split-brain divergence).
5b. `FlintReplicationF43.cfg` — no claim arbitration (`ClaimArb=FALSE`):
   TLC **must find** the temporal counterexample to
   `AdmissionNotStarved` — the `ReleaseCatchup → AcquireCatchup`
   starvation lasso with a warm standby parked forever (F43 as observed
   live on runad). The lasso is **weak-fairness-legal**: admission's
   enabling is intermittent, so WF never obligates it — machine-checked
   proof that the F43 fix had to be *priority*, not stronger fairness.
5c. `FlintReplicationResurrect.cfg` — fallible death evidence
   (`EvidenceStrict=FALSE`): "verified death" is a k8s observation
   (Node object gone / instance API), and here it can be wrong — a
   blackholed (recoverable) node deemed dead, e.g. a Node object
   deleted while the instance runs (the wedged-DS-roll unblock recipe).
   TLC **must find** an `Inv_NoFalseRisk` violation: ServeWithRisk
   excuses the writer holding the acked tail on false evidence — the
   surfaced risk is HOLLOW, the tail was recoverable all along. This is
   the k8s-evidence-vs-ground-truth split (`legUp` vs `deemedDead`);
   `Replace` and `ServeWithRisk` are justified by evidence, as in the
   code, and the strict runs verify `Inv_EvidenceSound` +
   `Inv_NoFalseRisk` under the strict-evidence axiom.
5d. `FlintReplicationP4.cfg` — the pre-P4 world (`SPECIFICATION
   SpecNoP4`: weak fairness on `RaidDeconfigure` dropped — nothing
   bounds dead-member detection). TLC **must find** a temporal
   counterexample to `EventuallyWritable`: the stall lasso where a
   blackholed serving leg sits in the raid forever and every write
   hangs (the 150-177s ledger stalls, unbounded). Gating this exposed
   that the prose claim "remove the P4 fairness and the liveness fails"
   was FALSE of `EventuallyServingAgain` — the stall is invisible to a
   content-shaped property (TLC verifies the old property HOLDS under
   `SpecNoP4`), so the tooth required stating `EventuallyWritable`, the
   property P4 actually guarantees; both strict runs now verify it.
5e. `FlintReplicationMaint.cfg` — the maintenance protocol
   (drain+barrier+lease all TRUE), rolls enabled, 3-leg breadth with a
   real failure budget alongside: every invariant — including
   `Inv_PlannedRollNeverCausesOutage` and `Inv_MaintFenceHolds` — must
   hold. INVARIANTS ONLY, deliberately: temporal checking is
   near-sequential and per-leg liveness at 3 legs costs minutes for no
   new corners; this run's job is invariant arity, in parallel, in
   seconds (853k states, ~7s).
5e'. `FlintReplicationMaintDeep.cfg` — the tranche's LIVENESS home:
   2-leg depth (torn writes, Scrub, zombie heads, roller death and the
   dead-leg corner all reachable across a roll campaign), every
   invariant plus ALL liveness — `MaintenanceEventuallyLifts` and
   `AdmissionNotStarved` across maintenance interleavings (~40s). Its
   **first run refuted the unconditional lifts property** — see "What
   the model already caught".
5f. `FlintReplicationRollUnfenced.cfg` — TODAY'S WORLD (`MaintFence =
   FALSE`, no drain protocol): TLC **must find**
   `Inv_PlannedRollNeverCausesOutage` violated — a routine DS roll with
   ZERO real failures blackholes a serving leg, P4 faults it out, the
   next roll follows pod-readiness, and the last serving leg
   deconfigures: `serving = {}` in 5 steps. The csi-node roll landmine
   as a counterexample.
5g. `FlintReplicationRollBarrier.cfg` — drain exists but the barrier is
   pod-readiness (`MaintBarrier = FALSE`, exactly what k8s
   maxUnavailable=1 gives you): TLC **must find**
   `Inv_PlannedRollBoundedImpact` violated at THREE legs. The
   unconditional last-serving-member belt (below) stops the direct
   drain-to-outage, so the barrier's necessity is redundancy EROSION:
   drain l1, roll, clear (all pods Ready), drain l2 while l1 is still
   stale — two legs out of service with zero real failures, one
   failure from outage. Proves fence, belt and barrier are separately
   necessary.
5h. `FlintReplicationRollLease.cfg` — unleased maintenance mark
   (`MaintLease = FALSE`): TLC **must find** the temporal
   counterexample to `MaintenanceEventuallyLifts` — the roller dies
   after the drain, the leg stays live, nothing lifts the mark, and the
   volume parks at reduced redundancy forever: the F43 parked standby
   re-created by a maintenance flag.
5i. `FlintReplicationRollRecordBarrier.cfg` — the barrier the
   IMPLEMENTATION actually has (`BarrierRaidAware = FALSE`: the roller
   reads the sync RECORD, not raid membership). Strict — must HOLD.
   Its **first run found a real silent-loss composition** (see "What
   the model already caught"), which forced the unconditional
   last-serving-member belt into `MaintDrain` and probe-first into the
   code. With the belt, the record-only shortcut costs availability in
   the monitor-lag race, never safety — which is what licenses it.
5j. `FlintReplicationRollWedged.cfg` — `SpecWedgedKubelet`: the roll's
   pod never comes back (kubelet's `WF(RollFinish)` dropped — the
   runak/runaj wedge family whose old workaround, deleting the Node
   object, is the Resurrect mutation's false-evidence hazard). Strict —
   every invariant plus writability on the survivor must HOLD: a wedged
   restart degrades exactly one leg's availability. The parked mark is
   the honest operational state, so the lifts property is deliberately
   not checked here.
5n. `FlintReplicationExpand.cfg` — the expansion tranche's strict run
   (the F56 size dimension: `legSize` per leg, `raidSize` as the
   consumer-visible high-water mark, `ExpandLeg` fan-out under the C2
   belt, both F43-item-#8 guards, `SizeHeal` = the F56 fix): every core
   invariant, `Inv_NoDeviceShrink`, `ExpansionCompletes` and
   `AdmissionNotStarved` must hold. `ExpansionCompletes` is the
   module's first per-leg PROGRESS obligation, and getting it to hold
   honestly forced four finds of its own — see "What the model already
   caught".
5o. `FlintReplicationExpandWedge.cfg` — the shipped pre-F56 code
   (`SizeHeal=FALSE`): TLC **must find** the F56 livelock as a lasso —
   a leg blackholed mid-fan-out returns as a live, content-warm,
   size-old standby; the admission size guard defers it every tick,
   the C2 belt refuses the expand retry that would grow it, and the
   chase's retention pin keeps the source-sized full build shut. Four
   individually-correct mechanisms, jointly starving — the F43 shape,
   rediscovered in the size dimension.
5p. `FlintReplicationExpandGuard.cfg` — the pre-F43-item-#8 world
   (`SizeGuard=FALSE`): TLC **must find** `Inv_NoDeviceShrink` violated
   — the pre-expand leg admitted under the grown device, the silent
   shrink (the §2.2a/C2-B hazard class). The shipped guard is
   load-bearing, not decorative.  *Single-flag since 2026-07-29: the
   audit's verifier proved the guard individually load-bearing
   (`SizeHeal=TRUE` and the violation is still found), so the earlier
   `SizeHeal=FALSE` here was a double mutation proving a weaker joint
   statement than this entry claimed.*
5k. `FlintClaims.cfg` — the shipped multi-process stack (Lease + marker
   grace, two processes, deaths and spurious leadership moves in
   budget): `Inv_NoColdAdmission` plus both liveness properties must
   hold — including the owner-dies-mid-window recovery story (marker
   ages, the survivor takes the lease, scrubs, re-warms, re-opens,
   commits).
5l. `FlintClaimsNoGrace.cfg` — the pre-F50 world (`MarkerGrace=FALSE`,
   Lease still ON): TLC **must find** the cold-admission loss in 7
   states — a renewal hiccup deposes a live leader whose in-flight
   catch-up dispatch keeps running (tick granularity), the new leader
   opens a window, the deposed dispatch reads stale+marker (exactly
   what a live pre-flip window looks like) and scrubs the payload, and
   the blind flip commits a cold leg. Machine-checked proof the grace
   and the Lease are complementary layers, not redundant ones.
5m. `FlintClaimsNoLeader.cfg` — the F53 world (`LeaderGate=FALSE`,
   grace ON): strict, **must hold**. Safety never depended on the
   process singleton — the record CAS carries one-window-at-a-time and
   the grace carries scrub-vs-live-window; the Lease buys ownership
   determinism and churn-freedom. The runaj A/B, machine-checked.

   *(Run 5q — a one-window-CAS mutation — was investigated and
   DROPPED: deleting the `window = "none"` guard from `WindowOpen`
   leaves all three claims verdicts bit-identical, machine-checked.
   At tick granularity the CAS is subsumed by the `legState` machine —
   an open requires `standby` and the first open demotes to `stale`,
   so a second open is structurally impossible whatever the window
   guard says.  The CAS's unique contribution appears only under
   read/write decomposition of the open — two processes both reading
   `standby` before either writes — which this module deliberately
   does not model (its actions are one-CAS-per-step by construction).
   A mutation that cannot lose proves nothing, so per this gate's own
   doctrine it is not a run.  The "record CAS carries
   one-window-at-a-time" claim above is therefore an axiom of the
   encoding, not a checked theorem — the honest statement of where
   tick granularity draws the line.)*
6. `FlintSnapshots.cfg` — the shipped copy protocol (full ordered walk,
   blobstore relink): `Inv_SessionFaithful` holds. Action coverage
   verified — the based suffix walk contributes zero new distinct
   states, itself a proof that faithful delta catch-up is
   content-equivalent to a full rebuild (why the optimization is legal).
7. `FlintSnapshotsSplit.cfg` — the **delta-split** bug (`WalkFull=FALSE`,
   catchup.rs's "shallow copy moves only the top layer" hazard): TLC
   **must find** a lost middle-epoch block.
8. `FlintSnapshotsOrder.cfg` — walk-order violation
   (`OrderedWalk=FALSE`, what `chain.reverse()` enforces): TLC **must
   find** an older version overwriting a newer one.
9. `FlintSnapshotsBareDelete.cfg` — bare retention delete
   (`RelinkOnDelete=FALSE`): the **finding #1 class** at content level —
   exactly what the sim harness's fake `bdev_lvol_delete` used to do.
   TLC **must find** a full build missing absorbed clusters.

**The 2026-07-29 audit tranche** — a 15-agent model↔code conformance
audit found four checked theorems that did not hold of the shipped code,
all one family: the model idealized the code's deliberate
availability-over-evidence arms while claiming correspondence.  Each arm
is now modeled behind a constant, with the idealization stated honestly
and a run with teeth:

10a. `FlintReplicationGateReal.cfg` — strict: BOTH shipped arms ON
   (`GateDeadline` + `StaleFloor`), deep 2-leg budget.  `InvCore`
   (everything except `Inv_NoFalseRisk`, which only the idealization
   satisfies) + post-storm liveness must HOLD over the mixed
   insync+stale serving states.  Its first run found a real modeling
   seam: `Replace` needed an `l ∉ serving` guard, reachable only once a
   forced-stale member could serve (P4's 20s fault-out orders before
   the 60s replace sweep at tick granularity).
10b. `FlintReplicationGateRealHollow.cfg` — the deadline arm's teeth:
   TLC **must find** `Inv_NoFalseRisk` violated with sound evidence —
   a merely-blackholed (recoverable) writer excused after the 180s
   deadline ("Never hang", drill 2.4's obligation).  The audit's
   finding #1, as a counterexample.
10c. `FlintReplicationGateRealStale.cfg` — the forced-stale floor's
   teeth: TLC **must find** `Inv_NoStaleServe` violated — a
   record-Stale leg auto-admitted beside the in-sync survivor, gate
   reads Proceed, NO risk marker (only the `StaleReplicaAdmitted`
   event).  The audit's finding #2, and the reachability proof for the
   per-leg `StaleServed` exemptions in the content invariants.
10d. `FlintReplicationMonitorLag.cfg` — the record-currency axiom's
   teeth (`MonitorCurrent=FALSE`): TLC **must find** the
   one-monitor-tick silent stale-read — a deconfigured, not-yet-marked
   leg recovers into a fresh `superblock:false` raid content-behind,
   gate reads Proceed.  A NEW finding (F58 candidate): no data-plane
   generation belt exists at NodeStage; the strict runs' `NewestOf` is
   a timing axiom, the same instrument class as FlintClaims' grace.
10e. `FlintReplicationExpandShrinkReal.cfg` — the shipped size-belt
   floor (`DeviceFloor=FALSE`: PV capacity only): TLC **must find**
   `Inv_NoDeviceShrink` violated — PV capacity lags the device after a
   partial fan-out and a lone pre-expand leg passes the old floor (the
   volumeMode:Block silent shrink; Filesystem mode is shielded by
   NodeExpandVolume's ordering).  The audit's finding #3;
   `DeviceFloor=TRUE` in the strict expand cfg is the wave-2 fix.
10f. `FlintReplicationMaintPark.cfg` — the shipped volume-wide
   admission parking (`SuppressScoped=FALSE`) under a wedged roll at 3
   legs: TLC **must find** the `StandbyAdmissionNotParked` lasso — a
   warm standby on an UNMARKED node parks forever behind another
   node's forever-renewed mark (900s TTL vs 60s tick, no wedge
   timeout).  The audit's finding #4: the F43 parked standby through a
   third door.
10g. `FlintReplicationMaintParkFixed.cfg` — per-leg suppression
   (`SuppressScoped=TRUE`, the design semantics — the wave-2 fix):
   same wedged world, `StandbyAdmissionNotParked` must HOLD.  Also the
   module's first 3-leg LIVENESS coverage.
10h. `FlintReplicationRollNoBelt.cfg` — `DrainBelt=FALSE`, the pre-fix
   record-level last-serving-member check: TLC **must find** the
   RecordBarrier silent loss.  Restores that bug class's
   rediscoverability — the fix had erased the pre-fix world from the
   configuration space, violating this gate's own doctrine.
10i. `FlintReplicationRollRecordBarrier3.cfg` — strict: the record-only
   barrier the implementation SHIPS, at 3-leg arity (invariants only;
   ~1.9M states).
10j. `FlintReplicationRollRecordBarrierDeep.cfg` — strict: the
   record-only barrier under the deep 2-leg budget, full liveness.
   (10i/10j were first run green by the audit's verifier agent, then
   gated.)

**The two-roller tranche (2026-07-29)** — the audit asserted in prose
that the maintenance roller's lease is safety-load-bearing; scouting
the code showed one-node-at-a-time and the readmission barrier are
PLANNER-only (gather-snapshot reads), the rv-guarded record CAS retries
by re-running `drain_for_maintenance` on the fresh record (preventing
lost updates, not concurrent drains), `is_leader()` is one in-process
read per tick while a tick's RPC work is unbounded (300s HTTP timeouts
vs a 15s lease), and `OP_MAINT_DRAIN` is process-local.  The machinery
(`RoguePlanDrain` captures a valid plan; `RogueDrainCommit` lands it
later applying only the commit-time guards) machine-checked the
question — and the answer inverted the audit's prose:

10k. `FlintReplicationRollerRace.cfg` — gate ON, shipped mutator: TLC
   **must find** `Inv_PlannedRollBoundedImpact` violated at 3 legs with
   ZERO failures — the deposed-but-alive roller's in-flight drain lands
   after the new leader's drain marked a different node.  **The lease
   cannot close the race it checks before the work** (F59 candidate).
   2-leg volumes were only ever protected incidentally, by the
   other-insync cardinality guard.
10l. `FlintReplicationRollerRaceUngated.cfg` — no leadership at all:
   TLC **must find** the same violation (the F50 split-process shape on
   the roller; the gate changes nothing the belt does not decide).
10m. `FlintReplicationRollerRaceFixed.cfg` — `DrainMarksBelt` with NO
   leader gate: strict, **must hold**.  Exclusivity AND record
   redundancy re-verified inside the mutation carry planned-maintenance
   safety alone — **its first run beat a marks-only belt** with the
   capture→drain→roll→clear→commit erosion (the barrier had to move
   into the CAS too, and the code fix implements both conjuncts).  The
   roller's lease buys pacing and churn-freedom, not safety — the
   `FlintClaimsNoLeader` verdict, extended to the roller and now
   machine-checked rather than asserted.

**The cutover tranche (2026-07-29)** — `cutover.rs` was the last
protocol-shaped subsystem with no model at all (the audit critic's
nomination).  A six-area scout plus adversarial verifiers settled two
things before any modeling: the *availability* fear is unfounded — a
controller that dies between `delete_pod` and `recreate_pod` does NOT
strand an RWX volume, because `rwx_nfs.rs`'s liveness reconciler
recreates an `Absent`/`Dead` server pod within about one 30s tick
(`nfs_reconcile_decision`, counting attachment INTENT) — and the
*planner* is where the real exposure lives: `VolumeCutoverView`
(cutover.rs:271-289) carries no leg health, no serving membership and
no writer set, and `plan_cutover` (305-385) reads only `sync_state` and
`last_epoch`.  **A bounce is a controller-initiated, zero-failure
teardown of a healthy serving data path issued without one term of
health in its guard.**  That makes it genuinely new reachable state:
nothing else in this module can take a volume down at `crashes = 0`
with maintenance off — `ServerCrash`/`ServerPartition` are
crash-budgeted, `RaidDeconfigure` needs `~Responsive`, and `MaintDrain`
is belted by `serving \ {l} # {}`.  The tranche also had to close a
correspondence gap first: `admit_standbys_at_stage` — the admission the
bounce exists to trigger — runs in the NODE process, under NO claim,
with the raid not yet created, and commits `record_in_sync` (writer-set
growth) BEFORE the freshness gate rules.  `Admit` cannot represent that
(it requires `claim = "admission"` and `serving # {}`), so `AdmitAtStage`
is its own action and the bounce's return path is modeled end to end.

11a. `FlintReplicationBounce.cfg` — strict: the commit-time preflight
   ON, both planner arms, two failures, plus `AdmitAtStage`.  Every
   invariant and the post-bounce liveness must hold.  **Since the code
   wave of 2026-07-29 this is the SHIPPED configuration, not a proposal:**
   `bounce_preflight` runs at the top of `execute_cutover`, and 11b/11c
   are its regression tests rather than open findings.
11b. `FlintReplicationBounceRisk.cfg` — `BouncePreflight = FALSE`, the
   SHIPPED planner, ONE bouncer, lease fully honored, no race of any
   kind: TLC **must find** `Inv_NoBounceInducedRisk` violated.  The
   trace is the whole finding — a leg is flagged data-path-lost while
   out of the raid, the volume reassembles and the leg comes back
   *serving*, the flag has not yet been cleared, a second leg dies, and
   the controller tears the volume down **on the stale flag**; the
   reassembly can only return by excusing an acked tail on a writer
   that was never verifiably dead.  Without the bounce that death was
   an ordinary fault-out on a two-writer volume.  **Cutover has no
   `DrainBelt`.**
11c. `FlintReplicationBounceRace.cfg` — the same with a deposed-but-
   alive second bouncer and the leader gate ON: TLC **must find** it
   again.  `is_leader()` is read once per tick (cutover.rs:714 — the
   single occurrence in 1548 lines) while the tick walks every flint PV
   with a blocking `await_detached` bounded only by `detach_timeout`
   (120s) *per bouncing volume*, against a 15s lease.  The decisive
   asymmetry with the roller: `drain_for_maintenance` had an rv-guarded
   record CAS to move `DrainMarksBelt` into — **cutover has no CAS
   anywhere** (`DeleteParams::default()`, a bare `create`, whole-array
   merge patches), so a commit-time preflight is the only belt this
   subsystem can host.
11d. `FlintReplicationBounceRaceFixed.cfg` — belt ON, **no leader gate
   at all**: strict, at the same two-failure budget as 11b/11c, so the
   belt is proven exactly where the counterexample lives.  The sharp
   theorem: the preflight ALONE carries bounce safety.  A third
   machine-checked instance of the `FlintClaimsNoLeader` verdict — the
   lease buys pacing, not safety.
11e. `FlintReplicationBounceLoop.cfg` — the pointless-rebounce CANARY.
   With every individual bounce belted safe, TLC **must still find**
   `Inv_NoPointlessRebounce` violated: there is no attempt counter, no
   backoff and no negative caching anywhere in `cutover.rs`; the `Err`
   arm records no attempt at all so the documented 900s minimum never
   applies on any failure path; and the data-path arm's verification
   predicate is a flag only the flagging node's agent may clear
   (`node_agent.rs:5241-5247`), permanently unsatisfiable once that node
   is gone.  Stated as a canary, not a theorem — **the fix is owed in
   code**, and the belt deliberately does not close churn.

Two honest notes on this tranche.  The bounce cfgs run under
`BounceBound`, a tighter raid-incarnation constraint than `GenBound`:
the manufactured-outage window needs failure BREADTH (two independent
failures — one to eject a leg from the raid, which is what creates the
bounce trigger, and one to take a surviving writer out during the
window) but not deep incarnation churn, and at `MaxCrashes = 2` the
generic budget puts the strict runs into tens of millions of states.
Both mutation runs were re-verified to still find their counterexamples
under it.  And `Inv_WriterSetGrounded` — the machine-check of the
audit verifier's prose rebuttal that admit-before-gate is the safe
direction — is stated with a `zombie = {}` guard, because its first run
found a composition that is NOT about the bounce: `AdmitAtStage` grows
the writer set while a partitioned old head can still be acking writes,
leaving a recorded writer that never served and does not hold the acked
tail.  The core safety theorems (`Inv_NoSilentLoss` included) hold
across it, so it is booked as an open question about the at-stage
admission's fencing order, not as a bounce finding.

**The pod layer (2026-07-29, same day)** — the cutover tranche above
abstracted the pod object away and justified it by the verifier's
finding that the *availability* question is closed in code.  That
justification was sound and the abstraction was still wrong, because it
hid a race that has nothing to do with availability: **the bare
`flint-nfs-<vol>` pod has TWO independent creators and nothing mutually
excludes them.**  `execute_cutover` deletes it, waits for the unstage
(`await_detached`, ≤`detach_timeout`, 2s poll), then recreates it from a
spec held in a local; `nfs_reconciler_pass` recreates it on a 30s tick
whenever it is `Absent` with client attachments.  Both are
`tokio::spawn`ed in the same process under the same lease.  The detach
wait exists to hold the pod down until kubelet unstages so the
replacement is *forced* to restage and reassemble — and for that entire
wait the pod is `Absent` with attachments intact, which is exactly the
reconciler's one `Recreate` cell.  (The cutover waits on the BACKING
PV's VolumeAttachment; the reconciler counts VAs on the USER PV.
Different objects, so the client attachments never drop.)

12a. `FlintReplicationBouncePod.cfg` — the shipped world with the
   **bouncer idealized** (`DetachWaitHonored = TRUE`: it recreates only
   after the unstage it waited for), isolating the reconciler as the
   sole cause.  TLC **must find** `Inv_BounceNotSilentlyDefeated`
   violated in **eight states**, and the trace has nothing exotic in it:
   a leg blackholes, recovers, and hot-rejoins as a warm standby — the
   ordinary trigger cutover exists to serve — the controller deletes the
   pod on a `"clean"` window with every writer healthy, and the
   reconciler recreates it while the volume is still staged.  Kubelet
   reuses the staged volume, no NodeStage runs, no reassembly happens,
   the standby stays parked, and clients ate an NFSv4 grace-window
   recovery for nothing.  No partition, no zombie, no second failure, no
   leadership change, no stale flag.
12b. `FlintReplicationBouncePodFixed.cfg` — `ReconcilerBelt`: no
   recreate while a bounce window is open.  Strict, and the volume must
   still converge.  **Shipped 2026-07-29** as a TIME-BOUNDED claim (a
   `bounce-in-flight` PV annotation carrying an expiry, which the
   reconciler honours), because the boundedness is exactly what this run
   cannot check — see the note below.
12c. `FlintReplicationBounceTimeout.cfg` — **the second door, and the
   reason 12b's belt is not the whole fix.**  With the reconciler
   already belted, `DetachWaitHonored = FALSE` is the shipped timeout
   path: `await_detached` returning false only WARNS and
   `execute_cutover` recreates anyway ("a same-node reuse will surface
   as CutoverIneffective").  TLC **must find** the same violation — the
   bouncer defeats its *own* wait.  Two independent doors to one harm,
   so fixing the reconciler alone leaves the bug reachable.
12d. `FlintReplicationBouncePlanner.cfg` — `plan_cutover` applies
   NEITHER of `plan_hot_rejoin`'s admission filters, so it can commit a
   full teardown whose only purpose the stage admission is guaranteed to
   refuse.  TLC **must find** `Inv_NoDoomedBounce` violated at 3 legs in
   the pre-fix volume-wide-suppression world.  (3 legs is required: at 2
   the drain belt refuses to drain the last serving leg, so a standby
   and a suppressed leg cannot coexist.)
12e. `FlintReplicationBouncePlannerScoped.cfg` — **the run that says do
   not write the fix.**  The same world with `SuppressScoped = TRUE`
   (the per-leg marks from the wave-2 code wave) and the planner still
   unfiltered exactly as `cutover.rs` ships it: strict, and it **holds**.
   Per-leg suppression leaves a standby on an unaffected leg admissible,
   so the bounce planned for it is not doomed — the wave-2 fix already
   closed a door nobody knew it was closing, and no planner filter is
   owed.  Scope: the hot-rejoin MARKER half of the same gap lives in
   `FlintClaims`' window abstraction and is not tested here.

`Inv_NoDoomedBounce` exists because of a methodological failure worth
recording.  12d was first written against `Inv_NoPointlessRebounce`, the
shared churn canary — and an A/B showed the canary fires with the
planner filter ON as well as OFF, because `BounceLoop`'s three other
doors reach it independently.  **A mutation run whose invariant is
violable for reasons other than the mutation proves nothing about the
mutation**, and it had briefly produced the opposite conclusion (that
the door survives the shipped fix).  The ghost is violable only through
this door, and flipping `PlannerDisjoint` alone flips the verdict.

Two honest notes, both corrections to how this tranche was pitched.
**The claim that the race needs no crash budget was wrong** — the *race*
needs no failures, but the bounce TRIGGER does: a standby or a
data-path flag requires a leg out of the raid, unreachable at
`MaxCrashes = 0` with maintenance off, and the first 12a run came back
green for exactly that reason.  Same class of error as the first
tranche's two green runs: reasoning about a hazard in isolation without
checking that its precondition is reachable in the configured world.
And **12b proves less than it looks**: `WF(BounceRecreate)` assumes the
bouncer completes, so the model never examines a bouncer that dies
mid-window while its belt has the only other creator held off.  The
shipped fix is therefore a *bounded* suppression — the claim carries an
expiry sized from the configured detach timeout, and the reader rejects
expired, unparseable and absurdly-far-future values alike — and the
boundedness is pinned by unit tests
(`recreate_claim_is_bounded_and_fails_open`), not by the gate.  A belt
whose necessary property the model cannot express is a belt that needs a
test; noting which is which is the point.

**The belt's own liveness (2026-07-29, after the code review)** — the one
gap in this model that a CODE review had to find instead of TLC, added
here so the next belt's liveness is machine-checked rather than argued.
`BouncePreflight` is a **guard**, so a blocked bounce is merely a
*disabled action*, and nothing in the module asked whether the
remediation it blocks ever happens.  The bounce is the escalation
ladder's terminal rung and the data-path arm fires when the path is
ALREADY dead, so refusing there lengthens an outage rather than
preventing one — and `freshness_gate::evaluate` is deadline-bounded for
exactly that reason ("Never hang").

12f. `FlintReplicationBounceStarve.cfg` — `RefusalBounded = FALSE`: TLC
   **must find** the `RemediationNotStarved` lasso, cycling
   `LegBlackhole`/`LegRecover` with the belt refusing forever.
12g. `FlintReplicationBounceBounded.cfg` — the shipped bound: strict,
   **must hold**, and `InvCore` with it, so the liveness costs no safety.

Getting this pair to mean anything took three corrections worth
recording, because each was a way of shipping a green run that proved
nothing.  **(1)** The first property said "the data-path flag eventually
clears" — but clearing it needs the flagged leg re-admitted (catch-up,
the claim, `Admit`), machinery the bounce does not own, so the lasso
existed in BOTH worlds and could not isolate the belt.  Restated on what
the belt actually blocks: the teardown.  **(2)** The bounded run then
still failed, on `MaxBounces` exhaustion — a state-space budget, not the
belt — so the property conditions on remaining budget, the same move the
roll invariants make with `crashes = 0`.  **(3)** With both fixed, the
STARVE run came back green, and the reason is the sharpest fact in this
tranche: **under a crash budget, "transiently unavailable forever" is
unrepresentable.** One blackhole either recovers (safe) or perishes into
`deemedDead` (safe), both under weak fairness.  That is the structural
reason this module was blind to an unbounded belt, and it took the
`WriterLimbo` constant — a flapping node costs no failure budget,
because a kubelet OOM loop is not a data-loss event — to make the world
expressible at all.

The FlintExtents tranches add seventeen runs (catalogued in
`scripts/check-tla.sh` next to their invocations): three strict —
`FlintExtents.cfg` (shipped design, ~1.96M distinct states: no
conflicting grants, no reuse under a live unfenced grant; stale theorems
NOT claimed), `FlintExtentsCommit.cfg` (the commit/size/scrub belts on
the shipped base, ~1.82M) and `FlintExtentsTarget.cfg` (FenceReaches +
PTPL over ALL machinery: both stale theorems hold — cite as a goal
only); eight mutations, each single-flag — `ReuseUnderGrant` (the
F65-of-extents), `GrantOverlap` (§8's PK-does-not-police-overlap),
`StaleSnapshotFree` (the tranche-1 finding, pinned), `LostFence` (the
standing residual — must keep failing until fencing is proven on
hardware), `TgtAmnesia` ("PTPL is mandatory" with teeth), `UngatedSize`
(the half-stub), `ForgedCommit` (the unfenced control path),
`BlindProvision` (the unscrubbed reuse); and six action-witness probes.

The mutation runs are the models' own regression tests; a model that
cannot rediscover the bug classes it exists for proves nothing.

## What maps to what

| model | code |
|---|---|
| `writerSet` + stamp/add/remove actions | `replica_sync.rs`: `set_writer_set` / `mark_in_sync` / `mark_stale` / `prune_writers_for_replacement` |
| `Assemble` gate disjunction | `freshness_gate.rs` (`Proceed` / `Defer` / `ServeWithRisk`) |
| `Replace` guard `legUp = "dead"` | `replica_replace.rs::node_gone` (the C2 justification) |
| `raidGen` / `legGen` / `NewestOf` | SPDK raid1 superblock examine (newest incarnation serves) |
| WF on `RaidDeconfigure` (split into `Fairness` vs `SpecNoP4`) | P4 dead-target timeouts (TCP_USER_TIMEOUT + fast_io_fail); `EventuallyWritable`/`GoodWritable` is the write-availability guarantee those timeouts buy |
| WF on `ConfirmDead` | the replace-after / node-gone threshold |
| `HotRejoin` (kept identity + payload) | `hot_rejoin.rs::hot_rejoin_volume` (contrast `Replace`: fresh identity, empty payload) |
| `RejoinGuard` at CatchUp **and** Admit | `catchup.rs`'s "usable shared epoch history" check; re-verified in the admission window |
| `Scrub` + WF on it | the `HotRejoinScrubbed` arm: delta demoted to full rebuild |
| `WriteTorn` | head crash between replica write and client ack |
| `ServerPartition` / `ZombieWrite` / Assemble's sever (`FenceZombie`) | the F48 zombie head; `catchup.rs`'s zombie-consumer sever at admission |
| `lineage` / `Inv_NoDivergentServing` | raid1 serves reads from ANY leg: one phantom block is a split-read surface |
| `Deferred` (liveness escape) | NodeStage's Defer arm: no in-sync material ⇒ designed unavailability, never stale service |
| `legUp` vs `deemedDead` / `DeemDead` / `LegPerish` | ground truth vs the record's node_gone evidence (Node object deletion / instance-termination observation); `EvidenceStrict` is the axiom they agree |
| `Inv_NoFalseRisk` | a surfaced risk is never hollow: every excused writer was truly dead (the C2 justification is real, not just recorded) |
| `LastResortServe` | the stale-only-survivor RUNBOOK override (not code — the code Defers); risk surfaced, sb generations restart from the survivor |
| `claim` / `AcquireCatchup` / `AcquireAdmission` / `ExpireClaim` | the R2 leased per-volume claim (F43); `WarmWaiting` is the yield predicate; expiry = holder death, budgeted |
| `AdmissionNotStarved` | the F43 theorem: no warm standby waits forever; S2's liveness foundation |
| `Responsive` (vs `legUp`) | the data plane cannot tell a planned tgt restart from a blackhole — the landmine's premise; every data-path guard uses it |
| `MaintDrain` (one CAS: remove + stale-mark + prune + suppress) | the planned drain the fix will compose from `replica_sync.rs` primitives under a Resolver-class R2 claim |
| `RollStart`/`RollFinish` | DS pod delete / kubelet restart completion (roller-independent, hence its own fairness) |
| `suppress` + `MaintClear`/`SuppressExpire`/`RollerDie` | the leased suppression mark (readmission exclusion); TTL expiry vs live-roller clear; `Replace` clears it with the identity swap |
| `FullRedundancy` barrier in `MaintDrain` | the raid-aware roll gate the DS controller cannot provide (pod-readiness ≠ readmitted) |
| `Inv_PlannedRollNeverCausesOutage` | with zero real failures, a rolling restart alone never takes the volume down |
| `MaintenanceEventuallyLifts` | no suppression mark outlives its purpose on a live leg (death-escaped, per-leg — see below) |
| `Inv_NoSilentLoss` | PacificA's commit invariant; the ledger oracle's zero-loss check is its runtime shadow |
| `Bounce` (`serving' = {}`, record UNCHANGED) | `execute_cutover`'s NFS-pod bounce → NodeUnstage's `teardown_volume_spdk_state`: the raid bdev and every per-replica controller are deleted and NO record field is touched — no stale marks, no writer-set prune |
| `BouncePlannable`'s two arms, with no health term | `plan_cutover` (cutover.rs:305-385) reading only `sync_state`/`last_epoch` off a `VolumeCutoverView` that carries no leg health, no serving membership, no writer set |
| `BouncePreflight` | THE PROPOSED BELT — re-verify at commit that every recorded writer is responsive-or-verifiably-dead (the `drain_leg` probe-first discipline, which cutover does not have) |
| `RogueBouncePlan`/`RogueBounceCommit` | the tick-top `is_leader()` read (cutover.rs:714, the file's only one) vs a tick whose per-volume `await_detached` runs to `detach_timeout`; no CAS exists anywhere to belt the commit |
| `dpFlag` + `AgentFlag`/`AgentClear` | `flint.csi.storage.io/data-path-lost` — written by a leg's own agent, clearable ONLY by the flagging node (`node_agent.rs:5241-5247`, `flagged_by_me`) |
| `AdmitAtStage` (unclaimed, `serving = {}`, writer set grows before the gate) | `admit_standbys_at_stage` (driver.rs:1967 → catchup.rs:2301) committing `record_in_sync` BEFORE `freshness_gate::evaluate` (driver.rs:2089) |
| `consecutiveBounces` / `Inv_NoPointlessRebounce` | the attempt counter `cutover.rs` does NOT have — the ghost that makes its absence checkable |
| **FlintClaims** | |
| `claim` as a per-process function | `volume_claims::global()` — one in-memory registry PER PROCESS, mutually invisible (the F50 premise) |
| `WindowOpen` / `WindowCommit` (open does NOT re-verify at commit) | `mark_hot_rejoin_intent` + prestage vs the flip (`mark_hot_rejoined`) — the F50 loss lives between them |
| `window` with no owner field; `winOwner` as in-memory-only state | a young stale+marker is indistinguishable from a live window by record state (the F50 doc's exact wording) |
| `ScrubMarked` under the CATCH-UP claim | catch-up's marked-dispatch performing hot-rejoin maintenance — "mutual exclusion between claim holders is not mutual exclusion between the operations that touch E_f" |
| `MarkerAge` gated on `winOwner = "none"` | the grace's quantitative content (300s >> window span) stated as ordering — `FLINT_HOT_REJOIN_RECONCILE_GRACE_SECS` |
| `LeaderGate` on acquire/open only; `SpuriousChange` deposing a live holder | P1's kube-Lease tick-granularity gating ("an in-flight op is never interrupted") |
| `Inv_NoColdAdmission` | the flip only ever lands the payload its open verified — F50's theorem |
| **FlintSnapshots** | |
| `Cut` / `chain` | `apply_epoch_cut` / the blobstore snapshot chain (retained epochs) |
| `Alloc(i)` / `ApplyWalk` oldest-first | per-epoch shallow copy; `lineage_chain` collects, `chain.reverse()` orders |
| `CopyBased` guard (base retained) | `LINEAGE_NOT_COVERED` — an aged-out base cannot be indexed; demoted to full rebuild |
| `ScrubTarget` | the full-rebuild demotion (`HotRejoinScrubbed` / "delta demoted to a full rebuild") |
| `Drop` with `RelinkOnDelete` | blobstore snapshot delete: single clone re-parented, clusters absorbed (`blobstore.c:8310-8324`); >1 clones -EBUSY (`:8534`) |
| `Inv_SessionFaithful` | a completed session = the cut, exactly; what licenses `FlintReplication`'s atomic `CatchUp`/`Admit` |

## What the model already caught

- Its **first TLC run** (tranche 1) disproved the author's belief
  (recorded in several code comments) that replacement is the *only*
  writer-set exit — the code's `mark_stale` also removes, and
  `set_writer_set` stamps wholesale at assembly. The model now mirrors
  the real maintenance rules, and the crash-before-stale-mark race they
  leave is shown to be covered by the superblock-generation belt — for
  exactly as long as at least one newer-generation leg attaches, which is
  why the record-level gate exists for the lone-returned-leg case.
- Tranche 2's first deep run forced the convergence property to state a
  design fact that had only lived in prose: when **no up in-sync leg
  exists**, unavailability is the designed outcome (`Deferred`) — the
  F36c choice sacrifices availability, never safety, and the
  stale-only-survivor last resort is a manual runbook, not an automatic
  action.
- The rejoin mutation's 11-state counterexample is a sharper statement of
  *why* the ancestry check exists than any comment: union-copy catch-up
  plus a torn write is already enough to smuggle a dead-lineage block
  into a serving raid — no zombie required.
- The F48 mutation shows gate and fence are **complementary belts**:
  with the fence on, pre-sever zombie writes are safe *because* the
  strict gate forces the zombie's legs (still the recorded writers) into
  the next assembly; each belt alone is insufficient.
- The maintenance tranche's first deep run refuted its own author's
  property. The unconditional "every suppression mark eventually lifts"
  fails honestly: drain a leg, then spot-reclaim its node AND every
  rebuild source (three budget events) — no restart can complete, no
  `Replace` has a source, the mark stays. But a mark on a truly dead
  leg is INERT (every action it gates already requires responsiveness),
  so the theorem is the per-leg, death-escaped form — and the
  counterexample is a scenario the fleet has actually lived (spot
  reclaim mid-campaign, runab/runam). The model forced the design to
  state exactly *whose* marks must lift: live legs' marks, always;
  dead legs' marks, by `Replace`'s identity swap when a source exists.
- The RecordBarrier run's first pass found a **silent-loss composition
  in the implemented drain**, 7 states: a survivor blackholes, P4
  deconfigures it, a write lands on the other leg alone, the survivor
  RECOVERS before the monitor stale-marks it — the record now calls
  both legs insync — and a drain armed on that record stale-marks and
  writer-prunes the SOLE serving leg holding the acked tail; the next
  assembly is gated by the pruned set and serves without it. Every
  record-level check passes on the lying record, so the fix had to be
  GROUND TRUTH: the unconditional `serving \ {l} # {}` belt in
  `MaintDrain`, implemented as probe-the-raid-BEFORE-the-record-round
  in `drain_leg` (and pinned by a unit test that replays the TLC
  trace). Found the day the code shipped, before any cluster ran it.
- The expansion tranche rediscovered **F56 itself** (the ExpandWedge
  lasso — found by hand in the code first, then independently by TLC,
  including the record-lag belt bypass: the fan-out starts against a
  doomed leg the record still calls insync). Getting its strict run to
  hold then flushed out four more finds, each invisible to every
  pre-existing property because none of them stated per-leg PROGRESS:
  (1) the **ghost-epoch model bug** — `EpochCut` cut the acked ledger,
  but after a ServeWithRisk assembly excused a lost writer, acked
  content exists that no live leg holds, and the cut minted an
  unsatisfiable chase target (the code snapshots what legs HOLD; the
  model now does too); (2) the **missing release-on-deferral** — a
  window whose standby de-warms (or is size-blocked) wedged the claim
  at "admission" forever once the crash budget was spent
  (`ReleaseAdmission` now models the code's RAII release, and
  `WarmWaiting` carries the size terms so the window never opens for a
  leg it cannot admit); (3) the **same-class-claimant WF trap** — the
  no-op claim cycle is real (the 30s scheduler timer claims to probe;
  it is the F43 lasso's engine, so it cannot be work-gated away), which
  makes any contender needing `claim = "none"` only intermittently
  enabled: the resizer's fan-out and the claim-holder's own dispatch
  work (`CatchUp`/`Scrub`) are now STRONG-fair — the honest abstraction
  of a persistent retrier and of work-runs-inside-the-hold; and (4)
  **candidate F57** — a standby whose node dies parks forever (the only
  standby→stale demotion is chase-source exhaustion; the raid monitor
  marks only members; `replica_replace` filters on `Stale`), escaped
  honestly in `ExpansionCompletes` with the fix owed in code.
- **The 2026-07-29 conformance audit** (15 agents, 7 areas, every
  finding adversarially verified; 107 model↔code correspondences
  verified faithful) found the four idealizations now behind
  `GateDeadline`/`StaleFloor`/`MonitorCurrent`/`DeviceFloor`/
  `SuppressScoped` (runs 10a-10j), plus corrections absorbed silently:
  `Replace` mints the swapped-in record as STALE (the code's
  `sync_state: Stale`; standby only after the full build — narrowing
  candidate F57 to the post-`record_standby` class), `ExpandGuard` is
  single-flag (the guard is individually load-bearing,
  verifier-proven), and `ExpansionCompletes` dropped its global
  `∃m stale` escape (hot-rejoin is a default-ON 60s retrier —
  `WF(HotRejoin)` is the honest abstraction; the escape now covers only
  a stale leg that CANNOT rejoin: unresponsive, or pinned serving by
  the stale floor).  The audit also stamped two residuals the model
  states but cannot discharge: `SizeHeal=TRUE` assumes the align grow
  eventually succeeds (a deterministic resize failure reproduces the
  F56 lasso in FIXED code — the ExpandWedge run doubles as that
  world), and each failed admission attempt leaks one epoch (the align
  runs after the final cut) — both owed to wave-2 code work.
- **The two-roller tranche found F59 (candidate) and then sharpened its
  own fix.**  The RollerRace run produced the double-drain WITH the
  lease fully honored — machine-proof that a once-per-tick in-process
  leadership check cannot close a race whose work outlives the lease —
  and RollerRaceFixed's first run REFUTED the obvious fix: a marks-only
  exclusivity belt loses to the capture→drain→roll→clear→commit
  erosion, because the readmission barrier is planner-only too.  The
  shipped fix (`drain_for_maintenance` refuses unless no other leg is
  marked AND every other leg is record-InSync, re-run by the rv-guarded
  retry) exists in its final form because TLC rejected the draft.  Net
  verdict, inverting the audit's prose: the roller's lease was never
  safety-load-bearing — the belt was missing.
- **The cutover tranche corrected its own premise twice before it found
  anything.**  The scout was sent to confirm an availability fear — the
  NFS server is a bare pod that nothing else recreates — and the code
  refuted it (`rwx_nfs.rs`'s liveness reconciler rebuilds an
  `Absent`/`Dead` server within about one 30s tick).  Then the first
  `BounceRisk` run came back GREEN, and the reason was not the one the
  design predicted (`MonitorCurrent`'s missing lag) but leg-arity and
  BUDGET: the manufactured-outage window needs TWO independent failures
  — one to eject a leg from the raid, which is what creates the bounce
  trigger in the first place, and one to take a surviving writer out
  during the window — and it is reachable only through the `DataPathArm`,
  the arm that is actually live under shipped RWX defaults.  With both
  corrected the counterexample is sharp, and its shape is the finding:
  the controller tears a healthy volume down **on a stale data-path
  flag** and can only come back by excusing an acked tail that was
  recoverable all along.  A green run is a claim about the config, not
  about the code — both times the config was wrong.

## Deliberate scope limits

- Hot-rejoin's esnap window internals — crash *inside* catch-up/scrub is
  the crash-sweep sim harness's job (`hot_rejoin.rs` tests); in both
  modules those steps are atomic. (Deep epoch chains and the
  stale-only-survivor last resort — tranche 3 candidates — landed in
  tranche 3: the former at content level in `FlintSnapshots`, the latter
  as `LastResortServe`.)
- SPDK blobstore internals (COW cluster mechanics, md sync ordering) —
  axiom territory: audited by citation, shadowed at runtime.
- Cross-module composition (a replication-level `CatchUp` step driving a
  snapshots-level session as one refined machine) — a possible tranche 4
  if a bug class ever demands it.  (The 2026-07-29 audit noted F56's
  actual mechanism — the revert re-creating a head as a clone of the
  base-epoch snapshot at its pre-grow size — spans both modules and is
  checkable in neither; it is covered by the sim composition test.)
- Identity domains (killed at compile time by the newtypes).
- **Stated by the 2026-07-29 audit** (previously silent): the
  retention-pin machinery (`pin_retention`/`advance_retention_pin`/
  `retire_epochs`) has no `FlintSnapshots` counterpart — atomic sessions
  make Drop-during-walk unrepresentable, so the pin's cross-step safety
  burden is carried by unit tests, not TLC.  `cutover.rs` **was** the one protocol-shaped
  subsystem with no model at all; the cutover tranche (runs 11a-11e,
  2026-07-29) closed that.  The POD OBJECT LAYER was out of
  scope and is now PARTLY in (runs 12a-12d): `podUp` plus the four-step
  delete → unstage → recreate split models the two independent creators
  of the bare server pod, because abstracting them away hid a real race.
  What remains out even there: delete-by-name-without-UID,
  409-against-a-Terminating-corpse, ControllerPublish as a third writer,
  and pod PHASE (Terminating is not distinguished from gone).
  Consequence still to accept: **the model cannot express "the volume
  had no serving NFS pod for N seconds"** — that claim stays with the
  drills, and the assumption that it never happens rests on
  `rwx_nfs.rs`'s `nfs_reconcile_truth_table` unit test, not on the gate.  Node taints are out entirely (no node
  dimension), as are the volatile attempt record's TIMING content (at
  tick granularity a controller restart and the shipped unconditional
  re-arm are the same transition) and multi-volume effects
  (head-of-line blocking, fleet-wide bounce concurrency).
  `FlintReplication`'s `claim = "admission"` still covers only the
  hot-rejoin/at-stage half; `FlintReplication`'s
  `claim = "admission"` covers only the hot-rejoin/at-stage half.  The
  leader-gate census is SIX sites (hot_rejoin, catchup, epoch_scheduler,
  cutover, maint_roll, rwx_nfs), and for the maintenance ROLLER the
  lease is safety-load-bearing (read-then-act on shared record state) —
  the NoLeader run's "operability, not safety" verdict is scoped to the
  modeled actions only.  `ClaimArb` models ABSOLUTE admission priority;
  the shipped arbitration is reservation-based and deliberately lapses
  (900s max + 120s backoff, `volume_claims.rs`) — `AdmissionNotStarved`
  certifies the design principle, not the shipped starvation bound.
  The ack/reply layer above raid1 (where F55 lived) is another
  instrument's job (drills), as is everything kernel-side.

## Data-plane axioms — verified against SPDK source (v26.05.1-pre, ~/github/spdk)

**2026-07-29 audit correction — read this section with one fact in
front of it:** flint creates every NodeStage raid with
`superblock:false` and deletes phantom raids spawned by leftover sbs
(`driver.rs ensure_raid1_bdev` / `clear_sb`).  The examine/generation
axioms below are TRUE OF SPDK but are NOT a belt flint's stage path
uses — no sb arbitration happens at assembly, every supplied base joins
as a full member, and a forced-stale or monitor-lagged leg serves reads
with no rebuild.  In the model this is now explicit: `NewestOf` encodes
the `MonitorCurrent` timing axiom (record currency), not examine; the
`MonitorLag` run shows what its absence costs.  The axioms remain
relevant to the hot-rejoin path's `skip_rebuild` adds and to the
phantom-raid hazard class.

The model's data-plane rules are *claims about SPDK raid1*, so they were
audited against the actual module (P4 exists because an unverified
transport axiom was wrong):

- **Reads go to ANY configured leg** (the split-read premise of
  `Inv_NoDivergentServing`): `raid1_channel_next_read_base_bdev` picks
  the member with the fewest outstanding read blocks —
  `module/bdev/raid/raid1.c:219`.
- **Membership changes bump the generation**: every superblock write does
  `sb->seq_number++` (`bdev_raid_sb.c:432`), and the writers are exactly
  configure / remove-base-bdev / resize / process-finish
  (`bdev_raid.c:1998,2208,2515,2662`) — the model's raidGen.
- **Newest incarnation serves; a stale leg re-enters only as a returning
  failed member**: examine compares `seq_number` — newer sb deletes and
  recreates the raid from itself; an older-sb leg is governed by the
  current sb, which marks its slot MISSING/FAILED, and it is re-added
  via `raid_bdev_configure_base_bdev` (rebuild path), never as a serving
  source (`bdev_raid.c:3883-3904,3949-3957`) — the model's `NewestOf` +
  `Admit` stamping `legGen := raidGen`.
- **The F36c lone-leg premise is real**: raid1 declares
  `CONSTRAINT_MIN_BASE_BDEVS_OPERATIONAL = 1` (`raid1.c:622`), so a lone
  returned leg whose sb is the only one in sight assembles ONLINE from
  its own superblock and serves its trailing lineage — which is exactly
  why the record-level gate exists.

One nuance the audit surfaced: examine's newest-wins recreate runs only
while the raid is CONFIGURING; once ONLINE, a later-attaching newer-sb
leg gets `-EBUSY` and the stale assembly keeps serving
(`bdev_raid.c:3888-3893`). "Newest of the attached set serves" is
therefore guaranteed only when the set attaches into one assembly
window — which is precisely what flint's NodeStage does (the gate picks
A first, then assembles once), and why the model's atomic `Assemble` is
a faithful abstraction of the orchestrated path rather than of raw
examine auto-assembly. The concurrent-assembly hazard that remains is
the F48 zombie, modeled at the record level.

The model checks the design, not the Rust. The campaign drills and the
ledger oracle remain the check on the axioms (P4 exists because a
transport assumption, not a design step, was wrong) — the audit above
converts the raid1 axioms from believed to cited.
