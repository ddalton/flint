--------------------------- MODULE FlintReplication ---------------------------
(***************************************************************************)
(* The flint replica-lifecycle / writer-set machine — the durability core  *)
(* every orchestrator mutates.  This is the machine whose design-level     *)
(* bugs each cost a live campaign to find: F36c (assembly without a        *)
(* transiently-absent writer-set leg = the 6-write-tail loss), the C2 pin  *)
(* (writer-set exits only via stale-mark / replacement / assembly stamp),  *)
(* P4 (omission failures stall writes until DETECTED), and F48 (a zombie   *)
(* head still writing to legs a new assembly is admitting).                *)
(*                                                                         *)
(* PacificA correspondence (the closest formalized framework):             *)
(*   - writerSet          ~ the configuration's replica group              *)
(*   - Inv_NoSilentLoss   ~ the Commit Invariant                           *)
(*   - Replace            ~ the Reconfiguration Invariant: identity swap   *)
(*                          justified only by verified death               *)
(*   - the k8s record     ~ the external configuration manager (etcd);     *)
(*                          each control action is one atomic CAS round    *)
(*                                                                         *)
(* Writer-set maintenance mirrors replica_sync.rs EXACTLY (this module's   *)
(* first TLC run caught the author assuming "replacement is the only       *)
(* exit"; the code's mark_stale also removes, and set_writer_set stamps    *)
(* wholesale at assembly):                                                 *)
(*   - MonitorMarkStale removes the leg      (mark_stale)                  *)
(*   - Replace removes the swapped identity  (prune_writers_for_replace)   *)
(*   - Admit adds the admitted leg           (mark_in_sync)                *)
(*   - Assemble stamps the serving set       (set_writer_set)              *)
(*                                                                         *)
(* Raid superblock generations (raidGen/legGen) model SPDK raid1 examine:  *)
(* every deconfigure or reassembly is a new incarnation; only legs at the  *)
(* newest attached generation serve.  This is the data-plane belt that     *)
(* makes the crash-before-stale-mark race survivable when every writer     *)
(* attaches; the F36c gate is the record-level belt for the case where     *)
(* ONLY the returned stale leg attaches — the sb of a lone old-generation  *)
(* leg has nothing newer to contradict it.                                 *)
(*                                                                         *)
(* Failure taxonomy (the P4 lesson):                                       *)
(*   "up"        healthy                                                   *)
(*   "blackhole" unreachable SILENTLY (spot terminate = no RST, or a       *)
(*               transient partition).  May recover (LegRecover) or be     *)
(*               confirmed dead (ConfirmDead = node_gone evidence).        *)
(*   "dead"      VERIFIED gone — the only justification Replace accepts.   *)
(* A blackholed serving leg stalls writes until the data plane faults it   *)
(* out.  Weak fairness on RaidDeconfigure IS the P4 dead-target-timeouts   *)
(* guarantee; before the fix nothing bounded that detection.               *)
(*                                                                         *)
(* TRANCHE 2 — the paths that previously only live drills exercised:       *)
(*                                                                         *)
(* HOT REJOIN (hot_rejoin.rs): a stale leg on a LIVE node re-enters       *)
(* keeping its identity AND its payload (contrast Replace: verified-dead   *)
(* node, fresh identity, empty payload).  The kept payload is usable only  *)
(* if the rejoiner's lineage is an ancestor of a live source's — the       *)
(* "usable shared epoch history" check in catchup.rs, here RejoinGuard,    *)
(* enforced at BOTH catch-up and admission.  When it fails, Scrub (the     *)
(* HotRejoinScrubbed arm) wipes the payload and the leg rebuilds from      *)
(* scratch.  Catch-up and admission copy by UNION (a block copy fills      *)
(* holes, it never erases) — which is exactly why the ancestry check is    *)
(* load-bearing: union over a divergent payload smuggles dead-lineage      *)
(* blocks into a serving raid.                                             *)
(*                                                                         *)
(* TORN WRITES (WriteTorn): the head crashes after some legs persisted a   *)
(* block but before the client was acked.  The block is legitimate         *)
(* content if a holder attaches at the next assembly, and a divergent      *)
(* phantom on a leg that misses it — the raw material of the rejoin        *)
(* hazard above.                                                           *)
(*                                                                         *)
(* THE F48 ZOMBIE (ServerPartition/ZombieWrite): a partitioned old head    *)
(* still holds leg connections and still acks client writes.  The fix —    *)
(* FenceZombie — is catchup.rs's zombie-consumer sever: admission cuts     *)
(* the old head's consumers BEFORE the new assembly serves.  Unfenced,     *)
(* the zombie either acks writes the new lineage never sees (silent        *)
(* loss) or keeps writing into legs the new head now serves (split-brain   *)
(* divergence); TLC finds both.                                            *)
(*                                                                         *)
(* lineage is the served lineage's content upper bound: recomputed at      *)
(* every Assemble as the union of what the attached legs hold, grown by    *)
(* every head write.  Inv_NoDivergentServing says a serving leg holds no   *)
(* block outside it — raid1 serves reads from ANY leg, so one phantom      *)
(* block is a split-read surface.                                          *)
(*                                                                         *)
(* GateStrict = FALSE is the pre-F36c bug; RejoinGuard = FALSE drops the   *)
(* shared-base ancestry check; FenceZombie = FALSE drops the F48 sever.    *)
(* scripts/check-tla.sh REQUIRES each mutation run to find its loss — the  *)
(* model must be able to rediscover every bug class it exists for.         *)
(*                                                                         *)
(* TRANCHE 3: LastResortServe models the stale-only-survivor RUNBOOK       *)
(* override (operator serves the freshest stale survivor, risk surfaced). *)
(* 2026-07-29 AUDIT CORRECTION: the earlier claim here — "the code        *)
(* itself Defers" — was FALSE.  The shipped NodeStage has TWO automatic   *)
(* availability arms this module now models behind constants: the gate's  *)
(* 180s defer deadline (GateDeadline: serve-with-risk on transient        *)
(* evidence — Inv_NoFalseRisk is a theorem only of the idealization) and  *)
(* the 2-base-floor forced-stale admission (StaleFloor: a record-Stale    *)
(* leg auto-admitted, serving reads, evented only — the StaleServed       *)
(* per-leg exemptions).  Content-level snapshot semantics (epoch deltas,  *)
(* walk order, retention relink) live in FlintSnapshots.tla, where they   *)
(* are non-trivial; here content is a write-set and epochCut a single     *)
(* cut.                                                                    *)
(*                                                                         *)
(* RESURRECTION / EVIDENCE FALLIBILITY: "verified death" is not an         *)
(* oracle — it is a k8s OBSERVATION (Node object gone, instance API says   *)
(* terminated), and the observation can be wrong or stale: a Node object   *)
(* deleted while the instance still runs (a real operational recipe used   *)
(* to unblock wedged DS rolls), or stale cloud state.  The model now       *)
(* splits GROUND TRUTH (legUp; LegPerish is a blackhole actually dying)    *)
(* from EVIDENCE (deemedDead; DeemDead is the record accepting node_gone   *)
(* proof).  Replace and ServeWithRisk are justified by EVIDENCE — as in    *)
(* the code — and EvidenceStrict is the axiom that evidence implies        *)
(* truth.  EvidenceStrict = FALSE lets a recoverable (blackholed) node be  *)
(* deemed dead: the resurrection world.  falseRisk records the harm: a     *)
(* ServeWithRisk that excused a writer that was NOT truly dead — the       *)
(* "surfaced risk" was hollow, the acked tail was recoverable all along.   *)
(* Inv_NoFalseRisk is the theorem; the Resurrect mutation must find its    *)
(* violation.  (Replace on a falsely-deemed node also strands the old      *)
(* identity's exports on a live node — the F44-F46/F49 residue family,     *)
(* fixed live and below this abstraction; see the comment at Replace.)     *)
(*                                                                         *)
(* S2 / R2 CLAIM ARBITRATION (the F43 machinery, modeled ahead of the      *)
(* S2 bounce-free-RWX-admission implementation): control work runs under   *)
(* a per-volume claim — "catchup" (reconcile/build/scrub) or "admission"   *)
(* (the cutover/hot-rejoin window that admits a warm standby).  F43 was    *)
(* a LIVENESS bug: catch-up's timer-driven claim renewal always beat the   *)
(* admission claim to the lock, so a warm replacement standby PARKED       *)
(* forever.  The fix (ClaimArb) is PRIORITY, not stronger fairness:        *)
(* catch-up may not (re)acquire while a warm standby awaits admission —    *)
(* which turns admission's enabling from intermittent (a race it can       *)
(* lose forever under weak fairness) into continuous (weak fairness       *)
(* fires it).  AdmissionNotStarved is the theorem; ClaimArb = FALSE is    *)
(* the pre-F43 bug and the F43 mutation run must find the starvation      *)
(* lasso.  Claims are leased: holder death frees the claim (ExpireClaim,   *)
(* budgeted as a failure event).  The model does not track holder          *)
(* identity — a real lease TTL guarantees what the conflation assumes.     *)
(*                                                                         *)
(* MAINTENANCE TRANCHE (the csi-node roll landmine, modeled ahead of the   *)
(* fix): a DaemonSet roll restarts spdk-tgt on every node in sequence — a  *)
(* PLANNED data-plane outage per node that the raid cannot distinguish     *)
(* from a failure.  Today (no protocol) the roll blackholes each serving   *)
(* leg in turn: P4 faults it out, the leg stale-marks, and rolling the     *)
(* next node before readmission takes the volume to serving = {} with     *)
(* ZERO real failures.  The modeled fix is three separately-necessary      *)
(* guards:                                                                 *)
(*   MaintFence   drain-before-restart: a node's tgt is never taken down   *)
(*                while its leg is in the serving set (MaintDrain is a     *)
(*                graceful remove+stale-mark, one CAS; the restart then    *)
(*                touches only a non-serving leg).  P4 needs NO            *)
(*                maintenance awareness — detection stays always-on,       *)
(*                which is the design argument against the rejected       *)
(*                alternative (suppressing dead-target detection during    *)
(*                rolls: a reclaim mid-roll would then hide).              *)
(*   MaintBarrier the next node waits for FULL readmission (in-sync +      *)
(*                serving), not pod-readiness.  k8s maxUnavailable=1       *)
(*                gives pod-level serialization unconditionally (modeled   *)
(*                so); it knows nothing of raid membership — that gap IS   *)
(*                the landmine's second half.                              *)
(*   MaintLease   the suppression mark (readmission excluded while the     *)
(*                node's tgt is fair game) is leased: a dead roller's      *)
(*                mark self-clears.  Unleased, a roller death parks the    *)
(*                drained leg at 1/2 redundancy forever — the F43 parked   *)
(*                standby by another door.                                 *)
(* Rolls are budgeted separately from failures (rolled is monotone — one   *)
(* campaign, each node once); a planned roll costs no crash budget, so     *)
(* Inv_PlannedRollNeverCausesOutage can condition on crashes = 0: with     *)
(* zero REAL failures, maintenance alone never takes the volume down.      *)
(* MaintEnabled = FALSE (every legacy cfg) disables all of it and leaves   *)
(* the prior state spaces bit-identical in behavior.                       *)
(*                                                                         *)
(* Out of scope: esnap-window INTERNALS (crash inside catch-up/scrub is    *)
(* the crash-sweep sim harness's job — here those steps are atomic);       *)
(* identity domains (killed at compile time by the newtypes); the LOCAL    *)
(* half of the landmine (ublk device continuity for consumers co-located   *)
(* with the rolled tgt — kernel-level, empirical, drill-gated; see        *)
(* docs/maintenance-drain-csi-node-roll.md).                               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Legs,        \* replica slots, e.g. {"l1", "l2", "l3"}
  MaxWrites,   \* bound on the write counter
  MaxCrashes,  \* bound on failure events (die/blackhole/crash/partition/torn)
  GateStrict,  \* TRUE = F36c gate on; FALSE = pre-F36c bug (must fail)
  RejoinGuard, \* TRUE = hot-rejoin shared-base ancestry check on
  FenceZombie, \* TRUE = assembly severs the previous head first (F48 fix)
  ClaimArb,    \* TRUE = admission-priority claim arbitration (F43 fix)
  EvidenceStrict, \* TRUE = node_gone evidence implies actual death
  MaintEnabled, \* TRUE = planned-roll actions exist (FALSE in legacy cfgs)
  MaintFence,  \* TRUE = drain-before-restart (never roll a serving leg's tgt)
  MaintBarrier,\* TRUE = next node waits for readmission, not pod-readiness
  MaintLease,  \* TRUE = a dead roller's suppression mark self-clears
  BarrierRaidAware, \* TRUE = the barrier reads GROUND TRUTH (raid membership
               \* + responsiveness); FALSE = the record only (all replicas
               \* insync) — the shortcut the implementation actually takes.
               \* The record can lag the raid by one monitor tick, so the
               \* weaker barrier can drain into a race with an undetected
               \* failure; the RecordBarrier strict run verifies that costs
               \* availability (an honest Defer), never safety.
  \* ---- the expansion tranche (F56; docs/f56-expand-replacement-circular-wait.md)
  ExpandEnabled, \* TRUE = the size dimension + expansion actions exist
               \* (FALSE keeps every legacy cfg's behavior graph identical)
  SizeGuard,   \* TRUE = the F43 item-#8 leg-size guards (admission refuses
               \* a head shorter than its copy source; the construction
               \* boundary refuses a short leg under a grown device; the
               \* stage belt refuses short legs under a grown floor).
               \* FALSE = the pre-guard world — the ExpandGuard mutation
               \* must find the silent device shrink.
  SizeHeal,    \* TRUE = the F56 fix (align_dst_head_size): catch-up and
               \* the admission session GROW an undersized head to its
               \* copy source instead of deferring it.  FALSE = the
               \* shipped-pre-fix world — the ExpandWedge mutation must
               \* find the permanent livelock (guards individually
               \* correct, jointly starving: belt blocks expand, guard
               \* blocks admission, nothing resizes, the retention pin
               \* holds the full-build escape shut).  NOTE (2026-07-29
               \* audit): TRUE also encodes the ASSUMPTION that the grow
               \* eventually succeeds — under a DETERMINISTICALLY failing
               \* bdev_lvol_resize the fixed code loops
               \* revert→align-fail→defer in exactly the pre-fix shape, so
               \* the ExpandWedge run doubles as that residual's model.
  \* ---- the shipped availability envelope (2026-07-29 conformance audit:
  \* the arms below are DELIBERATE code policies the model previously
  \* idealized away while claiming correspondence; each now has a constant,
  \* honest theorems, and a run with teeth) -------------------------------
  GateDeadline, \* TRUE = the shipped freshness gate's wall-clock defer
               \* bound (FLINT_F36C_DEFER_SECS, default 180s; the
               \* flint.io/f36c-defer PV annotation): once a deferral's
               \* deadline passes, NodeStage serves-with-risk EVEN IF the
               \* missing writers are only transiently unavailable
               \* ("Never hang" — the drill-2.4 obligation,
               \* freshness_gate.rs evaluate).  The excused tail may be
               \* RECOVERABLE — a hollow risk the evidence-only arm can
               \* never produce, which is why Inv_NoFalseRisk is a
               \* theorem only of the GateDeadline = FALSE idealization
               \* (every legacy cfg) and the GateRealHollow run must FIND
               \* its violation with the arm on.
  StaleFloor,  \* TRUE = the shipped 2-base-floor forced-stale admission
               \* (driver.rs "Last-resort fallback": below 2 attached
               \* bases NodeStage AUTOMATICALLY admits record-Stale
               \* replicas, in replica-index order, no operator).  The
               \* admitted leg keeps its stale content, keeps sync_state
               \* Stale, enters the writer set (set_writer_set stamps
               \* every attached base), and SERVES READS with no rebuild
               \* — surfaced only by the StaleReplicaAdmitted event, no
               \* risk annotation when the gate read Proceed.  FALSE =
               \* the idealization every legacy cfg checked (stale
               \* service strictly via the LastResortServe runbook).
  MonitorCurrent, \* THE RECORD-CURRENCY AXIOM (2026-07-29: previously
               \* misdocumented as "SPDK raid1 examine" — the shipped
               \* code creates every raid with superblock:false and
               \* clears leftover sbs, so NO data-plane generation
               \* arbitration exists at NodeStage).  TRUE = assume the
               \* raid-health monitor's stale-mark landed before any
               \* reassembly reads the record (encoded as Assemble
               \* serving only NewestOf of the attached in-sync legs);
               \* FALSE = the shipped exposure: every record-insync
               \* attached leg serves, so a leg deconfigured within one
               \* monitor tick of the crash re-enters content-behind —
               \* the MonitorLag run must FIND the silent stale-read.
               \* Same instrument class as FlintClaims' MarkerGrace:
               \* a timing assumption TLA cannot discharge, held by a
               \* ~60s monitor cadence vs a crash window.
  DeviceFloor, \* TRUE = assembly-time size belts floor on the DEVICE
               \* high-water mark in addition to PV spec.capacity (the
               \* audit-mandated fix: the two DIVERGE after a partial
               \* expand fan-out — the device grows when every SERVING
               \* leg grew, PV capacity only after the WHOLE fan-out
               \* succeeds).  FALSE = the shipped belts (PV capacity
               \* only, leg_size_guard::partition_legs) — the
               \* ExpandShrinkReal run must FIND Inv_NoDeviceShrink
               \* violated (a lone pre-expand leg served after the
               \* device grew: the volumeMode:Block silent shrink).
  SuppressScoped, \* TRUE = admission planning excludes only the MARKED
               \* leg (the design doc's and this module's original
               \* per-leg semantics; the audit-mandated fix).  FALSE =
               \* the shipped widening: plan_hot_rejoin parks the WHOLE
               \* volume's admission planning (and releases the F43
               \* reservation) while ANY replica carries a live mark —
               \* under a wedged roll whose live roller renews marks
               \* forever, a warm standby on an UNAFFECTED node parks
               \* indefinitely at reduced redundancy (the F43 shape by
               \* a third door; the MaintPark run must FIND the lasso).
               \* Catch-up/chase dispatch is per-leg in code either way.
  DrainBelt,   \* TRUE = MaintDrain's unconditional last-serving-member
               \* belt reads GROUND TRUTH (the code probes the raid
               \* before the record round — the RecordBarrier fix).
               \* FALSE = the pre-fix record-level belt (another
               \* recorded-insync leg suffices) — the RollNoBelt run
               \* must FIND the silent loss the original RecordBarrier
               \* investigation hit, restoring this bug class's
               \* rediscoverability (the module's own mutation rule).
  \* ---- the two-roller tranche (2026-07-29: is the roller's lease
  \* safety-load-bearing?  The audit inverted NoLeader's verdict for the
  \* roller in prose; these constants machine-check it) -------------------
  RollerRace,  \* TRUE = the deposed-roller overlap machinery exists: a
               \* roller instance may CAPTURE a drain plan from a valid
               \* snapshot (RoguePlanDrain) and COMMIT it later against
               \* changed state (RogueDrainCommit) with only the checks
               \* the code re-runs at commit time.  Code facts (scouted
               \* 2026-07-29): one-node-at-a-time is PLANNER-only (the
               \* gather snapshot's marked_nodes); the rv-guarded record
               \* CAS retries by re-running drain_for_maintenance on the
               \* FRESH record — preventing lost updates, not concurrent
               \* drains; is_leader() is one in-process atomic read per
               \* tick while a tick's RPC work is unbounded (300s HTTP
               \* timeouts × N volumes vs a 15s lease); OP_MAINT_DRAIN
               \* is process-local.  FALSE in every pre-existing cfg
               \* (state spaces preserved).
  RollerLeaderGate, \* TRUE = the lease gates CAPTURING a plan (tick-top
               \* is_leader): a stale plan then requires the one
               \* budgeted deposal overlap (leaderMoved).  FALSE = no
               \* leadership at all — a permanently split second roller.
               \* THE POINT: RollerRace must FIND the double-drain EVEN
               \* WITH the gate TRUE — the lease is checked before the
               \* work, not at the commit, so it cannot close the race.
  DrainMarksBelt, \* TRUE = the fix: drain_for_maintenance refuses unless
               \* NO other replica carries a live maintenance mark AND
               \* every other replica is record-InSync — one-node-at-a-
               \* time AND the readmission barrier both moved INTO the
               \* mutation, where the rv-guarded retry makes them
               \* race-proof.  (A marks-only belt is INSUFFICIENT —
               \* RollerRaceFixed's first run proved it: capture a plan,
               \* let the leader drain-roll-CLEAR another node, and the
               \* stale plan commits while that leg is still
               \* un-readmitted — the barrier is planner-only too, so
               \* the erosion arrives through the second door.  The
               \* all-others-insync form subsumes marks-empty except
               \* for the stage-admission edge — admit_standbys_at_stage
               \* has no maintenance filter, so a marked leg CAN be
               \* record-InSync — hence both conjuncts.)  FALSE = the
               \* shipped mutator (guards only: target exists, target
               \* unmarked-for-rejoin, and if the target is InSync
               \* another InSync leg remains — a cardinality check that
               \* incidentally protects 2-leg volumes and nothing
               \* else).  RollerRaceFixed (belt TRUE, gate FALSE) is
               \* the sharp theorem: the belt alone carries bounded
               \* impact — the lease buys pacing, not safety.
  \* ---- the cutover tranche (cutover.rs: plan→bounce→verify→judge) -------
  BounceEnabled, \* TRUE = the controller-initiated TEARDOWN of a healthy
               \* serving data path exists (execute_cutover).  This is a
               \* genuinely new door: at crashes = 0 with MaintEnabled =
               \* FALSE nothing else in this module can take a healthy
               \* volume down — ServerCrash/ServerPartition are crash-
               \* budgeted, RaidDeconfigure needs ~Responsive, MaintDrain
               \* is belted by serving \ {l} # {}.  FALSE in every
               \* pre-existing cfg (legacy behavior graphs stay identical).
  MaxBounces,  \* bound on bounces-since-last-progress; 0 in every legacy
               \* cfg, which also leaves GenBound numerically unchanged.
  AdmissionArm,\* TRUE = plan_cutover's converged-standby arm can fire
               \* (cutover.rs:336-383).  INERT under shipped RWX defaults —
               \* cfg.rwx_inplace defaults true and the planner returns
               \* Wait (366-370) — so this arm corresponds to
               \* FLINT_RWX_INPLACE_ADMISSION=disabled (S2's fallback rung)
               \* or the RWO flint.csi.storage.io/rejoin-bounce opt-in.
               \* Behind its own flag so each theorem names its world.
  DataPathArm, \* TRUE = plan_cutover's data_path_lost arm (312-334) and
               \* the annotation machinery exist.  It bypasses the standby
               \* AND lag gates entirely ("nothing to admit, only a data
               \* path to rebuild"), and its verification predicate is a
               \* flag ONLY the flagging node's agent may ever clear.
  BouncePreflight, \* THE PROPOSED BELT (the DrainBelt analogue).
               \* FALSE = SHIPPED: VolumeCutoverView carries no leg health,
               \* no serving membership and no writer set (cutover.rs:
               \* 271-289); plan_cutover reads only sync_state and
               \* last_epoch (305-385).  The precondition "this teardown
               \* will not cost availability" is ASSUMED, never checked —
               \* contrast drain_leg, which probes the raid BEFORE the
               \* record round.  TRUE = re-verify AT COMMIT that the
               \* volume can come back whole: every recorded writer
               \* responsive or verifiably dead.
  BounceRace,  \* TRUE = the two-bouncer machinery (RogueBouncePlan
               \* captures a plan valid at capture time; RogueBounceCommit
               \* lands it later under only the guards the code re-runs at
               \* commit — which is get_pod, and nothing else).
  BounceLeaderGate, \* TRUE = the lease gates CAPTURING a plan (the
               \* tick-top is_leader read at cutover.rs:714 — the SINGLE
               \* occurrence in 1548 lines); a stale plan then costs the
               \* one budgeted deposal overlap (leaderMoved, SHARED with
               \* the roller: orchestrator_lease.rs is ONE lease across all
               \* six leader-gated sites, so one deposal deposes every
               \* orchestrator at once).  FALSE = no leadership at all.
  PodLayer,    \* TRUE = model the flint-nfs-<vol> POD OBJECT and its two
               \* independent creators, splitting the atomic Bounce into
               \* delete → unstage → recreate.  This is the layer the
               \* first cutover tranche deliberately abstracted away; it
               \* is here now because the abstraction hid a race with no
               \* mutual exclusion anywhere in it.  FALSE = the atomic
               \* Bounce (every pre-existing cfg, including 11a-11e).
  ReconcilerBelt, \* FALSE = SHIPPED: rwx_nfs.rs's liveness reconciler
               \* recreates an Absent server pod whenever the user PV has
               \* client attachments — and nfs_reconcile_decision's
               \* signature is (backend_is_emptydir, pv_terminating,
               \* attachment_count, liveness), which CANNOT carry "a
               \* bounce is in flight", so the absence of a guard is
               \* provable from the type.  The cutover waits on the
               \* BACKING PV's VolumeAttachment while the reconciler
               \* counts VAs on the USER PV — different objects, so the
               \* client attachments never drop and the whole detach wait
               \* sits inside the reconciler's one Recreate cell.
               \* TRUE = the proposed fix: no recreate while a bounce
               \* window is open (one creator, not two).
  DetachWaitHonored, \* TRUE = the bouncer recreates only after the
               \* unstage it waited for.  FALSE = SHIPPED on the timeout
               \* path: await_detached returning false only WARNS and
               \* execute_cutover recreates anyway ("a same-node reuse
               \* will surface as CutoverIneffective").  Split from
               \* ReconcilerBelt so the runs can say WHICH creator
               \* defeated the bounce.
  PlannerDisjoint, \* TRUE = plan_cutover honours the admission filters
               \* plan_hot_rejoin honours.  FALSE = SHIPPED: it applies
               \* neither the maintenance-suppression nor the hot-rejoin
               \* marker filter, so it can plan a bounce for a standby
               \* the stage admission will then refuse — a bounce whose
               \* predicate is unsatisfiable before it is even issued.
  StageAdmit   \* TRUE = model admit_standbys_at_stage (driver.rs:1967 →
               \* catchup.rs:2301) as its own action.  Required for the
               \* bounce's RETURN path: Admit cannot represent it — Admit
               \* demands claim = "admission" and serving # {}, while the
               \* at-stage admission runs in the NODE process, under NO
               \* volume claim, with the raid not yet created, and commits
               \* record_in_sync (writer-set GROWTH) BEFORE the freshness
               \* gate rules (driver.rs:2089).  The code's order is
               \* admit→gate; this module's has been gate→admit.

VARIABLES
  \* ---- data plane -------------------------------------------------------
  serving,       \* SUBSET Legs: legs configured in the serving raid; {} = down
  zombie,        \* SUBSET Legs: a partitioned old head's assembly view (F48)
  legData,       \* [Legs -> SUBSET 1..MaxWrites]
  legUp,         \* [Legs -> {"up", "blackhole", "dead"}] — GROUND TRUTH
  deemedDead,    \* SUBSET Legs — the record's node_gone EVIDENCE
  falseRisk,     \* TRUE once ServeWithRisk excused a not-truly-dead writer
  raidGen,       \* current raid incarnation (bumped on deconfigure/assemble)
  legGen,        \* [Legs -> Nat]: newest incarnation each leg participated in
  acked,         \* SUBSET 1..MaxWrites
  nextWrite,
  lineage,       \* content upper bound of the CURRENT served lineage
  riskSurfaced,  \* TRUE once a ServeWithRisk assembly was chosen
                 \* (forced-stale service needs no ghost: a StaleFloor
                 \* member keeps record-state "stale" while serving, so
                 \* StaleServed below is derived state)
  \* ---- control plane (the k8s record; each action = one CAS) ------------
  state,         \* [Legs -> {"insync", "stale", "standby"}]
  writerSet,     \* SUBSET Legs — recorded serving-assembly membership
  epochCut,      \* SUBSET 1..MaxWrites — content captured at the last cut
  claim,         \* {"none", "catchup", "admission"} — the R2 volume claim
  deferExpired,  \* the persisted f36c-defer deadline has passed while the
                 \* volume was down (GateDeadline; flint.io/f36c-defer is
                 \* per-volume and survives NodeStage retries) — cleared
                 \* by the next assembly, exactly like the code's two
                 \* clear sites (missing-empty and ServeWithRisk)
  crashes,       \* failure budget spent
  \* ---- planned maintenance (the csi-node roll) ---------------------------
  rolling,       \* SUBSET Legs: node whose tgt is down for a PLANNED restart
  rolled,        \* SUBSET Legs: nodes already rolled this campaign (monotone)
  suppress,      \* SUBSET Legs: readmission suppressed (the maintenance mark)
  rollerDead,    \* TRUE once the roll orchestrator died mid-campaign
  stalePlan,     \* SUBSET Legs (≤1): a second/deposed roller's captured
                 \* drain plan — valid when captured, committed later
                 \* against changed state (RollerRace)
  leaderMoved,   \* the one budgeted spurious-deposal overlap was spent
                 \* (RollerLeaderGate worlds; deposal is not a volume
                 \* failure, so it deliberately costs NO crash budget —
                 \* Inv_PlannedRollBoundedImpact conditions on crashes=0)
  \* ---- expansion (the F56 size dimension) --------------------------------
  legSize,       \* [Legs -> {"old", "new"}]: each leg's lvol size
  raidSize,      \* {"old", "new"}: the consumer-visible device size — a
                 \* HIGH-WATER mark (raid1 caps at the min of its bases and
                 \* only grows once every base grew; a consumer that saw
                 \* "new" must never be served "old" again — the silent
                 \* shrink).  2026-07-29 audit: this previously ALSO stood
                 \* in for PV spec.capacity — a conflation, because the
                 \* two quantities diverge in reachable states (the device
                 \* grows when every SERVING leg grew; PV capacity only
                 \* after the whole fan-out succeeded).  pvSize now models
                 \* the record-side quantity; the shipped belts key on it.
  pvSize,        \* {"old", "new"}: PV spec.capacity — advances only when
                 \* EVERY leg grew (expand_replicated returns Done, the
                 \* external resizer patches capacity).  The shipped
                 \* assembly belts floor on THIS (leg_size_guard reads the
                 \* PV), which is what makes Inv_NoDeviceShrink violable
                 \* with DeviceFloor = FALSE.
  wantNew,       \* an expansion request is outstanding (the resizer's
                 \* retry loop; latched — one expansion per behavior)
  \* ---- the cutover bounce (cutover.rs) -----------------------------------
  bounceWindow,  \* {"none","clean","risky"} — an OPEN bounce window and
                 \* what the RECORDED writer set looked like when the
                 \* controller opened it.  "risky" = a writer was already
                 \* unavailable at commit time (the manufactured outage);
                 \* "clean" = every writer was responsive-or-deemed-dead,
                 \* so any later loss is an ordinary failure inside the
                 \* window that no preflight could have predicted.
                 \* Cleared by the assembly that brings the volume back.
  bouncePlan,    \* a second/deposed bouncer captured a plan, valid when
                 \* captured, committed later (the RogueDrain shape)
  bounceRisk,    \* the harm ghost: a ServeWithRisk assembly fired to come
                 \* back from a window the CONTROLLER opened on an already-
                 \* broken writer set
  consecutiveBounces, \* 0..MaxBounces — bounces since one accomplished
                 \* anything.  Reset by Admit/AdmitAtStage (a standby got
                 \* in) and AgentClear (the data path came back).  There is
                 \* NO attempt counter anywhere in cutover.rs; this ghost
                 \* is what makes its absence checkable.
  dpFlag,        \* {"none"} \cup Legs — the data-path-lost annotation,
                 \* "<node>|<since>".  ONLY the flagging leg's own agent
                 \* may clear it (node_agent.rs:5241-5247, flagged_by_me):
                 \* the CONFIRMED ownership trap.
  podUp,         \* the flint-nfs-<vol> server pod exists (PodLayer worlds;
                 \* pinned TRUE everywhere else).  A BARE pod: the bouncer
                 \* deletes it holding the replacement spec in a local, and
                 \* the liveness reconciler is a SECOND, independent creator
                 \* of the same pod name.
  bounceDoomed,  \* a bounce was COMMITTED whose only justification was a
                 \* standby the stage admission will then refuse — the
                 \* attributive ghost for PlannerDisjoint (the shared
                 \* pointless-rebounce canary cannot distinguish this
                 \* door from the three others, so it cannot test a fix)
  everServed     \* SUBSET Legs — legs that have served in their CURRENT
                 \* incarnation (Replace/Scrub wipe the payload and drop
                 \* membership).  The ghost that makes the admit-before-
                 \* gate theorem non-vacuous.

\* NOTE the bounce variables ARE in this tuple, deliberately: AgentFlag and
\* AgentClear change nothing else, so with them omitted <<AgentClear(l)>>_vars
\* would never hold and the weak fairness below would obligate NOTHING —
\* a silently vacuous liveness assumption.  (stalePlan/leaderMoved remain
\* outside it, as before: no action carrying them is fair, so the same
\* trap cannot bite there, and adding them would perturb the existing
\* roll-lease lasso runs.)
vars == <<serving, zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
          lineage, riskSurfaced, state, writerSet, epochCut, claim,
          deferExpired, deemedDead, falseRisk, crashes,
          rolling, rolled, suppress, rollerDead, legSize, raidSize, pvSize, wantNew,
          bounceWindow, bouncePlan, bounceRisk, consecutiveBounces, dpFlag,
          everServed, podUp, bounceDoomed>>

maintVars == <<rolling, rolled, suppress, rollerDead, stalePlan, leaderMoved>>

expandVars == <<legSize, raidSize, pvSize, wantNew>>

\* The audit tranche's one new piece of persistent state: the per-volume
\* f36c-defer deadline flag (grouped for the UNCHANGED lists).
gateVars == <<deferExpired>>

\* The cutover tranche's state (grouped like maintVars/expandVars so every
\* untouched action carries exactly one extra UNCHANGED line).
bounceVars == <<bounceWindow, bouncePlan, bounceRisk, consecutiveBounces,
                dpFlag, everServed, podUp, bounceDoomed>>

\* A forced-stale (StaleFloor) member keeps record-state "stale" while it
\* serves — the only way a stale-state leg is ever in the serving set
\* (MonitorMarkStale requires l \notin serving; the drain removes and
\* marks in one CAS; LastResortServe stamps its survivor insync).  The
\* content theorems escape on this exactly while the knowingly-behind
\* leg actually serves; the moment it deconfigures or a reassembly
\* excludes it, the theorems re-arm.
StaleServed == \E l \in serving : state[l] = "stale"

TypeOK ==
  /\ serving \subseteq Legs
  /\ zombie \subseteq Legs
  /\ legData \in [Legs -> SUBSET (1..MaxWrites)]
  /\ legUp \in [Legs -> {"up", "blackhole", "dead"}]
  /\ deemedDead \subseteq Legs
  /\ falseRisk \in BOOLEAN
  /\ raidGen \in Nat
  /\ legGen \in [Legs -> Nat]
  /\ acked \subseteq 1..MaxWrites
  /\ nextWrite \in 1..(MaxWrites + 1)
  /\ lineage \subseteq 1..MaxWrites
  /\ riskSurfaced \in BOOLEAN
  /\ state \in [Legs -> {"insync", "stale", "standby"}]
  /\ writerSet \subseteq Legs
  /\ epochCut \subseteq 1..MaxWrites
  /\ claim \in {"none", "catchup", "admission"}
  /\ deferExpired \in BOOLEAN
  /\ crashes \in 0..MaxCrashes
  /\ rolling \subseteq Legs
  /\ rolled \subseteq Legs
  /\ suppress \subseteq Legs
  /\ rollerDead \in BOOLEAN
  /\ stalePlan \subseteq Legs /\ Cardinality(stalePlan) <= 1
  /\ leaderMoved \in BOOLEAN
  /\ legSize \in [Legs -> {"old", "new"}]
  /\ raidSize \in {"old", "new"}
  /\ pvSize \in {"old", "new"}
  /\ wantNew \in BOOLEAN
  /\ bounceWindow \in {"none", "clean", "risky"}
  /\ bouncePlan \in BOOLEAN
  /\ bounceRisk \in BOOLEAN
  /\ consecutiveBounces \in 0..MaxBounces
  /\ dpFlag \in {"none"} \cup Legs
  /\ everServed \subseteq Legs
  /\ podUp \in BOOLEAN
  /\ bounceDoomed \in BOOLEAN

\* A leg's data path answers: its node is up AND its tgt is not down for a
\* planned restart.  The raid cannot tell the two apart — that symmetry is
\* the whole landmine, and every data-plane guard below uses this, not
\* legUp alone.  With MaintEnabled = FALSE, rolling = {} always and this
\* reduces to the old legUp = "up".
Responsive(l) == legUp[l] = "up" /\ l \notin rolling

UpInSync == {l \in Legs : state[l] = "insync" /\ Responsive(l)}

\* Newest-generation selection over an attached set.  2026-07-29 audit
\* CORRECTION: this is NOT "SPDK examine" — the shipped code creates every
\* raid with superblock:false (no sb arbitration; driver.rs
\* ensure_raid1_bdev) — it is the ENCODING of the MonitorCurrent axiom:
\* Assemble serving only NewestOf of the attached in-sync legs is
\* equivalent to assuming the monitor's stale-mark always lands before a
\* reassembly reads the record.  MonitorCurrent = FALSE drops the axiom
\* and every record-insync attacher serves — the shipped exposure, one
\* monitor tick wide (the MonitorLag run).
NewestOf(A) == {l \in A : \A m \in A : legGen[l] >= legGen[m]}

\* The admission-planning suppression gate.  Per-leg is the design (and
\* this module's original semantics); the shipped plan_hot_rejoin parks
\* the WHOLE volume while ANY replica carries a live mark
\* (record.replicas.iter().any(maint_drain_live)) — an accidental
\* widening (no design note; the doc says per-leg).  Catch-up/chase
\* dispatch is per-leg in code either way (CatchUp/Scrub keep l \notin
\* suppress below).
AdmissionOpen(l) == IF SuppressScoped THEN l \notin suppress
                                     ELSE suppress = {}

\* A warm standby awaits admission: caught up, its node live, a serving
\* source available, and (with the ancestry check on) actually admittable.
\* This is the predicate the F43 arbitration pivots on — catch-up must
\* yield exactly when this is true.
\* A suppressed leg (maintenance mark) is excluded from admission planning
\* entirely — it is neither admittable nor something catch-up must yield
\* to.  The mark's LIVENESS obligation (it must eventually lift) is
\* MaintenanceEventuallyLifts, not this predicate.
WarmWaiting ==
  \E l \in Legs :
    /\ state[l] = "standby"
    /\ Responsive(l)
    /\ AdmissionOpen(l)
    /\ epochCut \subseteq legData[l]
    /\ serving # {}
    \* Size-admissibility is part of "awaits admission": the real window
    \* CLOSES on a size-guard refusal (StandbyAdmissionDeferred releases
    \* the claim; the next reassembly re-opens) — so at tick granularity
    \* a size-blocked standby neither holds the window open nor makes
    \* catch-up yield to it.  Without this term the admission claim
    \* wedges on a leg Admit can never accept — a model artifact the
    \* ExpandWedge run surfaced (the code has no such hold-forever).
    /\ (SizeGuard => (SizeHeal \/ raidSize = "old" \/ legSize[l] = "new"))
    /\ \E src \in serving :
         /\ Responsive(src)
         /\ (RejoinGuard => legData[l] \subseteq legData[src])
         /\ (SizeGuard => (SizeHeal \/ legSize[src] = "old" \/ legSize[l] = "new"))

Init ==
  /\ serving = Legs
  /\ zombie = {}
  /\ legData = [l \in Legs |-> {}]
  /\ legUp = [l \in Legs |-> "up"]
  /\ deemedDead = {}
  /\ falseRisk = FALSE
  /\ raidGen = 1
  /\ legGen = [l \in Legs |-> 1]
  /\ acked = {}
  /\ nextWrite = 1
  /\ lineage = {}
  /\ riskSurfaced = FALSE
  /\ state = [l \in Legs |-> "insync"]
  /\ writerSet = Legs
  /\ epochCut = {}
  /\ claim = "none"
  /\ deferExpired = FALSE
  /\ crashes = 0
  /\ rolling = {}
  /\ rolled = {}
  /\ suppress = {}
  /\ rollerDead = FALSE
  /\ stalePlan = {}
  /\ leaderMoved = FALSE
  /\ legSize = [l \in Legs |-> "old"]
  /\ raidSize = "old"
  /\ pvSize = "old"
  /\ wantNew = FALSE
  /\ bounceWindow = "none"
  /\ bouncePlan = FALSE
  /\ bounceRisk = FALSE
  /\ consecutiveBounces = 0
  /\ dpFlag = "none"
  /\ everServed = Legs                   \* Init serves every leg
  /\ podUp = TRUE
  /\ bounceDoomed = FALSE

(***************************************************************************)
(* Data plane                                                              *)
(***************************************************************************)

\* Synchronous mirror: an ack requires the write on EVERY serving leg, all
\* responsive.  A blackholed serving leg stalls writes — the P4 150-177s
\* ledger stall, observed live.
Write ==
  /\ serving # {}
  /\ nextWrite <= MaxWrites
  /\ \A l \in serving : Responsive(l)
  /\ legData' = [l \in Legs |->
                   IF l \in serving THEN legData[l] \cup {nextWrite}
                                    ELSE legData[l]]
  /\ acked' = acked \cup {nextWrite}
  /\ lineage' = lineage \cup {nextWrite}
  /\ nextWrite' = nextWrite + 1
  /\ UNCHANGED <<serving, zombie, legUp, raidGen, legGen, riskSurfaced,
                 state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The head crashes between replicating a block and acking the client: the
\* block lands on SOME serving legs, the client never hears.  Either outcome
\* is legitimate for the client — but the legs now disagree, and a holder
\* that misses the next assembly carries a dead-lineage phantom (the raw
\* material of the rejoin-divergence hazard).
WriteTorn ==
  /\ serving # {}
  /\ nextWrite <= MaxWrites
  /\ crashes < MaxCrashes
  /\ \A l \in serving : Responsive(l)
  /\ \E S \in (SUBSET serving) \ {{}} :
       legData' = [l \in Legs |->
                     IF l \in S THEN legData[l] \cup {nextWrite}
                                ELSE legData[l]]
  /\ nextWrite' = nextWrite + 1
  /\ serving' = {}
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<zombie, legUp, raidGen, legGen, acked, lineage,
                 riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The F48 zombie: the partitioned old head still holds its leg
\* connections and still acks client writes.  It writes OUTSIDE the
\* record — no CAS, no lineage growth.  Its own raid is a sync mirror
\* too: one downed view-member stalls it.
ZombieWrite ==
  /\ zombie # {}
  /\ nextWrite <= MaxWrites
  /\ \A l \in zombie : Responsive(l)
  /\ legData' = [l \in Legs |->
                   IF l \in zombie THEN legData[l] \cup {nextWrite}
                                   ELSE legData[l]]
  /\ acked' = acked \cup {nextWrite}
  /\ nextWrite' = nextWrite + 1
  /\ UNCHANGED <<serving, zombie, legUp, raidGen, legGen, lineage,
                 riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* Verified death straight away (terminated AND observed so).
LegDie(l) ==
  /\ legUp[l] = "up"
  /\ crashes < MaxCrashes
  /\ legUp' = [legUp EXCEPT ![l] = "dead"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* Silent unreachability: maybe a dying node, maybe a transient partition.
LegBlackhole(l) ==
  /\ legUp[l] = "up"
  /\ crashes < MaxCrashes
  /\ legUp' = [legUp EXCEPT ![l] = "blackhole"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The transient case: the leg returns, data intact, whatever the record
\* now says about it.  (The F36c ingredient.)
LegRecover(l) ==
  /\ legUp[l] = "blackhole"
  /\ legUp' = [legUp EXCEPT ![l] = "up"]
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* GROUND TRUTH: the silently-unreachable node actually dies (the cloud
\* reaped it).  WF here is the axiom that a blackhole eventually RESOLVES
\* — it perishes or it recovers; it does not hang forever.
LegPerish(l) ==
  /\ legUp[l] = "blackhole"
  /\ legUp' = [legUp EXCEPT ![l] = "dead"]
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* EVIDENCE: the record accepts node_gone proof (Node object deleted /
\* instance API says terminated).  With EvidenceStrict this only happens
\* for a truly dead node — the axiom the pre-resurrection model baked in.
\* Without it, a BLACKHOLED node can be deemed dead: a Node object
\* deleted while the instance still runs (the wedged-DS-roll unblock
\* recipe), or stale cloud state.  The belief persists through a
\* LegRecover — an alive node the record thinks is dead.
DeemDead(l) ==
  /\ l \notin deemedDead
  /\ IF EvidenceStrict THEN legUp[l] = "dead"
                       ELSE legUp[l] \in {"dead", "blackhole"}
  /\ deemedDead' = deemedDead \cup {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The data plane faults an unresponsive leg out; survivors continue at a
\* NEW incarnation (their superblocks record the shrink).  WF on this
\* action IS the P4 fix (TCP_USER_TIMEOUT + fast_io_fail bound detection).
\* ~Responsive, not legUp: P4 cannot tell a PLANNED tgt restart from a
\* failure — an unfenced roll of a serving leg gets faulted out exactly
\* like a blackhole.  That symmetry is deliberate (detection stays
\* always-on through maintenance); the fence keeps rolls out of its way
\* by never rolling a serving leg's tgt in the first place.
RaidDeconfigure(l) ==
  /\ l \in serving
  /\ ~Responsive(l)
  /\ serving' = serving \ {l}
  /\ raidGen' = raidGen + 1
  /\ legGen' = [m \in Legs |-> IF m \in serving \ {l} THEN raidGen + 1
                                                      ELSE legGen[m]]
  /\ UNCHANGED <<zombie, legData, legUp, acked, nextWrite, lineage,
                 riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The whole assembly dies cleanly (process gone, connections dropped).
ServerCrash ==
  /\ serving # {}
  /\ crashes < MaxCrashes
  /\ serving' = {}
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The F48 case: the head is PARTITIONED, not dead.  The record sees it
\* gone; the process lives on with its leg connections — a zombie.  (One
\* zombie at a time: a second partition waits for the first to be severed.)
ServerPartition ==
  /\ serving # {}
  /\ zombie = {}
  /\ crashes < MaxCrashes
  /\ zombie' = serving
  /\ serving' = {}
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* Control plane — each action is one CAS round against the record         *)
(***************************************************************************)

\* monitor_raid_health / record_stale_replicas + mark_stale's writer-set
\* removal: a leg missing from the ONLINE raid stopped receiving writes.
\* The gap between the deconfigure and this mark is a real race window —
\* the sb generations and the gate must hold across it.
MonitorMarkStale(l) ==
  /\ serving # {}
  /\ l \notin serving
  /\ state[l] = "insync"
  /\ state' = [state EXCEPT ![l] = "stale"]
  /\ writerSet' = writerSet \ {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* Epoch scheduler: cut a consistent snapshot of the served content.  The
\* cut is what the serving legs actually HOLD in common (bdev_lvol_snapshot
\* runs on the legs), NOT the acked ledger: after a ServeWithRisk assembly
\* excused a lost writer, acked content exists that no live leg holds, and
\* cutting `acked` would mint a GHOST epoch — an unsatisfiable chase
\* target that parks every later standby cold forever.  (A pre-existing
\* model bug, invisible to every content-shaped property — they all
\* escape on riskSurfaced — and to AdmissionNotStarved — a cold standby
\* is not WarmWaiting.  Found by ExpansionCompletes' first strict run,
\* the module's first per-leg progress obligation.)
EpochCut ==
  /\ serving # {}
  /\ \A l \in serving : Responsive(l)
  /\ epochCut' = { b \in 1..MaxWrites : \A l \in serving : b \in legData[l] }
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet, claim,
                 deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* replica_replace + prune_writers_for_replacement: swap the identity of a
\* stale leg whose node the record DEEMS dead (the C2 justification is
\* EVIDENCE, not an oracle); the freed slot returns as an empty STALE leg
\* with a new identity on a fresh node (so legUp resets to "up" and the
\* deemed flag is cleared — it referred to the old identity's node); the
\* full build later promotes it to standby (record_standby).
\* When the evidence was FALSE, the old node later resurrects with the
\* old identity's lvol and exports intact — the F44-F46/F49 residue
\* family, fixed live by the teardown/identity-domain work and below
\* this abstraction; the record-level machine stays safe because the old
\* identity is no longer referenced anywhere.
Replace(l) ==
  /\ state[l] = "stale"
  /\ l \in deemedDead
  /\ l \notin rolling                     \* no identity swap mid-restart
  \* The slot's old base must have left the raid first — reachable only
  \* via StaleFloor (a forced-stale SERVING member's node dies): P4's
  \* fast_io_fail (20s) outruns the 60s replace sweep, so at tick
  \* granularity RaidDeconfigure orders before Replace.  A swap racing
  \* the fault-out inside that window is an identity-domain overlap
  \* (record's new identity vs the raid's old base) below this
  \* slot-granularity abstraction — the F44-F46 note above.
  /\ l \notin serving
  /\ UpInSync # {}                        \* something to rebuild from
  /\ legUp' = [legUp EXCEPT ![l] = "up"]
  /\ legData' = [legData EXCEPT ![l] = {}]
  /\ legGen' = [legGen EXCEPT ![l] = 0]
  \* 2026-07-29 audit correction: the swapped-in record enters STALE
  \* (replica_replace.rs mints sync_state: Stale; the full build promotes
  \* to standby via record_standby) — NOT standby directly.  This matters
  \* for F57's scope: a replacement whose node dies BEFORE record_standby
  \* is still Stale and re-replaceable; only a leg that REACHED standby
  \* parks (no demotion path — the real F57 class).
  /\ state' = [state EXCEPT ![l] = "stale"]
  /\ writerSet' = writerSet \ {l}
  /\ deemedDead' = deemedDead \ {l}
  \* The slot's new identity lives on a FRESH node: the old node's roll
  \* flags do not apply to it (and the fresh node has not been rolled).
  /\ rolled' = rolled \ {l}
  /\ suppress' = suppress \ {l}
  \* The replacement placeholder lvol is sized from PV spec.capacity —
  \* i.e. pvSize, which is still the PRE-expand value until the whole
  \* fan-out succeeds (F56).  Safe regardless: the leg is write-virgin,
  \* and the §9-5 full build re-creates its head sized from the SOURCE
  \* (CatchUp's virgin arm).
  /\ legSize' = [legSize EXCEPT ![l] = pvSize]
  /\ UNCHANGED <<serving, zombie, raidGen, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, claim, falseRisk, crashes,
                 rolling, rollerDead, stalePlan, leaderMoved,
                 raidSize, pvSize, wantNew>>
  /\ UNCHANGED gateVars
  \* A swapped-in identity is a FRESH lvol: whatever the old one served,
  \* this one has not (the ghost behind Inv_WriterSetGrounded).
  /\ everServed' = everServed \ {l}
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, consecutiveBounces,
                 dpFlag, podUp, bounceDoomed>>

\* hot_rejoin_volume: a stale leg on a LIVE node re-enters as a standby
\* KEEPING its identity and payload (contrast Replace).  Whether the
\* payload is usable is decided downstream — by the RejoinGuard ancestry
\* check at catch-up/admission, or by Scrub when it diverges.
HotRejoin(l) ==
  /\ state[l] = "stale"
  /\ Responsive(l)
  \* A forced-stale SERVING leg cannot promote: catchup_stale defers with
  \* ReplicaHeadInUse while a consumer holds the head (StaleFloor states).
  /\ l \notin serving
  \* Per-leg on purpose even in the shipped code: the stale→standby
  \* promotion is catch-up dispatch (catchup_stale), filtered per-leg on
  \* maint_drain_live — the volume-wide widening (SuppressScoped = FALSE)
  \* gates only ADMISSION planning (plan_hot_rejoin), not this door.
  /\ l \notin suppress
  /\ state' = [state EXCEPT ![l] = "standby"]
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, writerSet, epochCut,
                 claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* HotRejoinScrubbed: no usable shared history with ANY live in-sync
\* source — wipe the payload and rebuild from scratch.  Requires a live
\* source to rebuild from: a scrub with nothing to rebuild from would
\* destroy the last copy.
Scrub(l) ==
  /\ claim = "catchup"                    \* reconciler work runs claimed
  /\ state[l] = "standby"
  /\ Responsive(l)
  /\ l \notin suppress
  /\ legData[l] # {}
  /\ UpInSync # {}
  /\ ~\E src \in UpInSync : legData[l] \subseteq legData[src]
  /\ legData' = [legData EXCEPT ![l] = {}]
  /\ legGen' = [legGen EXCEPT ![l] = 0]
  /\ UNCHANGED <<serving, zombie, legUp, raidGen, acked, nextWrite, lineage,
                 riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  \* The payload is wiped: a scrubbed leg's history no longer entitles it
  \* to anything (same ghost bookkeeping as Replace).
  /\ everServed' = everServed \ {l}
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, consecutiveBounces,
                 dpFlag, podUp, bounceDoomed>>

\* Catch-up: build to the last epoch cut from an in-sync source.  A block
\* copy fills holes and never erases — union semantics — so the shared-base
\* ancestry check (RejoinGuard) is what keeps a kept payload honest.
CatchUp(l) ==
  /\ claim = "catchup"                    \* builds run under the R2 claim
  /\ state[l] = "standby"
  /\ Responsive(l)
  /\ l \notin suppress
  /\ \E src \in UpInSync :
       /\ epochCut \subseteq legData[src]
       /\ (RejoinGuard => legData[l] \subseteq legData[src])
       \* Size (F56): the §9-5 full build re-creates a VIRGIN head sized
       \* from the source exactly (revert_head_to_empty) — true pre-fix
       \* too, which is why replacement legs never wedged.  The §5 chase
       \* keeps the leg's own (possibly pre-expand) size unless SizeHeal
       \* — align_dst_head_size in the chase session.
       /\ legSize' = [legSize EXCEPT ![l] =
                        IF legData[l] = {} \/ SizeHeal THEN legSize[src]
                                                       ELSE legSize[l]]
  /\ legData' = [legData EXCEPT ![l] = legData[l] \cup epochCut]
  /\ UNCHANGED <<serving, zombie, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED <<raidSize, pvSize, wantNew>>
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* Admission (hot-rejoin window / cutover reassembly) + mark_in_sync's
\* writer-set add: quiesced delta copy from a healthy serving survivor,
\* then join the raid at its current incarnation.  The ancestry check is
\* re-run HERE against the actual serving source — the admission-window
\* verification — because the world may have reassembled since catch-up.
Admit(l) ==
  /\ claim = "admission"                  \* the window holds its claim
  /\ state[l] = "standby"
  /\ Responsive(l)
  /\ AdmissionOpen(l)                     \* the second door — volume-wide
                                          \* when SuppressScoped = FALSE
                                          \* (the shipped plan_hot_rejoin)
  /\ epochCut \subseteq legData[l]        \* warm standby (caught up)
  /\ serving # {}
  /\ \E src \in serving :
       /\ Responsive(src)
       /\ (RejoinGuard => legData[l] \subseteq legData[src])
       \* F43 item #8, the catch-up admission size guard: never record
       \* in_sync for a head SHORTER than its copy source.  SizeHeal is
       \* the F56 fix — align_dst_head_size in admit_one_standby grows
       \* the head in the same session instead of deferring forever.
       /\ (SizeGuard => (SizeHeal \/ legSize[src] = "old" \/ legSize[l] = "new"))
       /\ legData' = [legData EXCEPT ![l] = legData[l] \cup legData[src]]
       /\ legSize' = [legSize EXCEPT ![l] =
                        IF SizeHeal THEN legSize[src] ELSE legSize[l]]
  \* The node-agent construction boundary (raid_add_size_verdict): a short
  \* leg never joins a raid whose consumer-visible device already grew.
  /\ (SizeGuard => (raidSize = "old" \/ legSize'[l] = "new"))
  /\ serving' = serving \cup {l}
  /\ legGen' = [legGen EXCEPT ![l] = raidGen]
  /\ state' = [state EXCEPT ![l] = "insync"]
  /\ writerSet' = writerSet \cup {l}
  /\ claim' = "none"                      \* mark_in_sync closes the window
  /\ UNCHANGED <<zombie, legUp, raidGen, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED <<raidSize, pvSize, wantNew>>
  /\ UNCHANGED gateVars
  \* An in-place admission is progress too — and it is precisely the S2
  \* door the bounce was supposed to be replaced by, so it resets the
  \* pointless-rebounce counter exactly like the at-stage one.
  /\ consecutiveBounces' = 0
  /\ everServed' = everServed \cup {l}
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, dpFlag, podUp, bounceDoomed>>

\* NodeStage reassembly: the F36c freshness gate over the ATTACHABLE
\* in-sync legs A, then SPDK examine serves only A's newest generation
\* (set_writer_set stamps the actually-serving membership).  Proceed:
\* every writer-set leg attaches.  ServeWithRisk: every missing writer
\* VERIFIABLY dead — serve and surface.  Defer is the absence of this
\* action.  GateStrict = FALSE is the pre-F36c bug: any in-sync legs will
\* do — and a lone returned stale leg is its own newest generation, which
\* is precisely the 6-write-tail loss.
\*   The new lineage's content is what the attached legs collectively
\* hold.  FenceZombie is the F48 fix: admission severs the old head's
\* consumers BEFORE serving; unfenced, the zombie keeps writing.
Assemble ==
  /\ serving = {}
  \* A reassembly happens because a POD is scheduled and NodeStage runs.
  /\ (PodLayer => podUp)
  \* StaleFloor (2026-07-29 audit): below 2 attached in-sync bases the
  \* shipped NodeStage AUTOMATICALLY admits record-Stale replicas (in
  \* replica-index order — TLC's free choice of A covers every order).
  \* An admitted stale leg keeps its content (no rebuild: the raid is a
  \* fresh superblock:false create), keeps record-state "stale", joins
  \* the writer set, and serves reads — StaleServed is the escape the
  \* content theorems take while it does.  NOTE a wave-2 code gap,
  \* modeled faithfully by its ABSENCE: the code's forced-stale loop
  \* consults neither maintenance suppression marks nor hot-rejoin
  \* markers (driver.rs admits even a marked replica, contradicting its
  \* own exclusion-phase comment).
  /\ LET StalePool == IF StaleFloor
                      THEN {l \in Legs : state[l] = "stale" /\ Responsive(l)}
                      ELSE {}
     IN
     \E A \in (SUBSET (UpInSync \cup StalePool)) \ {{}} :
       LET AIn  == A \cap UpInSync
           ASt  == A \ UpInSync
           \* MonitorCurrent is the record-currency axiom (see NewestOf):
           \* with it, only the newest generation of the in-sync
           \* attachers serves; without it every record-insync attacher
           \* serves — the shipped superblock:false reality.  Forced-
           \* stale members serve either way (knowingly behind).
           Kept == (IF MonitorCurrent THEN NewestOf(AIn) ELSE AIn) \cup ASt
       IN
       \* The 2-base floor: stale material only while in-sync attachers
       \* are short of 2, and never past 2 bases.
       /\ (ASt # {} => (Cardinality(AIn) < 2 /\ Cardinality(A) <= 2))
       /\ \/ /\ writerSet \subseteq A
             /\ UNCHANGED <<riskSurfaced, falseRisk, bounceRisk>>
          \/ /\ GateStrict
             /\ writerSet \ A # {}
             /\ \/ \A w \in writerSet \ A : w \in deemedDead
                \* The shipped gate's SECOND justification (GateDeadline):
                \* the persisted defer deadline passed — serve and surface
                \* EVEN IF the missing writers are only transiently gone
                \* ("Never hang", freshness_gate.rs).  This arm is what
                \* makes falseRisk reachable with sound evidence: the
                \* excused tail may be recoverable (GateRealHollow).
                \/ (GateDeadline /\ deferExpired)
             /\ riskSurfaced' = TRUE
             \* The harm ghost: excusing a writer that was NOT truly
             \* dead makes the surfaced risk hollow — the acked tail was
             \* recoverable all along.
             /\ falseRisk' = (falseRisk \/ \E w \in writerSet \ A : legUp[w] # "dead")
             \* THE BOUNCE HARM GHOST: this excusal was needed only
             \* because the CONTROLLER tore a volume down whose recorded
             \* writer set was already not whole.  A writer lost DURING
             \* the window is an ordinary failure and does not stamp it.
             /\ bounceRisk' = (bounceRisk \/ bounceWindow = "risky")
          \/ /\ ~GateStrict
             /\ UNCHANGED <<riskSurfaced, falseRisk, bounceRisk>>
       \* The NodeStage leg-size belt (F43 #8).  2026-07-29 audit: the
       \* shipped floor is PV spec.capacity (leg_size_guard reads the
       \* PV) — pvSize, which LAGS the device after a partial fan-out.
       \* DeviceFloor adds the device high-water mark (the fix); without
       \* it a lone pre-expand leg passes an old floor after the device
       \* grew (ExpandShrinkReal).  The largest-cohort preference is
       \* abstracted away: TLC's free choice of A covers every subset
       \* the belt could keep.
       /\ (SizeGuard =>
             \A m \in Kept :
               ((pvSize = "new" \/ (DeviceFloor /\ raidSize = "new"))
                  => legSize[m] = "new"))
       /\ serving' = Kept
       /\ writerSet' = Kept
       \* The trusted lineage is the in-sync attachers' content only: a
       \* forced-stale member's blocks are exactly the divergence hazard
       \* (its phantoms trip Inv_NoDivergentServing, escaped by
       \* StaleServed while it serves).
       /\ lineage' = UNION {legData[m] :
                              m \in (IF MonitorCurrent THEN NewestOf(AIn)
                                                       ELSE AIn)}
       /\ legGen' = [m \in Legs |-> IF m \in Kept THEN raidGen + 1
                                                  ELSE legGen[m]]
       /\ everServed' = everServed \cup Kept
  /\ raidGen' = raidGen + 1
  \* The volume is back: any open bounce window closes here (the judge's
  \* "did it come back" half, and the only place bounceWindow clears).
  /\ bounceWindow' = "none"
  /\ zombie' = IF FenceZombie THEN {} ELSE zombie
  \* Both code clear-sites for flint.io/f36c-defer are assembly-tick
  \* decisions (missing-empty and ServeWithRisk): the deadline re-arms
  \* fresh on the next deferral.
  /\ deferExpired' = FALSE
  /\ UNCHANGED <<legData, legUp, acked, nextWrite, state, epochCut, claim,
                 deemedDead, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED <<bouncePlan, consecutiveBounces, dpFlag, podUp, bounceDoomed>>

\* The stale-only-survivor LAST RESORT — the RUNBOOK step, not code: the
\* code's gate correctly Defers (the Deferred liveness escape), and an
\* OPERATOR may explicitly serve the freshest stale survivor, accepting
\* surfaced risk.  Modeled to verify the machine stays sound after the
\* override: riskSurfaced is stamped (every content invariant escapes
\* honestly), the sb generations restart from the survivor, and
\* post-override convergence holds.  No fairness — an operator choice.
LastResortServe(l) ==
  /\ serving = {}
  /\ UpInSync = {}                       \* nothing the gate could use
  /\ state[l] = "stale"
  /\ Responsive(l)
  /\ l \notin suppress                   \* never serve a mid-maintenance leg
  /\ l \in NewestOf({m \in Legs : state[m] = "stale" /\ Responsive(m)
                                  /\ m \notin suppress})
  \* Even the operator override stages through the leg-size belt: a
  \* survivor shorter than the grown device would silently shrink it.
  \* Same floor as Assemble: PV capacity, plus the device high-water
  \* under the DeviceFloor fix.
  /\ (SizeGuard => ((pvSize = "new" \/ (DeviceFloor /\ raidSize = "new"))
                      => legSize[l] = "new"))
  /\ serving' = {l}
  /\ writerSet' = {l}
  /\ lineage' = legData[l]
  /\ legGen' = [legGen EXCEPT ![l] = raidGen + 1]
  /\ raidGen' = raidGen + 1
  /\ riskSurfaced' = TRUE
  /\ state' = [state EXCEPT ![l] = "insync"]
  /\ zombie' = IF FenceZombie THEN {} ELSE zombie
  /\ deferExpired' = FALSE               \* an assembly happened
  /\ UNCHANGED <<legData, legUp, acked, nextWrite, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  \* The override brings the volume back, so any open bounce window closes
  \* (with riskSurfaced already stamped by the override itself).
  /\ bounceWindow' = "none"
  /\ everServed' = everServed \cup {l}
  /\ UNCHANGED <<bouncePlan, bounceRisk, consecutiveBounces, dpFlag, podUp, bounceDoomed>>

(***************************************************************************)
(* The R2 claim — the F43 machinery.  Catch-up work (builds, scrubs) and   *)
(* the admission window each run under a per-volume claim.  The F43 bug:   *)
(* catch-up's timer renewal always won the reacquisition race, so the      *)
(* admission claim — and the warm standby behind it — starved.  The fix    *)
(* is PRIORITY (ClaimArb): catch-up may not (re)acquire while a warm       *)
(* standby awaits admission, which makes admission's enabling continuous   *)
(* and lets weak fairness fire it.  Note stronger fairness alone would     *)
(* NOT fix the real system: the model shows the un-arbitrated race is      *)
(* fair-legal precisely because admission's enabling keeps being           *)
(* interrupted.                                                            *)
(***************************************************************************)

\* NOTE (the expand tranche's fairness finding): AcquireCatchup is
\* deliberately NOT work-gated.  The code's tick claims on every firing
\* — the probe runs UNDER the claim (contract R2), and the epoch
\* scheduler's timer is writes-independent (60s planner tick, 300s cut) — so the no-op claim
\* cycle is real, and it is the ENGINE of the F43 lasso (work-gating
\* these actions was tried and deleted the F43 mutation's
\* counterexample outright).  The consequence: any contender needing
\* claim = "none" is only intermittently enabled, which weak fairness
\* never obligates.  The system's own answer for admission is ClaimArb
\* (priority — the F43 fix); for the claim-holder's OWN dispatch work
\* (CatchUp/Scrub) and for the external resizer's persistent retry
\* (ExpandLeg), the honest abstraction is STRONG fairness — see the
\* notes in FairnessCore.
AcquireCatchup ==
  /\ claim = "none"
  /\ (ClaimArb => ~WarmWaiting)           \* the F43 yield rule
  /\ claim' = "catchup"
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

ReleaseCatchup ==
  /\ claim = "catchup"
  /\ claim' = "none"
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

AcquireAdmission ==
  /\ claim = "none"
  /\ WarmWaiting                          \* something to admit
  /\ claim' = "admission"
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The window's deferral arm: the world changed between the open and the
\* flip — the leg de-warmed (a fresh epoch cut), died, was sized out, or
\* its source vanished — and the admission can no longer proceed.  The
\* real window RELEASES its claim on every deferral
\* (StandbyAdmissionDeferred; the claim guard is RAII around the window
\* task) and the next reassembly re-opens.  Without this the claim
\* wedges at "admission" once the crash budget (ExpireClaim's fuel) is
\* spent — a hold-forever the code does not have.  Found by the expand
\* tranche's strict run: AcquireAdmission → EpochCut de-warms the
\* standby → Admit disabled forever; every pre-expansion property was
\* blind to it (AdmissionNotStarved is satisfied BECAUSE the de-warm
\* falsifies WarmWaiting).
ReleaseAdmission ==
  /\ claim = "admission"
  /\ ~WarmWaiting                         \* nothing admissible: defer + close
  /\ claim' = "none"
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* Lease expiry: the claim holder died; the lease frees the claim.  A
\* controller death is a failure event — budgeted — which is also what
\* keeps expire/reacquire churn finite.
ExpireClaim ==
  /\ claim # "none"
  /\ crashes < MaxCrashes
  /\ claim' = "none"
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* Planned maintenance — the csi-node roll.  A DaemonSet roll restarts    *)
(* spdk-tgt node by node; each restart is a planned data-plane outage on  *)
(* that node.  The campaign is finite (rolled is monotone: one campaign,  *)
(* each node once) and costs NO crash budget — which is exactly what      *)
(* lets Inv_PlannedRollNeverCausesOutage condition on crashes = 0.        *)
(*                                                                        *)
(* MaintDrain is the fence's first half: one CAS that gracefully removes  *)
(* the leg from the serving raid (survivors continue at a new             *)
(* incarnation), stale-marks it, prunes the writer set, and stamps the    *)
(* suppression mark.  No detection wait, no write stall: the raid never   *)
(* sees a silent member.  With MaintBarrier the drain additionally waits  *)
(* for FULL redundancy (every leg in-sync + serving + responsive) — the   *)
(* readmission barrier.  Without it the guard is only what k8s            *)
(* maxUnavailable=1 actually gives you: one node in maintenance at a      *)
(* time, pod-level knowledge only — and TLC finds the outage where the    *)
(* previous leg's pod is Ready but its leg is still un-readmitted.        *)
(*                                                                        *)
(* RollStart/RollFinish bracket the tgt restart itself (kubelet's work:   *)
(* RollFinish is weakly fair — restarts complete — and fires whether the  *)
(* roll ORCHESTRATOR lives or not).  Under the fence a serving leg's tgt  *)
(* is never taken down (RollStart requires the suppression mark, which    *)
(* only MaintDrain mints); unfenced, RollStart hits a serving leg and     *)
(* the P4 machinery treats it exactly like a blackhole — the landmine.    *)
(*                                                                        *)
(* MaintClear lifts the suppression mark after the restart (a LIVE        *)
(* roller's act).  RollerDie is the budgeted failure: the orchestrator    *)
(* dies mid-campaign.  SuppressExpire is the lease: a dead roller's mark  *)
(* self-clears (TTL).  Without it — MaintLease = FALSE — the mark         *)
(* outlives its holder and the drained leg parks at reduced redundancy    *)
(* forever: the F43 parked standby re-created by a maintenance flag.      *)
(***************************************************************************)

FullRedundancy ==
  \A m \in Legs : state[m] = "insync" /\ m \in serving /\ Responsive(m)

\* What the implementation's barrier actually reads: the RECORD alone.
\* Weaker than FullRedundancy exactly by the monitor-tick lag between a
\* member failing and its stale-mark landing.
RecordRedundancy == \A m \in Legs : state[m] = "insync"

MaintDrain(l) ==
  /\ MaintEnabled /\ MaintFence
  /\ ~rollerDead
  /\ l \in serving
  /\ state[l] = "insync"
  /\ Responsive(l)
  /\ l \notin rolled                      \* one campaign, each node once
  /\ rolling = {} /\ suppress = {}        \* k8s pod-level serialization
  \* DATA-PLANE BELT: never drain the last serving member.  Found by the
  \* RecordBarrier run: with a record-only barrier, a deconfigured-but-
  \* not-yet-stale-marked survivor makes the record lie ("both insync")
  \* and a record-level last-leg check passes — the drain then removes
  \* the SOLE serving leg holding the acked tail and prunes it from the
  \* writer set: silent loss in 7 states.  The belt must read GROUND
  \* TRUTH (the code probes the raid BEFORE the record round —
  \* maint_roll.rs drain_leg's configured-base count).  DrainBelt = FALSE
  \* is the PRE-fix record-level belt (another recorded-insync leg
  \* suffices): the RollNoBelt run must rediscover the silent loss —
  \* restoring this bug class's mutation (2026-07-29 audit: the fix had
  \* erased the pre-fix world from the configuration space, violating
  \* the module's own rediscoverability rule).
  /\ IF DrainBelt THEN serving \ {l} # {}
                  ELSE \E m \in Legs \ {l} : state[m] = "insync"
  /\ (MaintBarrier =>                     \* readmitted, not just pod-ready
        IF BarrierRaidAware THEN FullRedundancy ELSE RecordRedundancy)
  /\ serving' = serving \ {l}
  /\ raidGen' = raidGen + 1
  /\ legGen' = [m \in Legs |-> IF m \in serving \ {l} THEN raidGen + 1
                                                      ELSE legGen[m]]
  /\ state' = [state EXCEPT ![l] = "stale"]
  /\ writerSet' = writerSet \ {l}
  /\ suppress' = suppress \cup {l}
  /\ UNCHANGED <<zombie, legData, legUp, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, claim, deemedDead, falseRisk,
                 crashes, rolling, rolled, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

RollStart(l) ==
  /\ MaintEnabled
  /\ ~rollerDead
  /\ legUp[l] = "up"
  /\ rolling = {} /\ l \notin rolled
  /\ (MaintFence => l \in suppress)       \* drain first — the fence
  /\ rolling' = {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 rolled, suppress, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* kubelet completes the restart — weakly fair, roller-independent.
RollFinish(l) ==
  /\ rolling = {l}
  /\ rolling' = {}
  /\ rolled' = rolled \cup {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 suppress, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

MaintClear(l) ==
  /\ ~rollerDead
  /\ l \in suppress
  /\ l \in rolled                         \* restart done for this node
  /\ suppress' = suppress \ {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 rolling, rolled, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The roll orchestrator dies mid-campaign while holding a mark — a
\* budgeted failure event, like ExpireClaim.
RollerDie ==
  /\ MaintEnabled
  /\ ~rollerDead
  /\ suppress # {}
  /\ crashes < MaxCrashes
  /\ rollerDead' = TRUE
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk,
                 rolling, rolled, suppress, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The lease: a dead roller's suppression mark self-clears after TTL
\* (never mid-restart — the TTL far exceeds a pod restart, and
\* RollFinish is fair).  MaintLease = FALSE is the mutation: the mark
\* outlives its holder forever.
SuppressExpire(l) ==
  /\ MaintLease
  /\ rollerDead
  /\ l \in suppress
  /\ l \notin rolling
  /\ suppress' = suppress \ {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 rolling, rolled, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* THE TWO-ROLLER RACE (RollerRace; 2026-07-29).  The audit asserted in    *)
(* prose that the roller's lease is safety-load-bearing; scouting the      *)
(* code showed WHY the question is live: one-node-at-a-time is enforced    *)
(* only in the PLANNER, from the tick's gather snapshot; the record CAS    *)
(* retries by re-running drain_for_maintenance against the fresh record   *)
(* (preventing lost updates, not concurrent drains — its only guards are  *)
(* target-exists, target-not-rejoin-marked, and if-insync-another-insync- *)
(* remains); the ground-truth probe refuses only the LAST configured      *)
(* base; and is_leader() is one in-process atomic read at tick top while  *)
(* the tick's RPC work is unbounded (300s HTTP timeouts × N volumes vs a  *)
(* 15s lease).  So a deposed-but-alive roller's in-flight drain can land  *)
(* AFTER the new leader's drain marked a different node.  RoguePlanDrain  *)
(* captures a plan valid at capture time; RogueDrainCommit lands it later *)
(* applying ONLY the commit-time guards.  The RollerRace run must FIND    *)
(* Inv_PlannedRollBoundedImpact violated at 3 legs WITH the leader gate   *)
(* ON — the lease cannot close a race it checks before the work — and     *)
(* RollerRaceFixed (DrainMarksBelt, no gate at all) must HOLD: the        *)
(* exclusivity belt inside the mutation carries safety alone.  2-leg      *)
(* volumes were only ever protected incidentally, by the other-insync     *)
(* cardinality guard.                                                      *)
(***************************************************************************)

RoguePlanDrain(l) ==
  /\ RollerRace
  /\ MaintEnabled /\ MaintFence
  /\ ~rollerDead
  /\ stalePlan = {}
  \* With the gate, a stale plan requires the one budgeted deposal
  \* overlap: the roller WAS leader at tick top, leadership moved after.
  \* Deliberately NOT a crash-budget event — a deposal is not a volume
  \* failure, and Inv_PlannedRollBoundedImpact conditions on crashes = 0.
  /\ (RollerLeaderGate => ~leaderMoved)
  /\ leaderMoved' = (leaderMoved \/ RollerLeaderGate)
  \* plan_roll's planner-side checks, all valid RIGHT NOW (in code they
  \* are snapshot reads: insync_by_node, marked_nodes empty, the barrier).
  /\ l \in serving
  /\ state[l] = "insync"
  /\ Responsive(l)
  /\ l \notin rolled
  /\ rolling = {} /\ suppress = {}
  /\ (MaintBarrier =>
        IF BarrierRaidAware THEN FullRedundancy ELSE RecordRedundancy)
  /\ stalePlan' = {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 rolling, rolled, suppress, rollerDead>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The captured plan lands — possibly long after the world changed.  Only
\* the guards the code re-runs at commit time apply: the fresh raid probe
\* (raid exists; never remove the last configured base) and
\* drain_for_maintenance's fresh-record guards (target insync => another
\* insync leg remains).  Deliberately ABSENT, matching the shipped code:
\* marked-set-empty, the barrier, rolling/rolled — planner-only snapshot
\* reads.  DrainMarksBelt is the fix, where the rv-guarded retry makes it
\* race-proof.
RogueDrainCommit ==
  /\ RollerRace
  /\ ~rollerDead
  /\ \E l \in stalePlan :
       /\ state[l] = "insync"             \* an already-drained target no-ops
       /\ serving # {}                    \* probe A: the raid exists
       /\ (l \in serving => serving \ {l} # {})  \* probe B: last configured base
       /\ \E m \in Legs \ {l} : state[m] = "insync"  \* other_insync, fresh record
       \* THE FIX: exclusivity AND the readmission barrier re-verified in
       \* the CAS — marks-empty alone loses to the capture→drain→clear→
       \* commit erosion (RollerRaceFixed's first counterexample).
       /\ (DrainMarksBelt => (suppress = {}
                               /\ \A m \in Legs \ {l} : state[m] = "insync"))
       /\ serving' = serving \ {l}
       /\ IF l \in serving
          THEN /\ raidGen' = raidGen + 1
               /\ legGen' = [m \in Legs |-> IF m \in serving \ {l} THEN raidGen + 1
                                                                   ELSE legGen[m]]
          ELSE UNCHANGED <<raidGen, legGen>>
       /\ state' = [state EXCEPT ![l] = "stale"]
       /\ writerSet' = writerSet \ {l}
       /\ suppress' = suppress \cup {l}
  /\ stalePlan' = {}
  /\ UNCHANGED <<zombie, legData, legUp, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, claim, deemedDead, falseRisk,
                 crashes, rolling, rolled, rollerDead, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* THE CUTOVER BOUNCE (cutover.rs).  A controller-initiated, ZERO-FAILURE  *)
(* teardown of a serving data path, issued so that the ordinary NodeStage  *)
(* reassembly runs and picks up a warm standby (or rebuilds a lost data    *)
(* path).  NodeUnstage's teardown_volume_spdk_state deletes the raid bdev  *)
(* and every per-replica controller and touches NO record field — no       *)
(* stale marks, no writer-set prune — so this is serving := {} with state, *)
(* writerSet, legData, legGen and epochCut all UNCHANGED.  Like the roll   *)
(* it costs NO crash budget: it is planned work, not a failure.            *)
(*                                                                         *)
(* NOTE what the guard does NOT mention: any leg health variable, serving  *)
(* membership, or writerSet.  That is not an abstraction — it is the       *)
(* shipped planner.  VolumeCutoverView (cutover.rs:271-289) carries none   *)
(* of them and plan_cutover (305-385) reads only sync_state and            *)
(* last_epoch.  BouncePreflight is the only place this module puts them    *)
(* back, which is the tranche's whole point.                               *)
(*                                                                         *)
(* The pod layer is deliberately abstracted away (delete → detach-wait →   *)
(* recreate, the liveness reconciler as a second creator, delete-by-name   *)
(* with no UID precondition).  Justification: the audit's verifier closed  *)
(* the AVAILABILITY question — rwx_nfs.rs's reconciler recreates an        *)
(* Absent/Dead server counting attachment INTENT, so the volume comes      *)
(* back within about one 30s tick — and the residuals it leaves (a         *)
(* spurious CutoverFailed, one stalled detach) are not durability facts.   *)
(* Same move by which atomic Assemble stands for six code hops.            *)
(***************************************************************************)

\* plan_cutover's two DISJOINT arms.  The data-path arm short-circuits
\* everything (cutover.rs:312-334); the standby arm needs a CONVERGED
\* standby (336-356).  The code's per-standby loop is an ALL-quantifier;
\* at these leg counts, with at most one standby, that coincides with the
\* existential.
BounceDataPathArm  == DataPathArm  /\ dpFlag # "none"
BounceAdmissionArm == AdmissionArm /\ \E l \in Legs :
                        /\ state[l] = "standby"
                        /\ epochCut \subseteq legData[l]
                        \* PlannerDisjoint: the shipped plan_cutover applies
                        \* NEITHER of plan_hot_rejoin's filters, so it can
                        \* plan a bounce for a standby the stage admission
                        \* will then refuse — the predicate is unsatisfiable
                        \* before the bounce is even issued.
                        /\ (PlannerDisjoint => AdmissionOpen(l))

\* The ONLY suppressor of a new plan in the shipped code is a LIVE ATTEMPT
\* RECORD — a stack-local HashMap (cutover.rs:706).  Deliberately NOT
\* modeled: at tick granularity the judge's unconditional re-arm at the
\* cooldown, the Err arm that records nothing at all (1058-1067), and the
\* standby-success branch's missing `continue` (912 — the one branch
\* without it, so planning re-runs in the SAME tick) all collapse to
\* "eligibility returns".  Modeling the record would only REMOVE behaviors
\* the code demonstrably has.
BouncePlannable ==
  /\ BounceEnabled
  /\ serving # {}                         \* something to tear down
  /\ (BounceDataPathArm \/ BounceAdmissionArm)

\* Was this bounce doomed the moment it was issued?  TRUE when the ONLY
\* justification was the admission arm and no standby the stage admission
\* would actually accept exists — plan_cutover applies neither
\* plan_hot_rejoin's maintenance-suppression filter nor its marker filter,
\* so it can issue a teardown whose purpose is unachievable before it
\* starts.  This is the attributive statement the shared churn canary
\* cannot make.
BounceDoomedAtCommit ==
  /\ ~BounceDataPathArm
  /\ ~\E l \in Legs : /\ state[l] = "standby"
                      /\ epochCut \subseteq legData[l]
                      /\ AdmissionOpen(l)

\* THE PROPOSED COMMIT-TIME BELT.  FALSE = shipped (no term of any kind).
BounceSafe ==
  \/ ~BouncePreflight
  \/ \A w \in writerSet : Responsive(w) \/ w \in deemedDead

\* Was the recorded writer set ALREADY broken when the controller tore the
\* volume down?  The discriminator between "the bounce manufactured it"
\* and "a failure landed inside the window".
BounceRiskAtCommit ==
  IF \A w \in writerSet : Responsive(w) \/ w \in deemedDead
  THEN "clean" ELSE "risky"

\* The in-process bounce: plan and execute in one tick.  The plan→execute
\* gap inside one process is a few API round trips (try_claim + one node
\* GET/PATCH), so one step is the faithful abstraction; the CROSS-process
\* gap is RogueBounce* below.
Bounce ==
  /\ ~PodLayer                            \* the pod-layer world splits this
  /\ BouncePlannable
  /\ BounceSafe
  /\ consecutiveBounces < MaxBounces
  /\ serving' = {}
  /\ bounceWindow' = BounceRiskAtCommit
  /\ bounceDoomed' = (bounceDoomed \/ BounceDoomedAtCommit)
  /\ consecutiveBounces' = consecutiveBounces + 1
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim,
                 deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bouncePlan, bounceRisk, dpFlag, everServed, podUp>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars

\* The deposed-but-alive bouncer.  RoguePlanDrain's shape, and leaderMoved
\* is SHARED with the roller on purpose: orchestrator_lease.rs is ONE lease
\* across all six leader-gated sites, so one deposal deposes every
\* orchestrator at once.
RogueBouncePlan ==
  /\ BounceRace /\ ~PodLayer
  /\ ~bouncePlan
  /\ (BounceLeaderGate => ~leaderMoved)
  /\ leaderMoved' = (leaderMoved \/ BounceLeaderGate)
  /\ BouncePlannable                      \* every planner term, valid NOW
  /\ bouncePlan' = TRUE
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 rolling, rolled, suppress, rollerDead, stalePlan>>
  /\ UNCHANGED <<bounceWindow, bounceRisk, consecutiveBounces, dpFlag,
                 everServed, podUp, bounceDoomed>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars

\* The captured plan lands, possibly long after the world changed.  ONLY
\* the guards the code RE-RUNS at commit time apply — and there is exactly
\* one: execute_cutover's get_pod (cutover.rs:465), i.e. "the thing is
\* still there".  Deliberately ABSENT, matching the shipped code: the
\* other process's attempt record (volume_claims::global() is a per-process
\* OnceLock and `bounces` is a stack local — mutually invisible), the arms'
\* own guards, leadership.  THE DECISIVE ASYMMETRY WITH THE ROLLER: where
\* drain_for_maintenance had an rv-guarded record CAS to move DrainMarksBelt
\* INTO, cutover has NO CAS anywhere — delete_pod uses DeleteParams::
\* default(), recreate_pod is a bare create, taint_node is a whole-array
\* merge patch.  A commit-time preflight is the only belt this subsystem
\* can host.
RogueBounceCommit ==
  /\ BounceRace /\ ~PodLayer
  /\ bouncePlan
  /\ serving # {}
  /\ BounceSafe
  /\ consecutiveBounces < MaxBounces
  /\ serving' = {}
  /\ bounceWindow' = BounceRiskAtCommit
  /\ bounceDoomed' = (bounceDoomed \/ BounceDoomedAtCommit)
  /\ consecutiveBounces' = consecutiveBounces + 1
  /\ bouncePlan' = FALSE
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim,
                 deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bounceRisk, dpFlag, everServed, podUp>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars

(***************************************************************************)
(* THE POD LAYER — the bounce as the code actually performs it, in FOUR    *)
(* steps instead of one, because two independent processes create the same *)
(* bare pod and NOTHING mutually excludes them.                            *)
(*                                                                         *)
(*   execute_cutover:  get_pod (spec into a LOCAL) → taint → delete_pod    *)
(*                     → await_detached(<= detach_timeout, 2s poll)        *)
(*                     → recreate_pod                                      *)
(*   nfs_reconciler_pass (30s tick, same process, same lease, separate     *)
(*                     tokio task): liveness Absent + client attachments   *)
(*                     >= 1  ⇒  Recreate.                                  *)
(*                                                                         *)
(* The detach wait exists to hold the pod down until kubelet UNSTAGES, so  *)
(* the replacement is forced to restage and reassemble the raid (the §6    *)
(* same-node race).  For that entire wait the pod is Absent with client    *)
(* attachments intact — the cutover waits on the BACKING PV's              *)
(* VolumeAttachment, the reconciler counts VAs on the USER PV — so the     *)
(* wait sits precisely inside the reconciler's one Recreate cell.  If the  *)
(* pod returns BEFORE the unstage, kubelet reuses the staged volume: no    *)
(* NodeStage, no reassembly, no admission — the bounce is silently         *)
(* defeated, clients ate a grace-window recovery for nothing, and (in the  *)
(* code) the bouncer's own recreate then 409s into the taken name, takes   *)
(* the Err arm, and records NO attempt at all.                             *)
(***************************************************************************)

\* delete_pod.  The volume is NOT unstaged yet — this is exactly the window
\* await_detached spends polling.
BounceDelete ==
  /\ PodLayer
  /\ podUp
  /\ BouncePlannable
  /\ BounceSafe
  /\ consecutiveBounces < MaxBounces
  /\ podUp' = FALSE
  /\ bounceWindow' = BounceRiskAtCommit
  /\ bounceDoomed' = (bounceDoomed \/ BounceDoomedAtCommit)
  /\ consecutiveBounces' = consecutiveBounces + 1
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bouncePlan, bounceRisk, dpFlag, everServed>>
  /\ UNCHANGED maintVars /\ UNCHANGED expandVars /\ UNCHANGED gateVars

\* kubelet's NodeUnstage, once the pod is really gone: the raid bdev and
\* every per-replica controller are torn down.  THIS is what the detach
\* wait is waiting for, and the only thing that makes the replacement
\* restage.
BounceUnstage ==
  /\ PodLayer
  /\ ~podUp
  /\ serving # {}
  /\ serving' = {}
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim,
                 deemedDead, falseRisk, crashes>>
  /\ UNCHANGED bounceVars
  /\ UNCHANGED maintVars /\ UNCHANGED expandVars /\ UNCHANGED gateVars

\* The bouncer's own recreate.  DetachWaitHonored = TRUE is the idealized
\* bouncer that recreates only after the unstage it waited for; FALSE is
\* the shipped timeout path, where await_detached returning false merely
\* WARNS and execute_cutover recreates anyway.
BounceRecreate ==
  /\ PodLayer
  /\ ~podUp
  /\ (DetachWaitHonored => serving = {})
  /\ podUp' = TRUE
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, consecutiveBounces,
                 dpFlag, everServed, bounceDoomed>>
  /\ UNCHANGED maintVars /\ UNCHANGED expandVars /\ UNCHANGED gateVars

\* THE SECOND CREATOR.  Enabled purely on "pod gone" — the shipped
\* reconciler cannot see a bounce, and its decision function's signature
\* proves it: nfs_reconcile_decision(backend_is_emptydir, pv_terminating,
\* attachment_count, liveness) has no input that could carry one.
\* ReconcilerBelt is the proposed fix (hold off while a window is open).
ReconcilerRecreate ==
  /\ PodLayer
  /\ ~podUp
  /\ (ReconcilerBelt => bounceWindow = "none")
  /\ podUp' = TRUE
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, consecutiveBounces,
                 dpFlag, everServed, bounceDoomed>>
  /\ UNCHANGED maintVars /\ UNCHANGED expandVars /\ UNCHANGED gateVars

(***************************************************************************)
(* THE DATA-PATH-LOST ANNOTATION (node_agent.rs detect_lost_data_paths).   *)
(* Written by a leg's OWN agent after consecutive raid-missing strikes     *)
(* under a live attachment.  The value is "<node>|<since>", and            *)
(* DataPathAction::Clear fires ONLY when flagged_by_me — the value's node  *)
(* prefix equals this agent's node (node_agent.rs:5241-5247).  A           *)
(* permanently-gone flagger therefore leaves a flag NOTHING can clear:     *)
(* the only controller-side sweep is gated on is_rwx && own_flag           *)
(* (cutover.rs:834-851), never a backing PV and never an RWO PV.  This is  *)
(* the CONFIRMED ownership trap behind the pointless-rebounce canary.      *)
(***************************************************************************)
AgentFlag(l) ==
  /\ BounceEnabled /\ DataPathArm
  /\ dpFlag = "none"
  /\ Responsive(l)                        \* a LIVE agent writes it
  /\ l \notin serving                     \* "the raid bdev is missing here"
  /\ state[l] = "insync"                  \* "...but the record calls it a writer"
  /\ dpFlag' = l
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, consecutiveBounces,
                 everServed, podUp, bounceDoomed>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars

AgentClear(l) ==
  /\ dpFlag = l
  /\ Responsive(l)                        \* ONLY the flagging node's agent
  /\ l \in serving                        \* the restage put ITS raid back
  /\ dpFlag' = "none"
  /\ consecutiveBounces' = 0              \* the bounce accomplished its job
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, everServed, podUp, bounceDoomed>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars

(***************************************************************************)
(* admit_standbys_at_stage — THE ADMISSION THE BOUNCE EXISTS TO TRIGGER,   *)
(* and the one Admit cannot represent.  It runs in the NODE process        *)
(* (driver.rs:1967, from NodeStage), under NO volume claim (cutover's      *)
(* claim was dropped when execute_cutover returned), with the raid NOT YET *)
(* CREATED, and it commits record_in_sync — which GROWS the writer set —   *)
(* BEFORE the freshness gate rules (driver.rs:2089).  The copy source is   *)
(* the attaching in-sync set, all fenced to this node with no writer live, *)
(* which is why the code can call the skew zero (catchup.rs:2516-2518).    *)
(* The epoch-chain half of this hop (the persisted cut, and the one leaked *)
(* epoch per failed attempt the audit booked) is NOT modeled: epochCut is  *)
(* a single set here, not a chain — a pre-existing scope limit.            *)
(***************************************************************************)
AdmitAtStage(l) ==
  /\ StageAdmit
  /\ serving = {}                         \* a stage is in progress
  /\ (PodLayer => podUp)
  /\ state[l] = "standby"
  /\ Responsive(l)
  /\ AdmissionOpen(l)
  /\ epochCut \subseteq legData[l]        \* the warm/lag re-check
  /\ \E src \in {m \in Legs : state[m] = "insync" /\ Responsive(m)} :
       /\ (RejoinGuard => legData[l] \subseteq legData[src])
       /\ (SizeGuard => (SizeHeal \/ legSize[src] = "old" \/ legSize[l] = "new"))
       /\ legData' = [legData EXCEPT ![l] = legData[l] \cup legData[src]]
       /\ legSize' = [legSize EXCEPT ![l] =
                        IF SizeHeal THEN legSize[src] ELSE legSize[l]]
  /\ state'     = [state EXCEPT ![l] = "insync"]
  /\ writerSet' = writerSet \cup {l}      \* mark_in_sync's DURABLE growth...
  /\ consecutiveBounces' = 0
  /\ UNCHANGED serving                    \* ...and the gate has NOT ruled yet
  /\ UNCHANGED claim                      \* the correspondence point: unclaimed
  /\ UNCHANGED <<zombie, legUp, raidGen, legGen, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, dpFlag, everServed, podUp, bounceDoomed>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED <<raidSize, pvSize, wantNew>>
  /\ UNCHANGED gateVars

(***************************************************************************)
(* Online expansion — the F56 size dimension                               *)
(* (docs/f56-expand-replacement-circular-wait.md).  ExpandRequest is the  *)
(* resizer's outstanding desire (latched; the retry loop is WF on         *)
(* ExpandLeg).  ExpandLeg is ONE leg's resize landing — deliberately      *)
(* non-atomic across legs: a leg lost between two ExpandLegs is exactly   *)
(* the partial fan-out that manufactures the size divergence.  The C2     *)
(* belt (every RECORDED leg in_sync) gates each step, and the fan-out     *)
(* runs under the Maintainer claim (claim = "none" here: acquire +        *)
(* resize + release folded into the action; resolvers preempt it in the  *)
(* real registry, which only narrows its enabling).  RaidGrow is the     *)
(* event-driven lvol→nvmf→bdev_nvme→raid1_resize chain: the consumer-    *)
(* visible device grows only once EVERY serving base grew (min-capped),  *)
(* and never shrinks back — raidSize is the high-water mark the size     *)
(* guards defend.                                                        *)
(***************************************************************************)

ExpandRequest ==
  /\ ExpandEnabled
  /\ ~wantNew
  /\ wantNew' = TRUE
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 legSize, raidSize, pvSize>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

ExpandLeg(l) ==
  /\ ExpandEnabled
  /\ wantNew
  /\ claim = "none"                       \* the OP_EXPAND Maintainer claim
  /\ legSize[l] = "old"
  /\ Responsive(l)                        \* the node agent must answer
  /\ \A m \in Legs : state[m] = "insync"  \* the C2 belt, read from the record
  /\ legSize' = [legSize EXCEPT ![l] = "new"]
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 raidSize, pvSize, wantNew>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

RaidGrow ==
  /\ ExpandEnabled
  /\ raidSize = "old"
  /\ serving # {}
  /\ \A l \in serving : legSize[l] = "new"
  /\ raidSize' = "new"
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 legSize, pvSize, wantNew>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* The external resizer's success path: ControllerExpandVolume returns
\* Done only when EVERY replica grew (expand_replicated's all-or-
\* Unavailable fan-out), and the resizer then patches PV spec.capacity.
\* THIS is why pvSize lags raidSize after a partial fan-out: the device
\* (RaidGrow) needs only the SERVING legs grown.  WF — the resizer is a
\* persistent retrier and this action, once enabled, stays enabled.
PvGrow ==
  /\ ExpandEnabled
  /\ wantNew
  /\ pvSize = "old"
  /\ \A l \in Legs : legSize[l] = "new"
  /\ pvSize' = "new"
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 legSize, raidSize, wantNew>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* Wall-clock passage on a deferring volume: the persisted
\* flint.io/f36c-defer deadline (default 180s) elapses.  WF — time
\* always advances; this is the engine of the gate's "Never hang"
\* obligation and of the GateRealHollow finding.
DeferClockExpire ==
  /\ GateDeadline
  /\ serving = {}
  /\ ~deferExpired
  /\ deferExpired' = TRUE
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED bounceVars

Next ==
  \/ Write
  \/ WriteTorn
  \/ ZombieWrite
  \/ EpochCut
  \/ ServerCrash
  \/ ServerPartition
  \/ Assemble
  \/ AcquireCatchup
  \/ ReleaseCatchup
  \/ AcquireAdmission
  \/ ReleaseAdmission
  \/ ExpireClaim
  \/ RollerDie
  \/ RogueDrainCommit
  \/ ExpandRequest
  \/ RaidGrow
  \/ PvGrow
  \/ DeferClockExpire
  \/ Bounce
  \/ RogueBouncePlan
  \/ RogueBounceCommit
  \/ BounceDelete
  \/ BounceUnstage
  \/ BounceRecreate
  \/ ReconcilerRecreate
  \/ \E l \in Legs :
       \/ LegDie(l)
       \/ LegBlackhole(l)
       \/ LegRecover(l)
       \/ LegPerish(l)
       \/ DeemDead(l)
       \/ RaidDeconfigure(l)
       \/ MonitorMarkStale(l)
       \/ Replace(l)
       \/ HotRejoin(l)
       \/ Scrub(l)
       \/ CatchUp(l)
       \/ Admit(l)
       \/ LastResortServe(l)
       \/ MaintDrain(l)
       \/ RollStart(l)
       \/ RollFinish(l)
       \/ MaintClear(l)
       \/ SuppressExpire(l)
       \/ RoguePlanDrain(l)
       \/ ExpandLeg(l)
       \/ AgentFlag(l)
       \/ AgentClear(l)
       \/ AdmitAtStage(l)

\* Recovery actions are weakly fair.  WF(RaidDeconfigure) is P4;
\* WF(LegPerish) is the axiom that a blackhole eventually resolves
\* (perish or recover — recovery is the environment's choice and gets no
\* fairness), and WF(DeemDead) is the replace-after threshold: evidence
\* eventually reaches the record.  WF(Scrub) is the HotRejoinScrubbed
\* arm: a divergent standby is eventually demoted to a full rebuild
\* rather than parking forever.  Failures and writes are the
\* environment; HotRejoin is the orchestrator's choice.
FairnessCore ==
  /\ \A l \in Legs :
       /\ WF_vars(LegPerish(l))
       /\ WF_vars(DeemDead(l))
       /\ WF_vars(MonitorMarkStale(l))
       /\ WF_vars(Replace(l))
       \* 2026-07-29 audit: hot-rejoin has been a default-ON 60s
       \* timer-driven retrier since v1.19.0 (076985d) — "initiation is
       \* the orchestrator's choice" was the pre-default-ON world.  WF
       \* here is the honest abstraction, and it is what lets
       \* ExpansionCompletes drop its global \E-stale escape (a parked
       \* live stale leg is no longer a legitimate resting state).
       /\ WF_vars(HotRejoin(l))
       /\ WF_vars(Scrub(l))
       /\ WF_vars(CatchUp(l))
       /\ WF_vars(Admit(l))
       \* Maintenance machinery: once a node is IN maintenance the DS
       \* controller drives it (RollStart), a live roller clears its mark
       \* (MaintClear), and the lease TTL fires for a dead one
       \* (SuppressExpire).  Initiating each node's DRAIN stays unfair —
       \* campaign pacing is the operator's choice, so "the campaign
       \* completes" is deliberately NOT a theorem here;
       \* MaintenanceEventuallyLifts is.  WF(RollFinish) — kubelet
       \* completes restarts — lives in FairnessKubelet below so the
       \* wedged-restart run can drop exactly that assumption.
       /\ WF_vars(RollStart(l))
       /\ WF_vars(MaintClear(l))
       /\ WF_vars(SuppressExpire(l))
       /\ WF_vars(ExpandLeg(l))
       \* The bounce's RETURN path: NodeStage always runs its standby
       \* admission (it is not orchestrator-paced work — it is on the
       \* stage path itself), and a LIVE flagging agent eventually clears
       \* its own data-path flag once its raid is back.  Both are gated by
       \* constants FALSE in every legacy cfg, and WF of a permanently
       \* disabled action obligates nothing — the same treatment
       \* WF_vars(ExpandLeg(l)) already gets under ExpandEnabled = FALSE.
       /\ WF_vars(AdmitAtStage(l))
       /\ WF_vars(AgentClear(l))
  /\ WF_vars(Assemble)
  /\ WF_vars(EpochCut)
  /\ WF_vars(AcquireCatchup)
  /\ WF_vars(ReleaseCatchup)
  /\ WF_vars(AcquireAdmission)
  \* Acquire needs WarmWaiting TRUE, release needs it FALSE — the pair
  \* cannot ping-pong on unchanged state, so this WF is trap-safe.
  /\ WF_vars(ReleaseAdmission)
  \* WF(RaidGrow) is the SPDK resize-event chain: event-driven, no RPC.
  /\ WF_vars(RaidGrow)
  \* WF(PvGrow): the external resizer is a persistent retrier; once every
  \* leg grew, the CSI success + capacity patch eventually land.
  /\ WF_vars(PvGrow)
  \* WF(DeferClockExpire): wall clocks advance — a persisting deferral's
  \* deadline eventually passes (GateDeadline worlds only; the action is
  \* disabled elsewhere and WF of a disabled action obligates nothing).
  /\ WF_vars(DeferClockExpire)
  \* Pod-layer obligations (PodLayer worlds only; WF of a permanently
  \* disabled action obligates nothing).  Kubelet eventually unstages a
  \* gone pod's volume; the bouncer eventually issues its recreate; the
  \* reconciler is a 30s persistent retrier.  INITIATING a bounce stays
  \* deliberately unfair — that is the orchestrator's choice, exactly as
  \* MaintDrain's initiation is.
  /\ WF_vars(BounceUnstage)
  /\ WF_vars(BounceRecreate)
  /\ WF_vars(ReconcilerRecreate)

\* kubelet's obligation, split out (the P4-split pattern): the recreated
\* pod eventually comes back.  The wedged-DS-roll history (runak/runaj)
\* says this is NOT always true of the world — SpecWedgedKubelet drops
\* it and the strict run verifies a never-returning pod degrades ONE
\* leg's availability and nothing else (every invariant holds, the
\* volume stays writable on the survivor; the parked mark is the honest
\* operational state, so MaintenanceEventuallyLifts is deliberately NOT
\* checked there).
FairnessKubelet == \A l \in Legs : WF_vars(RollFinish(l))

\* WF on RaidDeconfigure IS P4: the data plane detects a dead/silent
\* member in bounded time (TCP_USER_TIMEOUT + command watchdog +
\* fast_io_fail) and faults it out.  It is split from FairnessCore so
\* the P4 mutation (SpecNoP4) can drop exactly this assumption.
Fairness ==
  /\ FairnessCore
  /\ FairnessKubelet
  /\ \A l \in Legs : WF_vars(RaidDeconfigure(l))

Spec == Init /\ [][Next]_vars /\ Fairness

\* The expansion tranche's STRONG-fairness upgrades, split out (the
\* FairnessKubelet pattern) because SF is expensive at 3-leg breadth and
\* only the expansion cfgs' per-leg progress theorem needs it.  Why SF
\* at all: the no-op claim cycle is real and cannot be work-gated away
\* (it is the F43 lasso's engine — the scheduler's 60s tick claims to
\* probe), so any contender needing claim = "none" (ExpandLeg: the
\* external resizer, a PERSISTENT whole-RPC retrier) and the claim-
\* holder's own dispatch work (CatchUp/Scrub — the work runs INSIDE the
\* hold, RAII; a schedule that cycles the claim without doing its work
\* models a dispatch the code cannot be) are only intermittently
\* enabled, which weak fairness never obligates.  SF is the tick-
\* granularity encoding of "the persistent retrier eventually wins a
\* free window" (probability-1 in reality) and "the holder performs its
\* dispatch".  Deliberately NOT extended to admission — ClaimArb, the
\* code's own arbitration, carries that.  The F56 wedge lasso survives
\* SF: its ExpandLeg is belt-disabled outright and its CatchUp is a
\* content no-op (SF of a never-enabled / never-state-changing action
\* obligates nothing).
FairnessExpand ==
  \A l \in Legs :
    /\ SF_vars(Scrub(l))
    /\ SF_vars(CatchUp(l))
    /\ SF_vars(ExpandLeg(l))

SpecExpand == Init /\ [][Next]_vars /\ Fairness /\ FairnessExpand

\* The pre-P4 world: nothing bounds detection.  A blackholed serving leg
\* may sit in the raid forever, stalling every write — the 150-177s
\* ledger stalls, unbounded.  FlintReplicationP4.cfg checks
\* EventuallyWritable against THIS spec and must find the stall lasso.
SpecNoP4 == Init /\ [][Next]_vars /\ FairnessCore /\ FairnessKubelet

\* The wedged-restart world: the DS pod deleted for the roll never comes
\* back (image pull failure, crashloop — the runak/runaj wedge family).
\* Detection fairness stays (P4 is real); only kubelet's completion
\* obligation is dropped.
SpecWedgedKubelet ==
  Init /\ [][Next]_vars /\ FairnessCore
       /\ \A l \in Legs : WF_vars(RaidDeconfigure(l))

(***************************************************************************)
(* Invariants                                                              *)
(***************************************************************************)

\* THE durability invariant (F36c / PacificA commit invariant): a serving
\* assembly holds every acked write on every configured leg — or the risk
\* was explicitly surfaced.  Per-leg because raid1 serves reads from ANY
\* leg: one stale serving leg is a stale-read surface.
\* 2026-07-29: a FORCED-STALE member (StaleFloor; state stays "stale") is
\* exempt PER-LEG — it is the knowingly-behind, evented divergence hazard
\* — while every in-sync member's obligation stays live even in a mixed
\* assembly (a per-leg exemption, deliberately not a global escape).
Inv_NoSilentLoss ==
  (serving # {} /\ ~riskSurfaced) =>
    \A l \in serving : state[l] = "stale" \/ acked \subseteq legData[l]

\* The record never lies about sync for a leg the raid is serving.
Inv_InsyncServingIsCurrent ==
  ~riskSurfaced =>
    \A l \in serving : state[l] = "insync" => acked \subseteq legData[l]

\* Serving legs are all of the current incarnation (sb sanity).
Inv_ServingCurrentGen ==
  \A l \in serving : legGen[l] = raidGen

\* No serving leg holds a block outside the served lineage (rejoin /
\* split-brain divergence): raid1 serves reads from ANY leg, so a phantom
\* block from a dead lineage or a zombie head is a split-read surface.
\* This is what the RejoinGuard ancestry check and the F48 sever protect.
Inv_NoDivergentServing ==
  ~riskSurfaced =>
    \A l \in serving : state[l] = "stale" \/ legData[l] \subseteq lineage

\* Evidence soundness (the EvidenceStrict world): the record never deems
\* a recoverable node dead.  With fallible evidence this fails trivially;
\* the INTERESTING consequence is falseRisk below.
Inv_EvidenceSound ==
  \A l \in deemedDead : legUp[l] = "dead"

\* The resurrection theorem: a surfaced risk is never HOLLOW — every
\* writer a ServeWithRisk assembly excused was truly dead, so the
\* "unrecoverable" claim behind riskSurfaced is real.  The Resurrect
\* mutation (EvidenceStrict = FALSE) must violate this: a blackholed
\* writer holding the acked tail is deemed dead, excused, and later
\* recovers — the tail was there all along.
\* 2026-07-29 audit: this is a theorem ONLY of the GateDeadline = FALSE
\* idealization.  The shipped gate's deadline arm ("Never hang") excuses
\* transient — possibly recoverable — writers after 180s by DESIGN, so
\* with GateDeadline = TRUE this invariant is FALSE of both model and
\* code: the GateRealHollow run must find the violation, and the
\* GateReal strict run checks InvCore (everything else) instead.
Inv_NoFalseRisk == ~falseRisk

\* Canary, StaleFloor teeth (GateRealStale must violate it): the shipped
\* NodeStage really can serve a knowingly-stale leg with no operator and
\* — when the gate read Proceed — no risk marker (only the
\* StaleReplicaAdmitted event).  Its violation trace is the reachability
\* proof for every StaleServed exemption above.
Inv_NoStaleServe == ~StaleServed

\* THE MAINTENANCE THEOREM: with zero REAL failures, a rolling restart
\* alone never takes the volume down.  Rolls cost no crash budget, so
\* crashes = 0 isolates the pure-maintenance world; the only way to
\* serving = {} there is the roll itself.  Both roll mutations must
\* violate this — by different paths: MaintFence = FALSE finds today's
\* landmine (roll a serving leg's tgt → P4 faults it → roll the next
\* node before readmission → the last leg deconfigures); MaintBarrier =
\* FALSE finds the subtler half (drain exists, but the next drain
\* proceeds on pod-readiness while the previous leg is still stale —
\* the last serving leg is drained away).
Inv_PlannedRollNeverCausesOutage ==
  crashes = 0 => serving # {}

\* The fence, as an invariant: a serving leg's tgt is never down for a
\* planned restart.  Under MaintFence the suppression mark gates
\* RollStart, a suppressed leg is out of serving (drained) and cannot
\* re-enter (HotRejoin/Admit/Assemble/LastResortServe all refuse), so
\* the intersection stays empty — checked in the strict maintenance run.
Inv_MaintFenceHolds ==
  MaintFence => serving \cap rolling = {}

\* THE BARRIER'S NECESSITY, restated sharply.  The unconditional
\* last-serving-member belt (in MaintDrain) already prevents the direct
\* drain-to-outage, so at 2 legs a missing barrier only stalls the
\* campaign.  What the barrier uniquely prevents shows at >= 3 legs:
\* without it the roll ERODES redundancy — the previous node's leg is
\* still stale (pod Ready, leg un-readmitted) when the next drain fires,
\* and with zero real failures the volume walks down to a single serving
\* leg, one failure away from outage.  With the barrier, planned
\* maintenance never has more than ONE leg out of service.  The
\* RollBarrier mutation must violate this at 3 legs.
Inv_PlannedRollBoundedImpact ==
  crashes = 0 => Cardinality(Legs \ serving) <= 1

\* THE SIZE-SAFETY THEOREM (F43 item #8's contract): once the
\* consumer-visible device grew, no serving leg is ever shorter than it —
\* a short leg under a grown device is a silent device shrink (reads
\* beyond the short leg's end, resize2fs's world ripped out from under
\* the fs).  The ExpandGuard mutation (SizeGuard = FALSE) must violate
\* this: admit the returning pre-expand leg straight into the grown
\* assembly.
Inv_NoDeviceShrink ==
  raidSize = "new" => \A l \in serving : legSize[l] = "new"

(***************************************************************************)
(* THE BOUNCE-SAFETY THEOREM.  A bounce is never the REASON an acked tail  *)
(* had to be excused.  bounceRisk is stamped when the gate's ServeWithRisk *)
(* arm fires to come back from a window the CONTROLLER opened on a volume  *)
(* whose recorded writer set was ALREADY not whole.  A writer lost DURING  *)
(* the window is an ordinary failure no preflight could predict and does   *)
(* not stamp it; a writer already gone at commit time is the manufactured  *)
(* outage — and the shipped planner, which reads no leg health at all,     *)
(* cannot see it.  Fixed by BouncePreflight: BounceRisk must FIND this     *)
(* with the belt off, BounceRaceFixed must HOLD it with the belt on and    *)
(* NO leadership whatsoever.                                              *)
(***************************************************************************)
Inv_NoBounceInducedRisk == ~bounceRisk


(***************************************************************************)
(* THE ADMIT-BEFORE-GATE THEOREM.  admit_standbys_at_stage commits         *)
(* mark_in_sync — writer-set GROWTH — before the freshness gate rules, so  *)
(* a Defer can leave a leg recorded in_sync and in the writer set for an   *)
(* assembly that never happened, and the NEXT gate will wait on it.  The   *)
(* audit's verifier argued this is the SAFE direction (the admission is    *)
(* real: fenced, chased through the cut, size-checked before the in_sync   *)
(* write).  This MACHINE-CHECKS that rebuttal instead of trusting it: a    *)
(* writer-set member either has served in its current incarnation or       *)
(* holds the acked tail that entitled it.  The everServed disjunct is      *)
(* required — between a leg's deconfigure and its stale-mark the ordinary  *)
(* monitor lag leaves a served leg in writerSet without the tail, which is *)
(* not this residue.                                                      *)
(***************************************************************************)
Inv_WriterSetGrounded ==
  (~riskSurfaced /\ zombie = {}) =>
    \A l \in writerSet : l \in everServed \/ acked \subseteq legData[l]

(***************************************************************************)
(* THE POINTLESS-REBOUNCE CANARY.  A volume never eats two bounces in a    *)
(* row without one of them accomplishing something.  THIS IS FALSE OF THE  *)
(* SHIPPED DESIGN and is stated as a canary, not a theorem — the same      *)
(* instrument as Inv_NoStaleServe: its violation trace is the              *)
(* reachability proof and the fix is owed in CODE.  Three separately-      *)
(* sufficient shipped mechanisms violate it: (1) the Err arm emits         *)
(* CutoverFailed and records NO attempt (cutover.rs:1058-1067), so the     *)
(* documented 900s minimum between attempts is never applied on any error  *)
(* path — including the 409 the liveness reconciler causes by recreating   *)
(* the server INSIDE the detach wait; (2) the CutoverIneffective verdict   *)
(* removes the attempt and declares the volume eligible again with no      *)
(* counter, no backoff and no negative caching anywhere in the file; (3)   *)
(* the data-path arm's verification predicate is a flag only the flagging  *)
(* node's agent may clear, so a dead flagger makes it permanently          *)
(* unsatisfiable.  Checked ONLY in the BounceLoop run.                     *)
(***************************************************************************)
Inv_NoPointlessRebounce == consecutiveBounces <= 1

(***************************************************************************)
(* THE TWO-PLANNER DISJOINTNESS THEOREM.  plan_cutover applies NEITHER of  *)
(* plan_hot_rejoin's admission filters (maintenance suppression, hot-      *)
(* rejoin marker), so it can commit a full teardown of a healthy data path *)
(* whose only purpose — admitting a particular standby — the stage         *)
(* admission is guaranteed to refuse.  Stated as its own ghost rather than *)
(* on the shared churn canary DELIBERATELY: an A/B showed the canary is    *)
(* violated with the filter ON as well as OFF (three other doors reach it, *)
(* per BounceLoop), so it cannot test this fix.  This one is violable      *)
(* only through this door.                                                 *)
(***************************************************************************)
Inv_NoDoomedBounce == ~bounceDoomed

(***************************************************************************)
(* THE DOUBLE-CREATOR THEOREM.  A bounce is never silently defeated: the   *)
(* server pod never comes back while the volume it was supposed to force a *)
(* restage of is STILL STAGED.  The state this forbids is exactly the      *)
(* §6 same-node reuse the detach wait exists to prevent — pod present, an  *)
(* open bounce window, and serving never went to {} — after which kubelet  *)
(* reuses the staged volume, no NodeStage runs, no reassembly happens, the *)
(* standby stays parked, and clients ate an NFSv4 grace-window recovery    *)
(* for nothing.  Needs NO crash budget: it is a pure orchestration race    *)
(* between two tokio tasks in one process holding one lease, with no       *)
(* mutual exclusion between them.                                          *)
(***************************************************************************)
Inv_BounceNotSilentlyDefeated ==
  ~(bounceWindow # "none" /\ podUp /\ serving # {})

\* InvCore is everything except the evidence-purity theorem; Inv (every
\* legacy cfg) adds Inv_NoFalseRisk, which only the GateDeadline = FALSE
\* idealization satisfies.  The GateReal strict run checks InvCore.
InvCore == TypeOK /\ Inv_NoSilentLoss /\ Inv_InsyncServingIsCurrent
                  /\ Inv_ServingCurrentGen /\ Inv_NoDivergentServing
                  /\ Inv_EvidenceSound

Inv == InvCore /\ Inv_NoFalseRisk

(***************************************************************************)
(* Liveness: availability after the storm.  Once the failure budget is    *)
(* exhausted, the system converges to a serving assembly with the acked   *)
(* content intact (or the risk surfaced) — and stays there.  NOTE: this   *)
(* property is CONTENT-shaped and provably blind to the P4 stall (a       *)
(* blackholed serving member keeps it content-good while every write      *)
(* hangs) — TLC verifies it holds even under SpecNoP4.  The write-        *)
(* availability claim lives in EventuallyWritable below, where it is      *)
(* enforceable.  Remove WF(Scrub) and a divergent rejoiner parks as a     *)
(* standby forever instead of demoting to a full rebuild.                 *)
(*                                                                        *)
(* Deferred is the one legitimate unavailability: NodeStage's Defer arm.  *)
(* With no up in-sync leg there is no assembly material — every survivor  *)
(* is dead, blackholed, stale, or a parked standby — and the design       *)
(* SACRIFICES availability rather than serve stale data (the F36c        *)
(* choice; the stale-only-survivor last resort is a manual runbook, not   *)
(* an automatic action).                                                  *)
(***************************************************************************)
GoodServing ==
  \/ riskSurfaced
  \* 2026-07-29: a forced-stale (StaleFloor) assembly is its own honest
  \* post-storm category — the code has NO in-place heal for a serving
  \* stale member ("incremental rebuild lands in phase 3/4"; catch-up
  \* defers with ReplicaHeadInUse), so a mixed assembly persists until
  \* something restages.  It is EVENTED (StaleReplicaAdmitted), like
  \* riskSurfaced is annotated — the disjunct states the shipped
  \* envelope, not a wish.  Unreachable in every StaleFloor=FALSE cfg.
  \/ StaleServed
  \/ /\ serving # {}
     /\ \A l \in serving : acked \subseteq legData[l]

Deferred == serving = {} /\ UpInSync = {}

EventuallyServingAgain ==
  <>[](crashes < MaxCrashes \/ GoodServing \/ Deferred)

(***************************************************************************)
(* The P4 theorem — availability of WRITES, which GoodServing does NOT    *)
(* imply: a raid holding a blackholed member is content-good (every leg   *)
(* has the acked data) yet every write stalls, because a sync mirror      *)
(* needs all members responsive.  Gating this claim exposed that the      *)
(* earlier prose ("remove the P4 fairness and the liveness fails") was    *)
(* not true of EventuallyServingAgain at all — the stall is invisible to  *)
(* a content-shaped property.  EventuallyWritable is the property P4      *)
(* actually guarantees: after the storm, the serving assembly is all-up   *)
(* (writes flow) or the volume is honestly Deferred.  Remove WF on        *)
(* RaidDeconfigure (SpecNoP4) and TLC finds the stall lasso.              *)
(***************************************************************************)
GoodWritable == serving # {} /\ \A l \in serving : legUp[l] = "up"

EventuallyWritable ==
  <>[](crashes < MaxCrashes \/ GoodWritable \/ Deferred)

(***************************************************************************)
(* The F43 theorem: no warm standby waits forever.  Every wait resolves — *)
(* by admission (the arbitrated claim eventually reaches the window) or   *)
(* by the world changing (the standby or its source died; a new epoch     *)
(* cut de-warmed it — each makes WarmWaiting false, honestly).  With      *)
(* ClaimArb = FALSE this FAILS: the starvation lasso is catch-up's        *)
(* renewal beating the admission claim forever — F43's parked standby,    *)
(* rediscovered as a temporal counterexample.                             *)
(***************************************************************************)
AdmissionNotStarved == [](WarmWaiting => <>(~WarmWaiting))

(***************************************************************************)
(* The maintenance-lease theorem: a suppression mark on a leg that can    *)
(* still serve eventually lifts.  A live roller clears it (MaintClear,    *)
(* fair); a dead roller's lease expires (SuppressExpire, fair,            *)
(* MaintLease); the restart it waits on completes (RollFinish, fair,      *)
(* roller-independent); a Replace moves the identity to a fresh node and  *)
(* clears it.  The escape is the leg's own VERIFIED death: the first      *)
(* deep run of this tranche found the honest counterexample to the       *)
(* unconditional form — drain a leg, then the leg's node dies AND every   *)
(* rebuild source dies too (spot reclaim mid-maintenance); no restart     *)
(* can complete and no Replace has a source, so the mark stays.  A mark   *)
(* on a truly dead leg is INERT — every action it gates already requires  *)
(* responsiveness — so the per-leg, death-escaped statement is the        *)
(* design truth, not a weakening of it.  With MaintLease = FALSE the      *)
(* roll-lease mutation must still find the lasso: the roller dies after   *)
(* the drain, the leg stays LIVE, and nothing lifts the mark — the F43    *)
(* parked standby re-created by an unleased maintenance flag.             *)
(***************************************************************************)
MaintenanceEventuallyLifts ==
  \A l \in Legs :
    []((l \in suppress /\ legUp[l] = "up")
        => <>(l \notin suppress \/ legUp[l] = "dead"))

(***************************************************************************)
(* THE PARKING THEOREM (2026-07-29 audit).  A warm, responsive standby    *)
(* with NO mark of its own and a live serving source eventually admits    *)
(* or the world honestly changes under it.  With SuppressScoped = TRUE   *)
(* (per-leg marks — the design semantics) this HOLDS.  With the shipped  *)
(* volume-wide widening (SuppressScoped = FALSE) the MaintPark run must  *)
(* FIND the lasso at 3 legs under a wedged roll: one node's pod never    *)
(* returns, the live roller renews that node's mark forever (900s TTL vs *)
(* 60s tick, no wedge timeout), plan_hot_rejoin parks the WHOLE volume's *)
(* admission planning — and a warm standby on an UNAFFECTED node waits   *)
(* at reduced redundancy indefinitely: the F43 parked standby through a  *)
(* third door.                                                            *)
(***************************************************************************)
WarmLeg(l) ==
  /\ state[l] = "standby"
  /\ Responsive(l)
  /\ l \notin suppress
  /\ epochCut \subseteq legData[l]
  /\ (SizeGuard => (SizeHeal \/ raidSize = "old" \/ legSize[l] = "new"))
  /\ \E src \in serving :
       /\ Responsive(src)
       /\ (RejoinGuard => legData[l] \subseteq legData[src])
       /\ (SizeGuard => (SizeHeal \/ legSize[src] = "old" \/ legSize[l] = "new"))

StandbyAdmissionNotParked ==
  \A l \in Legs : [](WarmLeg(l) => <>(state[l] = "insync" \/ ~WarmLeg(l)))

(***************************************************************************)
(* THE F56 THEOREM: an outstanding expansion reaches every leg.  Per-leg  *)
(* and escape-hatched like the lifts property: a leg may instead die      *)
(* (verified); or SOME leg may park stale — hot-rejoin initiation is the  *)
(* orchestrator's choice (unfair, like the drain), and a parked stale     *)
(* leg legitimately holds the C2 belt for everyone; or the volume may     *)
(* honestly Defer (no assembly material).  The wedge does NONE of these:  *)
(* every leg is alive, none is stale — the returning leg is a live,       *)
(* chasing STANDBY, content-warm and size-old, whose admission the size   *)
(* guard defers every tick while the C2 belt blocks the very fan-out      *)
(* that would grow it and the retention pin holds the source-sized full   *)
(* build shut.  With SizeHeal = FALSE (the shipped pre-fix code) the      *)
(* ExpandWedge mutation must find exactly that lasso; with the fix the    *)
(* property holds.                                                        *)
(***************************************************************************)
ExpansionCompletes ==
  \A l \in Legs :
    [](wantNew => <>(\/ legSize[l] = "new"
                     \/ legUp[l] = "dead"
                     \* 2026-07-29 audit: this escape was `\E m : state[m]
                     \* = "stale"` — globally discharging EVERY leg's
                     \* obligation whenever ANY leg was stale, on the
                     \* stated ground that "hot-rejoin initiation is the
                     \* orchestrator's choice (unfair)".  False of the
                     \* shipped system (default-ON 60s retrier since
                     \* v1.19.0), and it made every stale-parked livelock
                     \* invisible — only standby-parked wedges (the F56
                     \* shape) were detectable.  With WF(HotRejoin) the
                     \* escape narrows to a stale leg that CANNOT rejoin:
                     \* unresponsive (blackholed mid-resolution), or a
                     \* forced-stale member pinned serving (StaleFloor).
                     \/ \E m \in Legs : state[m] = "stale"
                                        /\ (~Responsive(m) \/ m \in serving)
                     \* CANDIDATE F57, surfaced by this property's first
                     \* strict run and confirmed against the code: a
                     \* STANDBY whose node dies parks forever — the only
                     \* standby->stale demotion is chase-source
                     \* exhaustion (ReplicaChaseSourcesExhausted), the
                     \* raid-health monitor marks only raid MEMBERS, and
                     \* replica_replace filters on Stale — so nothing
                     \* demotes or replaces a dead mid-rebuild standby
                     \* and the volume sits at reduced redundancy until
                     \* an operator intervenes.  Escaped HONESTLY here
                     \* (the model models the implementation); the fix
                     \* is a code change, not a model change.
                     \/ \E m \in Legs : state[m] = "standby" /\ legUp[m] # "up"
                     \/ Deferred))

\* State-space bound for TLC (raidGen grows with deconfigures/assemblies,
\* bounded by the crash budget plus the roll campaign — each node rolls
\* at most once, and a roll adds at most a drain bump, a deconfigure
\* bump (unfenced), and a reassembly bump).
\* One extra assembly per bounce (the return path); MaxBounces = 0 in every
\* legacy cfg leaves this numerically identical.
GenBound == raidGen <= (3 * MaxCrashes) + (3 * Cardinality(Legs)) + 3
                       + (2 * MaxBounces)

\* The bounce runs need failure BREADTH — the manufactured-outage window
\* requires TWO independent failures, one to eject a leg from the raid
\* (which is what creates the bounce trigger in the first place) and one to
\* take a surviving writer out during the window — but they do NOT need
\* deep raid-incarnation churn on top of it.  GenBound's generic budget at
\* MaxCrashes = 2 makes the strict runs explore tens of millions of states
\* for behaviors that are all tail-churn; every bounce counterexample here
\* lands within a handful of incarnations.  STATED AS THE TRADE IT IS: the
\* bounce strict runs are theorems about the model UNDER this bound, and
\* the two mutation runs are re-verified to still find their counter-
\* examples with it applied.
BounceBound == raidGen <= (2 * MaxCrashes) + Cardinality(Legs) + 2

================================================================================
