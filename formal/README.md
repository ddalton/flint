# Formal models — the replica-lifecycle machine and the snapshot protocol

Two modules, one gate (`scripts/check-tla.sh`, seventeen TLC runs).

`FlintReplication.tla` models the durability core every flint orchestrator
mutates: leg lifecycle states, the writer set, epoch cuts, raid superblock
generations, the F36c freshness gate, the failure taxonomy the P4 work
made explicit (crash-stop vs **silent omission** vs verified death) —
since tranche 2: **hot rejoin** (kept-payload readmission, the shared-base
ancestry check, the Scrub demotion), **torn writes** (crash between
replica write and client ack), and the **F48 zombie head** (a partitioned
server still acking writes until admission severs it) — and since
tranche 3: **LastResortServe**, the stale-only-survivor runbook step
(operator override, risk surfaced; the code itself Defers). The S2
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

`FlintSnapshots.tla` (tranche 3) models the **snapshot protocol** at
block-content level — the layer where content is `[Blocks -> version]`
and the hazards are about which version survives a chain walk, which the
write-set abstraction above deliberately cannot express: epoch cuts onto
a retained chain, shallow per-epoch deltas, oldest-first walk order,
based sessions from a shared base vs full rebuilds, retention drops with
blobstore cluster absorption. Its theorem (`Inv_SessionFaithful`): every
completed copy session delivers exactly the cut. Sessions are atomic —
crash *inside* a session is the crash-sweep sim harness's job.

Verification of snapshots is layered deliberately:

1. **SPDK blobstore internals** — not modeled; audited by citation (the
   axioms section below) and enforced at runtime by the sim harness's
   faithful mock + `assert_chains_are_trees` shadow.
2. **flint's copy protocol over those primitives** — `FlintSnapshots.tla`.
3. **the record-level lifecycle that consumes the copies** —
   `FlintReplication.tla` (its atomic `CatchUp`/`Admit` steps are exactly
   what `Inv_SessionFaithful` licenses).

Run the gate: `scripts/check-tla.sh` (fetches tla2tools.jar on first use).
It runs seventeen configs, ALL required:

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
   `Inv_PlannedRollNeverCausesOutage` and `Inv_MaintFenceHolds` — and
   every liveness property, including `MaintenanceEventuallyLifts` and
   `AdmissionNotStarved` across roll interleavings, must hold.
5e'. `FlintReplicationMaintDeep.cfg` — the same protocol at 2-leg
   content depth (torn writes, Scrub, zombie heads and roller death all
   reachable across a roll campaign). Its **first run refuted the
   unconditional lifts property** — see "What the model already caught".
5f. `FlintReplicationRollUnfenced.cfg` — TODAY'S WORLD (`MaintFence =
   FALSE`, no drain protocol): TLC **must find**
   `Inv_PlannedRollNeverCausesOutage` violated — a routine DS roll with
   ZERO real failures blackholes a serving leg, P4 faults it out, the
   next roll follows pod-readiness, and the last serving leg
   deconfigures: `serving = {}` in 5 steps. The csi-node roll landmine
   as a counterexample.
5g. `FlintReplicationRollBarrier.cfg` — drain exists but the barrier is
   pod-readiness (`MaintBarrier = FALSE`, exactly what k8s
   maxUnavailable=1 gives you): TLC **must find** the same invariant
   violated by the subtler path — drain l1, roll, clear its mark (all
   pods Ready), drain l2 while l1 is still stale. Proves fence and
   barrier are separately necessary.
5h. `FlintReplicationRollLease.cfg` — unleased maintenance mark
   (`MaintLease = FALSE`): TLC **must find** the temporal
   counterexample to `MaintenanceEventuallyLifts` — the roller dies
   after the drain, the leg stays live, nothing lifts the mark, and the
   volume parks at reduced redundancy forever: the F43 parked standby
   re-created by a maintenance flag.
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
  if a bug class ever demands it.
- Identity domains (killed at compile time by the newtypes).

## Data-plane axioms — verified against SPDK source (v26.05.1-pre, ~/github/spdk)

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
