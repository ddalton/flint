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
  \* F61 (found LIVE on runao 2026-07-30, drill 3.14's first run; this module
  \* could not express it).  LocalLegs are legs whose drain is a NO-OP: the
  \* serving raid lives on the leg's own node (consumer == node, the "local
  \* half"), so maint_roll.rs emits MaintenanceLocalConsumer and SKIPS the
  \* drain.  Unattached volumes and nodes with no legs behave identically.
  \* Before this constant every element of Legs was drainable BY
  \* CONSTRUCTION, so no state existed where a node is pending-and-
  \* undrainable — which is exactly the state the shipped planner wedges in.
  LocalLegs,
  \* TRUE = the F61 FIX: RollStart is gated on the drain PASS having
  \* completed (`processed`), not on a mark having been minted
  \* (`suppress`).  FALSE reproduces the shipped predicate, where a node
  \* that legitimately marks nothing can never be rolled — the livelock.
  MaintProcessedGate,
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
  \* ---- the raid-lifetime tranche (F62, found LIVE on runao 2026-07-30 in
  \* the very roll the F61 fix enabled; docs/f62-local-half-outage-and-blind-
  \* barrier.md).  Ordered by dependency: the ARM makes the composition a
  \* real object, the REFUSAL is the orchestration fix, the two REPAIR arms
  \* are rival answers to "and what puts it back?" ------------------------
  \* ---- consumer mobility (2026-07-29, forced by the runap live gate) ----
  \* The F62 tranche treated LocalLegs as a CONSTANT: a leg's consumer never
  \* moved, so "this node hosts the composition" was a fact of the world
  \* rather than a fact of the moment.  That was defensible for the SAFETY
  \* question (does the roll destroy the volume?) and wrong for the
  \* OPERATIONAL one, which the live gate raised immediately: a refused node
  \* sits on an old revision until something changes, and what changes is the
  \* CONSUMER MOVING.  Measured on runap: 14s after the NFS server left the
  \* refused node, the roller rolled it unprompted — because the shipped
  \* predicate is recomputed every tick from live state.  Nothing in this
  \* module demanded that, and RollProcessedNodeRolls actively permitted the
  \* opposite by treating maintSkipped as a TERMINAL outcome.  So the model
  \* was WEAKER than the code here, which is the direction that lets a
  \* regression land unnoticed.
  RefusalSticky, \* TRUE = the BUG: fix B's eligibility gate reads the
               \* remembered `maintSkipped` set instead of the live
               \* condition, so a refusal is permanent even after the
               \* consumer leaves.  FALSE = shipped (maint_roll.rs rebuilds
               \* local_consumer_nodes from the gather every tick).  Exists
               \* so Inv_RefusalNeverClears has a bug side; without one, a
               \* green fix side proves nothing.
  ConsumerMobile, \* TRUE = the consumer can relocate (localLegs is live
               \* state and RelocateConsumer exists).  FALSE pins localLegs
               \* to the LocalLegs constant forever, so every pre-mobility
               \* run keeps its exact behavior graph — the RaidLifetimeArm
               \* discipline again.
  RaidLifetimeArm, \* TRUE = raidHost/staged/raidSeen are live state and the
               \* composition has its own lifetime.  FALSE pins them inert,
               \* so every pre-F62 cfg keeps its exact behavior graph (and
               \* its gate cost) — the same discipline ExpandEnabled and
               \* DataPathArm used when they landed.
  MaintLocalRefuse, \* TRUE = the F62 fix: the roller REFUSES a local-consumer
               \* node, records it in maintSkipped for the operator, and
               \* keeps converging every other node.  FALSE = the shipped
               \* post-F61 behavior: roll it, which destroys the serving
               \* composition with ZERO real failures.  Note the polarity
               \* of this pair — F61's fix WITHOUT this one is strictly
               \* worse than F61's bug, because the livelock was the only
               \* thing preventing the outage.  TLC must show exactly that.
  RaidReconcileArm, \* Repair A2: the node agent re-creates the composition
               \* for volumes the VA says are attached to its node when its
               \* tgt comes back (the v1.10.0 note's option 1).  Does NOT
               \* need a superblock — flint passes "superblock": false ON
               \* PURPOSE (driver.rs 3159; the §3 phantom-assembly class, and
               \* the 1 MiB payload shift that silently formatted restored
               \* snapshots on 2026-06-12).
               \*
               \* THROUGH THE F62 TRANCHE THIS ARM WAS A LIE, and the honest
               \* record of it belongs here.  It did one thing: relax
               \* Assemble's `~staged` guard.  That answered "would a repair
               \* of this SHAPE restore the volume?" — yes — and was then
               \* cited as "A2 is modelled green", which it never was.  A
               \* relaxed guard on the existing creator cannot exhibit A2's
               \* actual hazard, because the hazard is a SECOND creator, and
               \* the state it produces was unrepresentable while raidHost
               \* was a scalar.  The arm now drives AgentBootReconcile, a
               \* real action with its own guard and its own inputs.
  VaCanLag,    \* TRUE = the attached VolumeAttachment may still name the OLD
               \* host after the consumer has moved, closed later by
               \* VaCatchUp.  This is not a pessimisation invented for the
               \* model: node_agent.rs:3219 documents the ublk reaper's
               \* reason for existing as "the local disk a STALE VA made us
               \* rebuild after the consumer moved away".  FALSE = an
               \* instantaneous attacher, which is the world where A2 looks
               \* safe and is therefore the wrong world to gate it in.
  NodeStageValidatesBases, \* TRUE = the candidate fix to ensure_raid1_bdev
               \* (driver.rs:3105): when NodeStage finds a raid of this name
               \* already ONLINE it VALIDATES the base set before reusing it,
               \* deleting and re-creating on a mismatch.  FALSE = shipped,
               \* which reuses unconditionally — "already ONLINE (N base(s)
               \* configured) — reusing", where the count it reads reaches the
               \* log line and nothing else.
               \*
               \* Harmless TODAY: the only creator is NodeStage, so an online
               \* raid of that name means a previous NodeStage finished and its
               \* base set came from the same PV replica record.  It becomes a
               \* hazard the moment A2 adds a SECOND creator whose base set was
               \* chosen at a different time.  That is why this arm belongs to
               \* the A2 tranche and not to a bug report.
  StrikeRepairArm, \* TRUE = the SHIPPED periodic in-place repair exists:
               \* detect_lost_data_paths counts consecutive
               \* attached-here-but-no-raid observations and, at its threshold,
               \* calls repair_data_path (node_agent.rs).
               \*
               \* Added 2026-07-30 to correct an OVERSTATEMENT in this
               \* tranche.  FlintReplicationUncontrolledBlind.cfg was green —
               \* "no interleaving recovers the volume" — and was read as "the
               \* shipped code cannot recover an uncontrolled tgt death".  That
               \* is wrong: the model had only A1's trigger, which needs
               \* data_path_raid_seen (the COLLAPSE-EVENT path,
               \* raid_collapse_verdict's `previously_seen`), and the layer-2
               \* repair needs no seeded state at all — its gate is
               \* strikes >= threshold on live observations.
               \*
               \* Note what this action already carries: repair_data_path
               \* refuses unless `is_staged_here(volume_handle)` — kubelet's own
               \* staging directory. That is EXACTLY the belt this tranche
               \* derived for A2 from first principles, already shipped, on the
               \* adjacent repair. So the shipped repair is already the SAFE
               \* shape, and A2 differs from it only in its TRIGGER.
               \*
               \* The strike COUNT is abstracted away deliberately: it is a
               \* debounce against an in-flight NodeStage, not a safety guard,
               \* and every question asked here is about reachability or safety
               \* rather than latency.
  A2LocalStagingBelt, \* TRUE = A2 assembles only where kubelet still
               \* believes the volume staged (stagedAt = vaNode).  The
               \* candidate that discriminates a class-3 death (staged left
               \* alone) from a relocation (staged cleared) using LOCAL
               \* ground truth alone.  FALSE = A2 trusts the VA by itself.
  A2SoleOwnershipBelt, \* TRUE = A2 refuses to assemble while ANY other host
               \* holds a composition over these lvols — a cluster-wide
               \* probe, not a local record.  Implementable with machinery
               \* that already shipped: fix C's `bdev_raid_get_bdevs` on
               \* another node (maint_roll.rs gather_volume_maint) is exactly
               \* this call.  FALSE = the naive A2, which trusts its one
               \* local predicate.
  UncontrolledTgtDeath, \* TRUE = TgtDie(l) exists: a csi-node tgt dies with
               \* node and consumer in place, with NO roller involved.
               \* THE DEFAULT PATH, and the model could not express it until
               \* now — class-3 destruction lived only inside RollStart, an
               \* action gated on the roller's own arms.  So every F62/F63
               \* run has been asking "can flint's roller cause this?" when
               \* the operative question is "can a plain helm upgrade?"
               \* It can: maintenance.drainRoll.enabled defaults FALSE, the
               \* chart only sets updateStrategy: OnDelete inside that
               \* conditional (node.yaml:13-24), so the DaemonSet takes
               \* k8s's RollingUpdate default and rolls every node pod on a
               \* template change — and plan_roll stands down anyway when
               \* !on_delete (maint_roll.rs:248).  OOM kills, kubelet
               \* restarts, evictions and node-image upgrades land here too.
               \* Fixes B and B' cannot reach ANY of it; they govern only a
               \* roller that is off by default and refuses to act in this
               \* configuration.
  DpSeenRehydrate, \* Repair A1: data_path_raid_seen is rehydrated from the
               \* STAGED-volume set when the agent starts.  Note it must be
               \* the staged set and NOT live SPDK: seeding from SPDK reads
               \* the raid list, which is empty in precisely the situation
               \* that matters, so that seed would be a no-op exactly when
               \* needed.  FALSE = shipped, where the set is a fresh empty
               \* HashSet and CollapseEvent::Lost's `seen.contains` can
               \* never hold for a raid that died with the previous process.
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
               \* chert.us/f36c-defer PV annotation): once a deferral's
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
               \* or the RWO disk.chert.us/rejoin-bounce opt-in.
               \* Behind its own flag so each theorem names its world.
  DataPathArm, \* TRUE = plan_cutover's data_path_lost arm (312-334) and
               \* the annotation machinery exist.  It bypasses the standby
               \* AND lag gates entirely ("nothing to admit, only a data
               \* path to rebuild"), and its verification predicate is a
               \* flag ONLY the flagging node's agent may ever clear.
  BouncePreflight, \* THE BELT — SHIPPED 2026-07-29 (the DrainBelt
               \* analogue), `bounce_preflight` in cutover.rs, evaluated at
               \* the top of execute_cutover so it re-reads node evidence as
               \* late as possible.  FALSE is now the PRE-FIX world, kept as
               \* the BounceRisk/BounceRace regression mutations.
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
  ReconcilerBelt, \* TRUE = SHIPPED 2026-07-29: the bouncer takes a
               \* TIME-BOUNDED recreate claim (the bounce-in-flight PV
               \* annotation, carrying an EXPIRY sized from the configured
               \* detach timeout — a fixed TTL would expire mid-wait once an
               \* operator raised that timeout) and nfs_reconcile_decision
               \* honours it.  Boundedness is reader-enforced, which is the
               \* property THIS MODEL CANNOT CHECK: WF(BounceRecreate)
               \* assumes the bouncer completes, so a bouncer dying under
               \* its own belt is unrepresentable here and is covered by
               \* unit tests instead.
               \* FALSE = the PRE-FIX world, kept as the BouncePod
               \* regression mutation: rwx_nfs.rs's liveness reconciler
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
  DetachWaitHonored, \* TRUE = SHIPPED 2026-07-29: the bouncer
               \* recreates only after the unstage it waited for, and on
               \* timeout HANDS OFF to the reconciler (releasing the claim)
               \* instead of recreating into a still-staged volume.
               \* FALSE = the pre-fix timeout path, kept as the
               \* BounceTimeout regression mutation: await_detached returning false only WARNS and
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
  WriterLimbo, \* TRUE = a leg may go unresponsive WITHOUT spending crash
               \* budget, so it can oscillate unavailable/available
               \* indefinitely.  This models the one world the belt's liveness
               \* bug needs and a budget forbids: a flapping kubelet (OOM
               \* loop, flaky network) keeps resetting
               \* Ready.lastTransitionTime so the node never crosses
               \* node_gone_secs, and its Node object is never deleted so
               \* NodeGone never fires either — a writer neither answering nor
               \* verifiably gone, forever.  A flapping kubelet is not a
               \* data-loss event, which is why it should not consume a
               \* failure budget.  UNDER A BUDGET THIS IS UNREACHABLE: one
               \* blackhole either recovers (safe) or perishes into deemedDead
               \* (safe), both under weak fairness — the structural reason this
               \* module could not see the unbounded belt until a CODE review
               \* found it.  FALSE in every other cfg.
  RefusalBounded, \* TRUE = SHIPPED 2026-07-29: a standing preflight refusal
               \* is BOUNDED — after the gate's own defer bound the bounce
               \* proceeds anyway and says so (CutoverPreflightOverridden).
               \* FALSE = an unbounded belt, which is a LIVENESS BUG this
               \* module could not previously express: BouncePreflight is a
               \* GUARD, so a blocked bounce is merely a disabled action, and
               \* nothing asked whether the lost data path is ever repaired.
               \* The 2026-07-29 code review found it in the code before the
               \* model could; these runs close that gap so the next belt's
               \* liveness is machine-checked rather than argued.
  StageAdmit,  \* TRUE = model admit_standbys_at_stage (driver.rs:1967 →
               \* catchup.rs:2301) as its own action.  Required for the
               \* bounce's RETURN path: Admit cannot represent it — Admit
               \* demands claim = "admission" and serving # {}, while the
               \* at-stage admission runs in the NODE process, under NO
               \* volume claim, with the raid not yet created, and commits
               \* record_in_sync (writer-set GROWTH) BEFORE the freshness
               \* gate rules (driver.rs:2089).  The code's order is
               \* admit→gate; this module's has been gate→admit.

  (*************************************************************************)
  (* THE KUBE-DS TRANCHE (2026-07-30).  The k8s DaemonSet controller as a   *)
  (* THIRD actor, on its own pod-lifecycle axis, independent of the roller. *)
  (*                                                                       *)
  (* WHY IT HAS TO EXIST: MaintBarrier is conjoined ONLY into drain-side    *)
  (* actions (MaintDrain :1979, MaintDrainSkip :2437, RoguePlanDrain :2665).*)
  (* RollStart has NO redundancy gate of any kind and RollFinish is         *)
  (* unconditional, so RollUnfenced.cfg is green with respect to a          *)
  (* kubelet-side barrier it cannot express.  And the roller governs only   *)
  (* OnDelete, which is OFF by default — the SHIPPED DaemonSet takes k8s    *)
  (* RollingUpdate, which no flint code can refuse.                        *)
  (*                                                                       *)
  (* MEASURED LIVE 2026-07-30 (cluster runaq, k8s v1.34.10, 4 nodes,        *)
  (* maxUnavailable=1) — these constants exist to make each observation     *)
  (* a checkable claim rather than a story:                                 *)
  (*   * all pods Ready, template bump  => max 1 unavailable NODE. Readiness*)
  (*     DOES pace a DaemonSet roll.                                        *)
  (*   * all pods NOT-Ready, bump       => ALL FOUR deleted in the SAME     *)
  (*     SECOND.  update.go rollingUpdate() appends every unavailable       *)
  (*     old-revision pod to allowedReplacementPods, which is NEVER clipped *)
  (*     by the budget; only candidatePodsToDelete is.                      *)
  (*   * delete one pod with 3 peers unavailable => replacement in ~5s.     *)
  (*     Creation has ZERO availability accounting.                        *)
  (*   * minReadySeconds=30, four HEALTHY Ready=True pods that had merely   *)
  (*     flipped Ready recently => ALL FOUR deleted.                        *)
  (*   * reproduced on flint's OWN csi-node DaemonSet (chart 1.21.0): two   *)
  (*     spdk-tgt sidecars killed in the same second.                       *)
  (*************************************************************************)
  KubeDsArm,      \* TRUE = the DS controller exists.  FALSE in all 71
                  \* legacy cfgs, which pins podPhase at Init and disables
                  \* every new action, so no pre-existing behaviour graph
                  \* moves.  This is the SHIPPED path: RollingUpdate with
                  \* maintenance.drainRoll.enabled = FALSE.
  ReadyScope,     \* WHICH readiness predicate the probe computes — the
                  \* safety-critical choice, so every A/B varies it ALONE.
                  \*   "socket"    SHIPPED: startupProbe test -S
                  \*               /var/tmp/spdk.sock, a FILE, 2-5 min ahead
                  \*               of data-path recovery.
                  \*   "volume"    FORBIDDEN: a cross-node redundancy term.
                  \*   "selfLive"  self-scoped but LIVE, not latching — the
                  \*               probe a reviewer would actually write
                  \*               believing they had followed the design.
                  \*   "selfLatch" THE PROPOSAL: self-scoped, latching,
                  \*               deadline-bounded.
  PodTrigger,     \* WHICH trigger class may delete pods.  Craft rule 11: a
                  \* model carrying ONE trigger reports the others as
                  \* unreachable, and the measured result is exactly that
                  \* readiness paces one class and not the other.
                  \*   "template"  rollingUpdate() — the ONLY path that
                  \*               consults readiness.
                  \*   "evict"     manage() -> syncNodes(createDiff) — spot
                  \*               reclaim, OOM, eviction.  No accounting.
  MinReadySecsArm,\* TRUE = pods carry the minReadySeconds settle window.
  MaxKubeEvents,  \* budget on external readiness faults.
  GraceExceedsRecovery
                  \* THE TRANCHE'S DELIVERABLE.  Without it ReadyOnDeadline
                  \* is enabled in the very state PodStart produces, so the
                  \* "deadline" is TRUE, every socket-arm behaviour has a
                  \* step-for-step selfLatch counterpart, and NO run can
                  \* show the proposal beats shipping nothing.  TRUE encodes
                  \* "the configured grace outlives readmission latency" as
                  \* "the deadline cannot fire while this node is still a
                  \* warm standby awaiting admission".

VARIABLES
  \* ---- data plane -------------------------------------------------------
  serving,       \* SUBSET Legs: legs configured in the serving raid; {} = down
  \* ---- the raid COMPOSITION, as an object in its own right (F62) ---------
  \* Until F62 this module had only `serving`, whose comment above admits the
  \* conflation: "{} = down" folds together "the members left" and "the
  \* composition does not exist".  Those have DIFFERENT LIFETIMES, and the
  \* difference is the bug:
  \*
  \*   the lvols    live on disk, for the life of the PV — days/weeks
  \*   the volume   is staged for as long as a consumer wants it
  \*   the raid     lives exactly as long as ONE spdk-tgt process, on the one
  \*                node hosting the consumer — the most-restarted component
  \*                in the system
  \*
  \* Nothing else in the model has the raid's lifetime, so nothing else could
  \* express F62's state: the composition gone while every leg is healthy, on
  \* disk, and recorded in_sync.  It is NOT derivable from `serving` — its
  \* whole point is to gate the RESTORE actions.  Admit/HotRejoin add a member
  \* to an existing raid; with only `serving` they could lift the volume out of
  \* {} unconditionally, which is exactly why TLC believed every outage was
  \* recoverable and blessed the F61 fix that breaks live.
  raidHosts,     \* SUBSET (Legs \cup {"remote"}) — the set of tgt processes
                 \* holding a composition over these lvols.  {} = it does not
                 \* exist anywhere.  The host is MOBILE: node loss relocates
                 \* it with the consumer (the F42/drill-2.5 self-heal family).
                 \*
                 \* A SET, not a name, and that is the whole point of the A2
                 \* tranche.  Through the F62/F63 tranches this was a scalar
                 \* `raidHost`, which silently asserted the property A2 is
                 \* most likely to break: that at most one composition can
                 \* exist.  A scalar cannot represent two, so TLC could never
                 \* refute it, and `FlintReplicationRaidReconcile.cfg` went
                 \* green on a hazard it was structurally unable to see.
                 \* That is the pod-layer tranche's lesson a second time —
                 \* THE ABSTRACTION WAS THE BUG, two independent creators of
                 \* one object — and here the two creators are NodeStage
                 \* (Assemble) and the agent's own boot reconcile (A2).
                 \*
                 \* Every OTHER creator assigns a singleton, so cardinality
                 \* 2 is reachable through A2 and nothing else: the invariant
                 \* Inv_SingleComposition is a test of A2 specifically.
  staged,        \* BOOLEAN — kubelet believes the volume is staged on that
                 \* node.  THE DISCRIMINATOR between the three destroyers,
                 \* because only a destroyer that also clears it has an
                 \* INVERSE (kubelet will call NodeStage again):
                 \*
                 \*  1. consumer pod deleted -> node_unstage_volume ->
                 \*     teardown_volume_spdk_state step 2 -> bdev_raid_delete
                 \*     (driver.rs:3494).            staged' = FALSE — paired.
                 \*  2. node destroyed -> consumer relocates -> NodeStage on
                 \*     the NEW host re-creates it.  staged' = FALSE — paired.
                 \*     (1 and 2 are one equivalence class from the volume's
                 \*     point of view, which is why modelling F62 needs no
                 \*     mobile-consumer dimension.)
                 \*  3. the csi-node pod's tgt dies while node and consumer
                 \*     stay put: NO RPC, and kubelet still believes the
                 \*     volume staged.  staged UNCHANGED = TRUE — UNPAIRED.
                 \*     Nothing calls NodeStage, so nothing re-creates it.
                 \*     That is F62, and it is the whole of F62.
  raidSeen,      \* BOOLEAN — the host agent's data_path_raid_seen entry for
                 \* this volume.  PROCESS-SCOPED: a HashSet in the node agent,
                 \* emptied by the very restart that destroys the composition.
                 \* CollapseEvent::Lost needs it, so the detector is disabled
                 \* EXACTLY when the hazard fires (F62a).  The third instance
                 \* this cycle of "a guard silently disables progress and no
                 \* property asks" — so a property asks now.
  raidLostOnce,  \* GHOST (BOOLEAN): a class-3 destroyer has fired at least
                 \* once — the composition died with a tgt process while the
                 \* volume stayed staged.  Exists to turn "can the volume
                 \* EVER come back?" into a reachability question TLC answers
                 \* with a trace, instead of a liveness question that a
                 \* flapping environment and a finite bounce budget can
                 \* defeat for reasons unrelated to the fix under test.
  relocating,    \* GHOST (BOOLEAN): a consumer relocation is in flight — the
                 \* volume is legitimately down between the old host's
                 \* NodeUnstage and the new host's NodeStage.  Exists so the
                 \* maintenance theorem can keep its teeth: "a planned roll
                 \* never takes the volume down" must not be defeated by an
                 \* external pod reschedule, and must not be quietly widened
                 \* to excuse one either.  Cleared by the assembly that ends
                 \* the window, and RelocationWindowCloses obligates that it
                 \* ends.  THE B'-vs-A2 DISCRIMINATOR: if the ROLLER triggers
                 \* the relocation (fix B', ~95s of guest stall measured on
                 \* runap) then the outage IS maintenance-caused and this
                 \* exemption is what would be hiding it — which is the whole
                 \* argument for A2, where no window opens at all.
  localLegs,     \* SUBSET Legs — legs whose node currently hosts the
                 \* consumer, i.e. whose drain is a no-op and whose ROLL
                 \* destroys the composition.  A VARIABLE, not the constant,
                 \* because the consumer moves: the refused set is a fact of
                 \* the moment.  Pinned to LocalLegs when ~ConsumerMobile.
  stagedAt,      \* Legs \cup {"remote", "none"} — WHICH node kubelet believes
                 \* the volume is staged on.  `staged` is the same fact with
                 \* the location thrown away, kept because many guards read it
                 \* (Inv_StagedAgrees ties them together).
                 \*
                 \* Added for A2, and it turns out to be the answer rather
                 \* than more scenery.  The F62 doc already named `staged` THE
                 \* DISCRIMINATOR between the three destroyers — only one that
                 \* clears it has an inverse — and A2's whole problem is
                 \* telling "the composition died under me and is owed here"
                 \* apart from "the consumer left and the attacher has not
                 \* noticed".  The VA cannot tell those apart.  This can:
                 \*
                 \*   class-3 death   stagedAt = me   (kubelet still believes
                 \*                                    it staged HERE)
                 \*   relocation      stagedAt # me   (NodeUnstage ran, and
                 \*                                    the new host restaged)
                 \*
                 \* And it is LOCAL ground truth — the node's own staging
                 \* state, observable without asking the API server and
                 \* without remembering anything, which is the property F63's
                 \* two fidelity bugs were both about not having.
  vaNode,        \* Legs \cup {"remote", "none"} — the node named by the
                 \* attached VolumeAttachment.  A2's ONLY input in its naive
                 \* form, so it is modelled separately from localLegs (the
                 \* truth) rather than derived from it.
                 \*
                 \* This is the API-server view, and it is the RIGHT input:
                 \* it survives the agent's death (a local HashSet does not),
                 \* which is F8's lesson and the same predicate A1's seed
                 \* already trusts — `va_map.get(&pv_name) ==
                 \* Some(&self.node_name)` at node_agent.rs:3279.
                 \*
                 \* But it LAGS, and the implementation says so in its own
                 \* words.  node_agent.rs:3219 explains the ublk reaper's
                 \* existence: "a disk that is attributable but not desired
                 \* is a leak — e.g. the local disk a STALE VA made us
                 \* rebuild after the consumer moved away — and the fast
                 \* detector would otherwise resurrect it forever."  So
                 \* rebuild-from-a-stale-VA is not a hypothesis; it is an
                 \* observed behaviour with a garbage collector already
                 \* written for it.
                 \*
                 \* The asymmetry that earns A2 its own tranche: for the
                 \* single-replica path that comment describes, a stale-VA
                 \* rebuild leaks a ublk disk — recoverable, and reaped.  For
                 \* a RAID over lvols that another host may also have
                 \* assembled, the same staleness produces a SECOND WRITER
                 \* over the same bytes.  Same trigger, one class worse
                 \* outcome, and no reaper can undo a write.
                 \* Free to track truth atomically when ~VaCanLag.
  a2Created,     \* SUBSET (Legs \cup {"remote"}) — PROVENANCE: hosts whose
                 \* composition was built by A2 rather than by NodeStage.
                 \* The adopt hazard is entirely about WHO created the object
                 \* NodeStage later finds, which is also why the shipped
                 \* unconditional reuse is correct TODAY: with NodeStage the
                 \* only creator, this set is always empty.
  adoptedA2,     \* GHOST (BOOLEAN): NodeStage short-circuited onto a
                 \* composition A2 had built.  Stamped only by AssembleAdopt,
                 \* so Inv_NoAdoptOfA2Composition is attributable to that door
                 \* alone — the pod-layer tranche's rule about mutation runs
                 \* whose invariant is violable by other means.
  validateRemoved, \* SUBSET (Legs \cup {"remote"}) — hosts where the
                 \* VALIDATING NodeStage has deleted a composition.  Monotone,
                 \* like raidLostOnce: it is the memory a reachability ghost
                 \* needs, not live state.
  flapped,       \* GHOST (BOOLEAN): A2 rebuilt a composition at a host the
                 \* validating NodeStage had just deleted one from.  THE LOOP,
                 \* and it has to be stated as an ORDER rather than a count.
                 \* My first attempt was ~(a2Builds >= 2 /\ validateDeletes >=
                 \* 1) and TLC answered it with build, build, delete — two
                 \* builds and a delete, no cycle.  Violable for a reason other
                 \* than the mechanism, which the pod-layer tranche's rule says
                 \* proves nothing about the mechanism.  This ghost is stamped
                 \* only when A2 puts back what validation removed.
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
                 \* volume was down (GateDeadline; chert.us/f36c-defer is
                 \* per-volume and survives NodeStage retries) — cleared
                 \* by the next assembly, exactly like the code's two
                 \* clear sites (missing-empty and ServeWithRisk)
  crashes,       \* failure budget spent
  \* ---- planned maintenance (the csi-node roll) ---------------------------
  rolling,       \* SUBSET Legs: node whose tgt is down for a PLANNED restart
  rolled,        \* SUBSET Legs: nodes already rolled this campaign (monotone)
  suppress,      \* SUBSET Legs: readmission suppressed (the maintenance mark)
  processed,     \* SUBSET Legs: the drain PASS completed for this node,
                 \* whether or not it stamped a mark.  F61: the shipped
                 \* code conflated this with `suppress`, because in THIS
                 \* module MaintDrain always minted a mark, so "drained"
                 \* and "marked" were the same event and one variable
                 \* served both roles.  In the code they come apart —
                 \* drain_leg marks per VOLUME, so a node whose every
                 \* volume is skipped (consumer == node, unattached, no
                 \* legs) finishes a pass having marked nothing — and the
                 \* planner, keyed on marks, then never reaches DeletePod.
  maintSkipped,  \* SUBSET Legs: nodes the roller REFUSED to roll and SURFACED
                 \* as an operator-actionable condition (F62 fix B).  A
                 \* refusal is only honest if it is visible, so the refused
                 \* set is state the campaign-progress property can name —
                 \* the alternative is F61's silent wedge wearing a new hat.
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
  everServed,    \* SUBSET Legs — legs that have served in their CURRENT
                 \* incarnation (Replace/Scrub wipe the payload and drop
                 \* membership).  The ghost that makes the admit-before-
                 \* gate theorem non-vacuous.
  \* ---- the kube-DS axis (2026-07-30) ------------------------------------
  podPhase,      \* [Nodes -> {"up","gone"}].  The POD, which is NOT the node
                 \* and NOT the tgt.  SCOPE LIMIT, recorded because it now
                 \* reads as a contradiction next to Responsive: TgtDie
                 \* leaves podPhase alone, so a node whose tgt died is still
                 \* pod-present.  The module has no action where a tgt death
                 \* and a pod-readiness change are the SAME event — which is
                 \* precisely the coupling a readinessProbe creates.
  oldRev,        \* SUBSET Nodes — pods still on the pre-bump
                 \* ControllerRevision.  A SET, re-minted by every
                 \* TemplateBump, and PodStart removes n so a replacement is
                 \* born current.  Deliberately NOT a counter: a counter
                 \* admits exactly ONE campaign, and craft rule 5 — a scalar
                 \* asserts a uniqueness the hazard denies.
  probeGreen,    \* SUBSET Nodes — the MEMOIZED verdict of FLINT'S OWN probe.
                 \* kubelet stores a pod condition and the controller reads
                 \* the STORED one, so memoizing captures the one tick of
                 \* probe staleness an inline evaluation cannot.
  readyLatch,    \* SUBSET Nodes — the latch.  Monotone WITHIN an
                 \* incarnation, with exactly ONE clearer (PodStart, and
                 \* deletion).  That single-clearer fact is what makes the
                 \* safety theorem structural rather than asserted.
  settled,       \* SUBSET Nodes — minReadySeconds satisfied.  Cleared on the
                 \* GREEN readiness transition as well as at pod start,
                 \* because IsPodAvailable reads the Ready condition's
                 \* LastTransitionTime.  Clearing only at start would be
                 \* bookkeeping keyed on a REMEMBERED EVENT instead of the
                 \* LIVE CONDITION (craft rule 7) and would make the MEASURED
                 \* four-healthy-pods-deleted state unreachable.
  extRed,        \* SUBSET Nodes — readiness reddened by something that is
                 \* NOT the data path.  THE PRODUCER: a csi-node pod is Ready
                 \* only when all four containers are, so the pod verdict
                 \* already has an input unrelated to spdk-tgt.  This is
                 \* exactly how runaq made 2 of 4 pods NotReady on the
                 \* SHIPPED chart.  It is a conjunct of availability on EVERY
                 \* arm: the fix arm does not get to be green by having its
                 \* producer removed from it.
  dsDeleted,     \* SUBSET Nodes — pods the controller has deleted.
  kubeEvents,    \* 0..MaxKubeEvents
  probeReddened, \* BOOLEAN ghost.  Single writer: ProbeEval's red branch,
                 \* stamped only when the pod is present AND its tgt is up.
  relatched,     \* BOOLEAN ghost.  Single writer: ReadyOnRecovery, stamped
                 \* when a DELETED leg latches again — so the barrier probe
                 \* names the ACTION (readmission ran) and not the SITUATION.
  budgetBroken   \* BOOLEAN ghost.  Single writer: DsRollingUpdate.
                 \* Inv_DsBudgetNeverBroken is a claim about a STEP (how many
                 \* LIVE tgts died in one sync) and a state invariant cannot
                 \* see a step.  Scoped to TgtUp nodes: TgtDie leaves podPhase
                 \* alone, so an unscoped count would fire on a node the model
                 \* is pretending is up — arithmetic, not harm.

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
          rolling, rolled, suppress, processed, maintSkipped, rollerDead,
          legSize, raidSize, pvSize, wantNew,
          raidHosts, staged, stagedAt, raidSeen, raidLostOnce, localLegs,
          relocating, vaNode, a2Created, adoptedA2, validateRemoved, flapped,
          bounceWindow, bouncePlan, bounceRisk, consecutiveBounces, dpFlag,
          everServed, podUp, bounceDoomed>>

maintVars == <<rolling, rolled, suppress, processed, maintSkipped, rollerDead,
               stalePlan, leaderMoved>>

\* The F62 tranche's state, grouped like maintVars so every untouched action
\* carries exactly one extra UNCHANGED line.  Pinned inert when
\* RaidLifetimeArm = FALSE, which is why the 50 pre-F62 runs are unperturbed.
raidVars == <<raidHosts, staged, stagedAt, raidSeen, raidLostOnce, localLegs,
              relocating, vaNode, a2Created, adoptedA2, validateRemoved,
              flapped>>

expandVars == <<legSize, raidSize, pvSize, wantNew>>

\* The audit tranche's one new piece of persistent state: the per-volume
\* f36c-defer deadline flag (grouped for the UNCHANGED lists).
gateVars == <<deferExpired>>

\* The cutover tranche's state (grouped like maintVars/expandVars so every
\* untouched action carries exactly one extra UNCHANGED line).
bounceVars == <<bounceWindow, bouncePlan, bounceRisk, consecutiveBounces,
                dpFlag, everServed, podUp, bounceDoomed>>

\* The kube-DS tranche's state.
\*
\* NOTE dsVars is deliberately NOT in `vars`, following the stalePlan /
\* leaderMoved precedent noted above.  Keeping the tuple textually identical
\* means [][Next]_vars and all THIRTY-EIGHT WF_vars(...) conjuncts are
\* unchanged from before this tranche, so temporal checking cannot drift —
\* the strongest zero-perturbation guarantee available, and it removes any
\* need to touch the fairness block.  Fairness still works: WF_vars(A) needs
\* an A-step that changes `vars`, and such steps exist because A's own
\* variables still move.
\*
\* AND these do NOT appear in ~60 per-action UNCHANGED lists, unlike every
\* previous tranche.  That route was measured and REJECTED: the existing
\* groups do not nest uniformly (gateVars appears at 55 sites but only 35 are
\* followed by bounceVars), so no scripted insertion covers every action, and
\* a MISSED UNCHANGED is not loud — TLC reports an incompletely-specified
\* successor only when that action actually FIRES, so a rarely-taken action
\* survives the cheap runs and fails at run 60 of 91.  Instead Next wraps the
\* whole legacy disjunction in `/\ UNCHANGED dsVars` exactly once; see Next.
dsVars == <<podPhase, oldRev, probeGreen, readyLatch, settled, extRed,
            dsDeleted, kubeEvents, probeReddened, relatched, budgetBroken>>

\* A forced-stale (StaleFloor) member keeps record-state "stale" while it
\* serves — the only way a stale-state leg is ever in the serving set
\* (MonitorMarkStale requires l \notin serving; the drain removes and
\* marks in one CAS; LastResortServe stamps its survivor insync).  The
\* content theorems escape on this exactly while the knowingly-behind
\* leg actually serves; the moment it deconfigures or a reassembly
\* excludes it, the theorems re-arm.
StaleServed == \E l \in serving : state[l] = "stale"

(***************************************************************************)
(* THE RAID COMPOSITION (F62).  Where the composition lives when it        *)
(* exists: co-located with the local half if there is one, else on a node  *)
(* owning no leg at all — the ordinary RWX shape, where the raid reaches    *)
(* BOTH legs over NVMe-oF.  A base may be local or remote; the composition  *)
(* does not care, which is why "remote" is a legal host and not an error.   *)
(***************************************************************************)
RaidHostInit == IF LocalLegs = {} THEN "remote" ELSE CHOOSE l \in LocalLegs : TRUE

\* Where a re-created composition lands: wherever the consumer is NOW.  The
\* F62 tranche used RaidHostInit here, which could only ever rebuild the raid
\* where it already was — fine while the consumer was immobile, and wrong the
\* moment it moves.  Live on runap the composition died on aws-1 and was
\* re-created on aws-2.
HostFor(LL) == IF LL = {} THEN "remote" ELSE CHOOSE l \in LL : TRUE

RaidPresent == raidHosts # {}

\* Where the composition is, when the model needs to name ONE — only ever
\* used for display and for the single-host predicates inherited from the
\* scalar era.  Safe because every creator other than A2 assigns a
\* singleton; when A2 has produced two, that is precisely the state
\* Inv_SingleComposition reports, so nothing downstream should be trusting
\* a single name in it.
PrimaryHost == IF raidHosts = {} THEN "none" ELSE CHOOSE h \in raidHosts : TRUE

\* The VA's view of where the consumer is, as the truth would have it.  A2
\* reads vaNode; this is what vaNode would be if the attacher were
\* instantaneous.  The gap between them is the whole hazard.
VaTruth == HostFor(localLegs)

(***************************************************************************)
(* A raid bdev by itself means NOTHING — it needs at least one healthy base *)
(* lvol attached, local or remote — and a healthy lvol with no composition *)
(* over it is just bytes on a disk.  So SERVICE is the conjunction, and the *)
(* two halves have different lifetimes.  Confirmed against SPDK            *)
(* v26.05.1-pre: raid1 carries                                             *)
(*   .base_bdevs_constraint = {CONSTRAINT_MIN_BASE_BDEVS_OPERATIONAL, 1}   *)
(* (raid1.c:622), so at ONE surviving base a raid1 stays a raid1, degraded  *)
(* and serving — there is no demotion to a direct lvol, SPDK has no such    *)
(* mechanism.  At ZERO, raid_bdev_remove_base_bdev_done decrements past the *)
(* floor and calls raid_bdev_deconfigure (bdev_raid.c:2069-2074): the raid  *)
(* bdev DESTROYS ITSELF.                                                   *)
(*                                                                         *)
(* SCOPE LIMIT, recorded rather than papered over: that last rule means     *)
(* serving = {} FORCES the composition gone, so Admit/HotRejoin lifting     *)
(* serving out of {} — which this module has always allowed — is optimistic *)
(* for every path, not just F62's.  Making it exact is a whole-model        *)
(* rework; this tranche checks the safe direction globally                  *)
(* (Inv_RaidCompositionCoupled) and gates the restore actions only under    *)
(* RaidLifetimeArm, so the arm-on runs answer the question and the 50       *)
(* pre-F62 runs keep their behavior graphs.                                 *)
(***************************************************************************)
VolumeUp == RaidPresent /\ serving # {}

\* The composition host may be a node that owns no leg — the ordinary RWX
\* shape.  The module has written (Legs \cup {"remote"}) inline since F62;
\* naming it once is for the kube-DS tranche, which quantifies over it in
\* nine actions.  Same set, so no existing definition changes meaning.
Nodes == Legs \cup {"remote"}

\* The shipped chart's maxUnavailable: node.yaml declares no rollingUpdate
\* block, so this is k8s's default of 1.
MaxUnavail == 1

TypeOK ==
  /\ serving \subseteq Legs
  \* ---- the kube-DS axis; no cardinality bound on any of these, because
  \* the ABSENCE of a bound is what lets TLC occupy the fleet-wide-delete
  \* state the tranche exists to find.
  /\ podPhase \in [Nodes -> {"up", "gone"}]
  /\ oldRev \subseteq Nodes
  /\ probeGreen \subseteq Nodes
  /\ readyLatch \subseteq Nodes
  /\ settled \subseteq Nodes
  /\ extRed \subseteq Nodes
  /\ dsDeleted \subseteq Nodes
  /\ kubeEvents \in 0..MaxKubeEvents
  /\ probeReddened \in BOOLEAN
  /\ relatched \in BOOLEAN
  /\ budgetBroken \in BOOLEAN
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
  /\ processed \subseteq Legs
  /\ maintSkipped \subseteq Legs
  /\ raidHosts \subseteq (Legs \cup {"remote"})
  /\ vaNode \in Legs \cup {"remote", "none"}
  /\ stagedAt \in Legs \cup {"remote", "none"}
  /\ a2Created \subseteq (Legs \cup {"remote"})
  /\ adoptedA2 \in BOOLEAN
  /\ validateRemoved \subseteq (Legs \cup {"remote"})
  /\ flapped \in BOOLEAN
  /\ staged \in BOOLEAN
  /\ raidSeen \in BOOLEAN
  /\ raidLostOnce \in BOOLEAN
  /\ localLegs \subseteq Legs
  /\ relocating \in BOOLEAN
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

\* ---- kube-DS helpers.  Declared here because Responsive reads TgtUp.
\* (Nodes and MaxUnavail live above TypeOK, which types podPhase over Nodes.)

PodPresent(n) == podPhase[n] = "up"

\* "this node's spdk-tgt is serving": its pod exists AND, for a real leg,
\* its node is up.  Note the asymmetry with TgtDie, which kills the
\* composition WITHOUT touching podPhase — see the podPhase scope limit.
TgtUp(n) == PodPresent(n) /\ (n \in Legs => legUp[n] = "up")

\* A leg's data path answers: its node is up AND its tgt is not down for a
\* planned restart.  The raid cannot tell the two apart — that symmetry is
\* the whole landmine, and every data-plane guard below uses this, not
\* legUp alone.  With MaintEnabled = FALSE, rolling = {} always and this
\* reduces to the old legUp = "up".
\*
\* The KubeDsArm conjunct is how a DELETED POD reaches all ~20 Responsive
\* sites at once: a csi-node pod delete kills spdk-tgt, so the leg stops
\* answering.  Arm-gated, so with KubeDsArm = FALSE it is (FALSE => P) =
\* TRUE and every legacy behaviour graph is untouched.
Responsive(l) == legUp[l] = "up" /\ l \notin rolling
                 /\ (KubeDsArm => TgtUp(l))

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

\* The same predicate for ONE node, which is what the kube-DS grace needs:
\* "the deadline cannot fire while this node is still a warm standby awaiting
\* admission" is how GraceExceedsRecovery encodes "the configured grace
\* outlives readmission latency".  Guarded on n \in Legs because state,
\* legData and legSize are functions over Legs, so an unguarded version is a
\* runtime error the moment it is applied to "remote".
WarmWaitingFor(n) ==
  /\ n \in Legs
  /\ state[n] = "standby"
  /\ Responsive(n)
  /\ AdmissionOpen(n)
  /\ epochCut \subseteq legData[n]
  /\ serving # {}
  /\ (SizeGuard => (SizeHeal \/ raidSize = "old" \/ legSize[n] = "new"))
  /\ \E src \in serving :
       /\ Responsive(src)
       /\ (RejoinGuard => legData[n] \subseteq legData[src])
       /\ (SizeGuard => (SizeHeal \/ legSize[src] = "old" \/ legSize[n] = "new"))

Init ==
  /\ serving = Legs
  \* ---- the kube-DS axis at rest.  With KubeDsArm = FALSE no action writes
  \* any of these, so they are constant across every state of all 71 legacy
  \* runs and each pre-existing state maps to exactly one new state — which
  \* is why the distinct-state counts are required to be IDENTICAL.
  /\ podPhase = [n \in Nodes |-> "up"]
  /\ oldRev = {}
  \* Nothing reports NotReady on the shipped chart: the csi-node DaemonSet
  \* declares no readinessProbe at all (verified live on chart 1.21.0).
  /\ probeGreen = Nodes
  \* LATCHED at Init, like every other variable here: the run starts from a
  \* healthy STEADY STATE (serving = Legs, every leg insync), i.e. pods that
  \* started long ago and have long since recovered.  With readyLatch = {}
  \* instead, the selfLatch verdict is FALSE at step 1 while the pod is still
  \* marked green, so ProbeEval's change-guard fires and reddens EVERY pod
  \* immediately — the fix arm would fail for a reason that has nothing to do
  \* with the fix.  The latch is cleared by PodStart, which is exactly when
  \* the grace is supposed to matter.
  /\ readyLatch = Nodes
  /\ settled = Nodes
  /\ extRed = {}
  /\ dsDeleted = {}
  /\ kubeEvents = 0
  /\ probeReddened = FALSE
  /\ relatched = FALSE
  /\ budgetBroken = FALSE
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
  /\ processed = {}
  /\ maintSkipped = {}
  \* The composition exists at Init, hosted where the consumer is: co-located
  \* with the local half when there is one, otherwise on a node outside Legs
  \* (the ordinary RWX shape — the NFS server's own node).  staged is what
  \* kubelet believes; raidSeen is TRUE because the agent has been running
  \* and has observed its own raid.
  /\ raidHosts = {RaidHostInit}
  \* The attacher agrees with reality at Init; only VaCanLag can separate them.
  /\ vaNode = RaidHostInit
  /\ stagedAt = RaidHostInit
  /\ a2Created = {}      \* no composition here was built by A2
  /\ adoptedA2 = FALSE
  /\ validateRemoved = {}
  /\ flapped = FALSE
  /\ staged = TRUE
  /\ raidSeen = TRUE
  /\ raidLostOnce = FALSE
  /\ localLegs = LocalLegs
  /\ relocating = FALSE
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* Silent unreachability: maybe a dying node, maybe a transient partition.
LegBlackhole(l) ==
  /\ legUp[l] = "up"
  \* WriterLimbo: a flapping node is not a volume failure, so it does not
  \* spend the budget — the only way to express indefinite limbo here.
  /\ (WriterLimbo \/ crashes < MaxCrashes)
  /\ legUp' = [legUp EXCEPT ![l] = "blackhole"]
  /\ crashes' = IF WriterLimbo THEN crashes ELSE crashes + 1
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
                 rolling, processed, maintSkipped, rollerDead, stalePlan, leaderMoved,
                 raidSize, pvSize, wantNew>>
  /\ UNCHANGED gateVars
  /\ UNCHANGED raidVars
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
  \* A base cannot join a composition that no longer exists (F62).  Every
  \* restore action in this module could lift `serving` out of {} regardless
  \* — the optimism that let TLC call every outage recoverable.
  /\ (RaidLifetimeArm => RaidPresent)
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ (RaidLifetimeArm => RaidPresent)     \* nothing to admit INTO (F62)
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
  /\ UNCHANGED raidVars
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
  \* ...AND NodeStage runs only for a volume kubelet believes UNSTAGED.  This
  \* one conjunct is the whole of F62a.  Class-1 and class-2 destroyers (see
  \* `staged`) clear the flag, so this action is their inverse and the volume
  \* comes back.  A tgt that dies under a still-staged volume clears nothing,
  \* so the sole creator of the composition is DISABLED exactly when the
  \* composition is missing — a permanent outage with every leg healthy, on
  \* disk, and recorded in_sync.  RaidReconcileArm is repair A2: the node
  \* agent re-creates the composition for volumes ITS OWN records say are
  \* staged here, needing no stage event and no superblock.
  /\ (RaidLifetimeArm => (~staged \/ RaidReconcileArm))
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
  \* Both code clear-sites for chert.us/f36c-defer are assembly-tick
  \* decisions (missing-empty and ServeWithRisk): the deadline re-arms
  \* fresh on the next deferral.
  /\ deferExpired' = FALSE
  \* THE CREATOR.  All three assignments are no-ops in arm-off worlds (Init
  \* pins exactly these values there), so the 50 pre-F62 runs are untouched.
  \* raidSeen' = TRUE because the process that just built the raid has, by
  \* construction, observed it — the honest way for data_path_raid_seen to
  \* become populated.
  \* UNION, not assignment.  NodeStage creates a composition on ITS node and
  \* has no idea what any other tgt holds — so it must not be the thing that
  \* tidies away a phantom A2 left elsewhere.  Assigning a singleton here
  \* would let the legitimate creator silently repair the illegitimate one,
  \* masking exactly the bug this tranche exists to find.  In the A2-off
  \* world the two forms are equivalent (no state has two hosts).
  /\ raidHosts' = raidHosts \cup {HostFor(localLegs)}
  /\ vaNode' = IF VaCanLag THEN vaNode ELSE HostFor(localLegs)
  /\ relocating' = FALSE   \* the assembly ENDS the window
  /\ staged' = TRUE
  /\ stagedAt' = HostFor(localLegs)   \* kubelet staged it HERE
  /\ raidSeen' = TRUE
  \* THIS host's composition is now NodeStage's work, not A2's.
  /\ a2Created' = a2Created \ {HostFor(localLegs)}
  /\ UNCHANGED <<raidLostOnce, localLegs, adoptedA2, validateRemoved,
                 flapped>>
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
  \* A CREATOR, like Assemble — the runbook stages the survivor, so a
  \* composition exists again.  Deliberately NOT gated on ~staged: the
  \* operator works outside kubelet's bookkeeping, which is the whole point
  \* of a runbook step.  It cannot mask F62 regardless, because it requires
  \* UpInSync = {} and F62 leaves every leg recorded in_sync.
  /\ raidHosts' = raidHosts \cup {HostFor(localLegs)}   \* union: see Assemble
  /\ vaNode' = IF VaCanLag THEN vaNode ELSE HostFor(localLegs)
  /\ relocating' = FALSE   \* the assembly ENDS the window
  /\ staged' = TRUE
  /\ stagedAt' = HostFor(localLegs)
  /\ raidSeen' = TRUE
  \* THIS host's composition is now NodeStage's work, not A2's.
  /\ a2Created' = a2Created \ {HostFor(localLegs)}
  /\ UNCHANGED <<raidLostOnce, localLegs, adoptedA2, validateRemoved,
                 flapped>>
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ l \notin localLegs                   \* F61: a local-half leg's drain
                                          \* is a NO-OP (MaintDrainSkip)
  \* F61: ONE node in flight. The roller takes pending.first() and finishes
  \* that node before starting the next, so a node cannot be processed while
  \* an earlier processed node is still unrolled. Without this the model
  \* interleaves two in-flight nodes — a state plan_roll cannot produce.
  \* F62: ...or REFUSED (fix B).  Without maintSkipped here a refused node
  \* would block every later node — F61's livelock rebuilt out of its own fix.
  /\ processed \subseteq (rolled \cup maintSkipped)
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
  /\ processed' = processed \cup {l}     \* the pass completed AND marked
  /\ UNCHANGED <<zombie, legData, legUp, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, claim, deemedDead, falseRisk,
                 crashes, rolling, rolled, maintSkipped, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* THE CONSUMER MOVES (2026-07-29).  The class-2 destroyer, as an action    *)
(* rather than a comment: the consumer pod is rescheduled onto another      *)
(* node, so NodeUnstage runs on the old host (composition deleted, `staged` *)
(* cleared — PAIRED, so Assemble is its inverse) and the set of legs whose  *)
(* node hosts the consumer CHANGES.                                        *)
(*                                                                         *)
(* This is what the F62 tranche could not say, and the omission mattered in *)
(* the operational direction rather than the safety one: with LocalLegs a   *)
(* constant, a refused node was refused in every reachable state, so no     *)
(* property could distinguish a roller that re-examines the condition every *)
(* tick (what the code does — maint_roll.rs recomputes local_consumer_nodes *)
(* from the gather) from one that gives up permanently.  The live gate      *)
(* settled it in 14 seconds; this action is how TLC gets to check it.       *)
(*                                                                         *)
(* Modelled as EXTERNAL — the operator's or the scheduler's act, not the    *)
(* roller's.  A roller that relocates consumers ITSELF (fix B', measured at *)
(* ~95s of guest stall per node on runap) would be a different action, and  *)
(* would have to answer for the outage it causes: the relocation window     *)
(* takes the volume down, which is exactly why it is not free and why A2    *)
(* (re-create the composition on boot) remains the better answer.           *)
(***************************************************************************)
RelocateConsumer(dest) ==
  /\ ConsumerMobile
  /\ RaidLifetimeArm            \* meaningless without a composition to move
  /\ dest \in Legs \cup {"remote"}
  /\ HostFor(localLegs) # dest  \* an actual move
  \* The consumer cannot land on a node whose tgt is mid-restart: kubelet
  \* would not schedule onto it, and the stage would fail if it did.
  /\ rolling = {}
  /\ localLegs' = IF dest = "remote" THEN {} ELSE {dest}
  \* Class-2 destroyer: unstage on the old host, and kubelet KNOWS, which is
  \* the whole difference from the roll's class-3 destroyer.
  \* HOST-SCOPED, not a wholesale clear.  NodeUnstage runs on the host the
  \* consumer is LEAVING and destroys the raid in THAT tgt; it cannot reach a
  \* composition in another process.  Written as `= {}` before the adopt
  \* tranche, which would have let this destroyer tidy away a phantom on the
  \* node the consumer is moving TO — masking the very hazard the tranche
  \* exists to find, the same trap as Assemble's union.
  \* In the A2-off world the composition always sits at the consumer, so the
  \* two forms coincide and no legacy graph moves.
  /\ raidHosts' = raidHosts \ {HostFor(localLegs)}
  \* Likewise: `serving` is global (one composition at a time is exact in the
  \* adopt cfgs), so it empties only when the DEPARTING host is the one that
  \* held the raid.  A surviving phantom keeps its members, which is what the
  \* later NodeStage inherits.
  /\ serving' = IF HostFor(localLegs) \in raidHosts THEN {} ELSE serving
  /\ a2Created' = a2Created \ {HostFor(localLegs)}
  \* THE LAG.  With VaCanLag the attached VolumeAttachment still names the OLD
  \* host across this transition, and VaCatchUp closes it later.  That window
  \* is what node_agent.rs:3219's reaper exists to clean up on the
  \* single-replica path, and it is where A2 can assemble a second writer.
  /\ vaNode' = IF VaCanLag THEN vaNode ELSE (IF dest = "remote" THEN "remote" ELSE dest)
  /\ staged' = FALSE
  /\ stagedAt' = "none"   \* NodeUnstage RAN — kubelet knows
  /\ raidSeen' = DpSeenRehydrate
  /\ relocating' = TRUE         \* the window OPENS here
  /\ UNCHANGED raidLostOnce     \* this destroyer has an inverse; not the F62 ghost
  /\ UNCHANGED <<adoptedA2, validateRemoved, flapped>>
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim,
                 deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, consecutiveBounces,
                 dpFlag, everServed, podUp, bounceDoomed>>

(***************************************************************************)
(* THE A2 TRANCHE (2026-07-29).  Three actions: the destroyer nobody can   *)
(* refuse, the attacher's lag, and the repair itself.                      *)
(***************************************************************************)

(***************************************************************************)
(* THE DESTROYER OUTSIDE THE ROLLER.  Identical in effect to RollStart's    *)
(* class-3 branch, and deliberately NOT gated on any maintenance state:     *)
(* no `rolling`, no marks, no lease, no barrier, no refusal.  A tgt dies    *)
(* with node and consumer in place and nothing asked the roller's opinion.  *)
(*                                                                         *)
(* This is the gap the whole F62/F63 pair was measured inside of.  Class-3  *)
(* destruction existed ONLY inside RollStart, so every run so far has been  *)
(* answering "can flint's roller destroy a composition?" — a question about *)
(* a feature that is OFF BY DEFAULT and which, in that default             *)
(* configuration, refuses to act at all (plan_roll returns Blocked when     *)
(* !on_delete, maint_roll.rs:248).  The operative question is whether a     *)
(* routine `helm upgrade` can, and the chart answers it: updateStrategy:    *)
(* OnDelete is emitted only inside the drainRoll.enabled conditional        *)
(* (node.yaml:13-24), so the shipped default DaemonSet takes k8s's          *)
(* RollingUpdate and rolls every node pod on a template change.  Add OOM    *)
(* kills, kubelet restarts, evictions, node-image upgrades and GitOps       *)
(* syncs, none of which consult flint at all.                              *)
(*                                                                         *)
(* Hence the ranking this tranche exists to test: fix B refuses, and fix    *)
(* B' relocates, but BOTH are properties of a roller that is not in this    *)
(* path.  Only a repair on the agent side has an inverse for THIS.          *)
(***************************************************************************)
TgtDie(l) ==
  /\ UncontrolledTgtDeath
  /\ RaidLifetimeArm
  /\ l \in raidHosts          \* it held the composition; nothing else to kill
  /\ raidHosts' = raidHosts \ {l}
  \* SPDK's own rule: with the composition gone there is nothing for a base
  \* to be a member of (raid_bdev_deconfigure, bdev_raid.c:2069-2074).
  /\ serving' = {}
  \* The detector's HashSet died with the process; A1 is what rehydrates it.
  /\ raidSeen' = DpSeenRehydrate
  /\ raidLostOnce' = TRUE
  \* UNPAIRED, and this is the whole point: `staged` untouched, so kubelet
  \* never calls NodeStage again, and `vaNode` untouched, because the volume
  \* really is still attached here.  No orchestrator was involved, so there
  \* is no orchestrator-side fix that could have declined it.
  /\ a2Created' = a2Created \ {l}   \* provenance dies with the composition
  /\ UNCHANGED <<adoptedA2, validateRemoved, flapped>>
  /\ UNCHANGED <<staged, stagedAt, vaNode, localLegs, relocating>>
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* THE SHIPPED PERIODIC REPAIR (layer 2), modelled 2026-07-30 to correct an *)
(* overstatement this tranche had made.                                     *)
(*                                                                         *)
(* `detect_lost_data_paths` counts consecutive attached-here-but-no-raid     *)
(* observations and, at its threshold, calls `repair_data_path`, which       *)
(* reassembles "exactly as NodeStage would (sync-record-aware)".  It needs   *)
(* NO seeded state: `data_path_raid_seen` gates the COLLAPSE EVENT           *)
(* (raid_collapse_verdict's `previously_seen`), not this.  So                *)
(* FlintReplicationUncontrolledBlind.cfg's green establishes only that the   *)
(* COLLAPSE-DETECTOR path is unreachable without A1 — never that the         *)
(* shipped code cannot recover.  With this armed it can, and                 *)
(* FlintReplicationUncontrolledStrike.cfg is the run that says so.           *)
(*                                                                         *)
(* THE BELT IS ALREADY HERE, and that is the finding worth keeping:          *)
(* `repair_data_path` refuses unless `is_staged_here(volume_handle)` —       *)
(* kubelet's own staging directory, read locally, nothing remembered.  This  *)
(* tranche derived exactly that predicate for A2 from first principles       *)
(* before noticing the implementer had already reached for it on the         *)
(* adjacent path.  So it is UNCONDITIONAL here, not behind                   *)
(* A2LocalStagingBelt: the shipped repair has no unbelted variant to model.  *)
(* A2 differs from this action only in its TRIGGER (boot, versus N ticks of  *)
(* strikes), which is why A2's benefit is latency and predicate simplicity   *)
(* rather than making recovery possible.                                     *)
(*                                                                         *)
(* The strike COUNT is abstracted away on purpose — a debounce against an    *)
(* in-flight NodeStage, not a safety guard.                                  *)
(***************************************************************************)
StrikeRepair ==
  /\ StrikeRepairArm
  /\ RaidLifetimeArm
  \* attached_here: the VA names this node (`attached_here.contains(pv)`).
  /\ vaNode \in Legs \cup {"remote"}
  \* !raid_present, for the raid name derived from this volume's handle.
  /\ vaNode \notin raidHosts
  \* is_staged_here — the belt, unconditional in the shipped code.
  /\ stagedAt = vaNode
  \* The repair runs INSIDE the node agent process, so it cannot run on a
  \* node whose pod is gone.  Without this the kube-DS tranche would let a
  \* deleted node repair itself and every barrier run would be green for a
  \* reason that cannot happen.  Arm-gated, so legacy runs are untouched.
  /\ (KubeDsArm => TgtUp(vaNode))
  \* Something the records vouch for to rebuild from.
  /\ UpInSync # {}
  /\ raidHosts' = raidHosts \cup {vaNode}
  /\ serving' = UpInSync
  /\ raidSeen' = TRUE
  /\ relocating' = FALSE
  \* Provenance stamped like A2's: this is a non-NodeStage creator, so if the
  \* belt above were ever removed the adopt theorem would catch it here too.
  \* It cannot fire falsely as things stand — the belt implies `staged`, and
  \* AssembleAdopt requires ~staged.
  /\ a2Created' = a2Created \cup {vaNode}
  /\ flapped' = (flapped \/ vaNode \in validateRemoved)
  /\ UNCHANGED <<staged, stagedAt, raidLostOnce, localLegs, vaNode,
                 adoptedA2, validateRemoved>>
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* THE ADOPT — what NodeStage does when it finds a raid of this name        *)
(* already there.  `ensure_raid1_bdev` (driver.rs:3105) is create-OR-       *)
(* CONVERGE, and its converge branch has two arms:                         *)
(*                                                                         *)
(*   state == "online"  -> REUSE IT AND RETURN.  "already ONLINE (N        *)
(*                         base(s) configured) — reusing".  The base set    *)
(*                         is never compared to the one NodeStage intended; *)
(*                         that count reaches the log line and nowhere else.*)
(*   anything else       -> delete (clear_sb when available) and create.    *)
(*                                                                         *)
(* Ordinary `Assemble` cannot represent the first arm, because it requires  *)
(* serving = {} — an online raid has at least one configured base (SPDK's   *)
(* raid1 floor is 1), so Assemble is DISABLED in exactly the state the      *)
(* adopt describes.  Hence a separate action for the short-circuit.         *)
(*                                                                         *)
(* Note this is NOT a bug report against shipped code.  With NodeStage the  *)
(* sole creator, an online raid of that name is one NodeStage itself built  *)
(* from the same PV replica record, so reuse is a correct idempotent        *)
(* restage.  It becomes a hazard only once A2 is a second creator whose     *)
(* base set was chosen at a different time — which is why the arm lives in  *)
(* this tranche.                                                           *)
(***************************************************************************)
AssembleAdopt ==
  /\ RaidLifetimeArm
  /\ ~NodeStageValidatesBases        \* the shipped reuse-unconditionally arm
  \* NodeStage runs, on the same terms as Assemble: a pod is scheduled and
  \* kubelet believes the volume UNSTAGED.
  /\ (PodLayer => podUp)
  /\ ~staged
  \* ...and finds a raid of this name ONLINE in this node's tgt.  `serving`
  \* non-empty IS "online" here: raid1's operational floor is one configured
  \* base (raid1.c:622), and at zero the bdev deconfigures itself.
  /\ HostFor(localLegs) \in raidHosts
  /\ serving # {}
  \* THE SHORT-CIRCUIT.  It returns early, so the composition and its member
  \* set are INHERITED WHOLE — serving, writerSet, lineage and legGen all
  \* untouched.  That is the defect in one line: whatever the previous
  \* creator chose is now what this consumer gets.
  /\ adoptedA2' = (adoptedA2 \/ HostFor(localLegs) \in a2Created)
  /\ staged' = TRUE
  /\ stagedAt' = HostFor(localLegs)
  /\ relocating' = FALSE
  /\ raidSeen' = TRUE
  /\ UNCHANGED <<serving, raidHosts, raidLostOnce, localLegs, vaNode,
                 a2Created, validateRemoved, flapped>>
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* THE CANDIDATE FIX, MODELLED HONESTLY — INCLUDING THAT IT DELETES.       *)
(*                                                                         *)
(* With NodeStageValidatesBases the converge branch stops trusting `online` *)
(* and takes the delete-then-create path instead, which is the shape the    *)
(* code already has for a CONFIGURING phantom.  The point of modelling it   *)
(* rather than asserting it: a fix whose remedy is to DESTROY the other     *)
(* creator's object invites the two creators to undo each other, and        *)
(* `validateRemoved` + `flapped` are here so TLC can say whether they do. *)
(* A remedy that swaps a phantom for a create/delete loop is not a remedy;  *)
(* it is the MaintPark lasso wearing a different hat.                      *)
(***************************************************************************)
AssembleValidate ==
  /\ RaidLifetimeArm
  /\ NodeStageValidatesBases
  /\ (PodLayer => podUp)
  /\ ~staged
  /\ HostFor(localLegs) \in raidHosts
  /\ serving # {}
  \* Delete it.  Ordinary Assemble is then enabled (serving = {}) and builds
  \* from the set NodeStage actually intended — two steps, as in the code.
  /\ raidHosts' = raidHosts \ {HostFor(localLegs)}
  /\ serving' = {}
  /\ a2Created' = a2Created \ {HostFor(localLegs)}
  /\ validateRemoved' = validateRemoved \cup {HostFor(localLegs)}
  /\ UNCHANGED <<staged, stagedAt, raidSeen, raidLostOnce, localLegs,
                 relocating, vaNode, adoptedA2, flapped>>
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* THE ATTACHER'S LAG CLOSING.  The VA catches up with where the consumer   *)
(* actually is.  Exists so VaCanLag is a WINDOW and not a permanent lie —   *)
(* without this action a stale VA would never converge and every liveness   *)
(* property would fail for a reason that has nothing to do with A2.         *)
(***************************************************************************)
VaCatchUp ==
  /\ VaCanLag
  /\ RaidLifetimeArm
  /\ vaNode # VaTruth
  /\ vaNode' = VaTruth
  /\ UNCHANGED <<a2Created, adoptedA2, validateRemoved, flapped,
                 serving, raidHosts, staged, stagedAt, raidSeen, raidLostOnce,
                 localLegs, relocating>>
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* REPAIR A2 ITSELF.  The agent's boot pass re-creates the composition for  *)
(* a volume the VA says is attached to it.  Modelled on the function that   *)
(* would host it — rehydrate_exports_from_ground_truth — which already      *)
(* does exactly this shape for the SINGLE-REPLICA path                      *)
(* (ensure_ublk_disk / ensure_export_for rebuilt from VA + PV), and which   *)
(* for replica_count > 1 currently only seeds detectors (A1).  So A2 is     *)
(* "extend the existing pass to the replica case", and its guard is the     *)
(* predicate that pass already trusts: node_agent.rs:3279's                 *)
(* `va_map.get(&pv_name) == Some(&self.node_name)`.                         *)
(*                                                                         *)
(* NOTE WHAT IS *NOT* IN THE GUARD: `staged`.  A2 does not consult kubelet  *)
(* — it cannot, it is a node agent reading the API server — and that is     *)
(* precisely why it can invert a class-3 death that left `staged` TRUE.     *)
(* The same blindness is the hazard: nothing in this guard distinguishes    *)
(* "the composition died under me and is owed here" from "the consumer      *)
(* left and the attacher has not caught up".                                *)
(***************************************************************************)
AgentBootReconcile ==
  /\ RaidReconcileArm
  /\ RaidLifetimeArm
  /\ vaNode \in Legs \cup {"remote"}
  /\ vaNode \notin raidHosts        \* nothing assembled here yet
  \* A raid needs at least one healthy base recorded in_sync — the lvols
  \* outlive everything, but a composition over nothing is not a repair.
  /\ UpInSync # {}
  \* CANDIDATE BELT 1 — sole ownership.  A cluster-wide "does anyone else
  \* already hold a composition over these lvols?" probe, using the same live
  \* call fix C already ships (bdev_raid_get_bdevs against another node,
  \* maint_roll.rs gather_volume_maint).
  \*
  \* Expected to be INSUFFICIENT, which is why it is armed separately from
  \* belt 2 rather than bundled with it.  It is check-then-act: it can only
  \* see a composition that already exists, so A2 defeats it by going FIRST —
  \* assemble on the stale node while raidHosts = {}, and the legitimate
  \* NodeStage (which has no such belt, and should not need one) supplies the
  \* second host afterwards.  If TLC agrees, the A/B says so out loud instead
  \* of this being a paragraph of my reasoning.
  /\ (A2SoleOwnershipBelt => raidHosts = {})
  \* CANDIDATE BELT 2 — local staging.  Assemble only where kubelet ALSO
  \* still believes the volume staged.  This is the discriminator the F62
  \* analysis already identified and then did not use: the class-3 destroyer
  \* is defined by leaving `staged` alone, and a relocation is defined by
  \* clearing it.  So this admits exactly the case A2 exists for and refuses
  \* exactly the case that produces a phantom — without a cluster-wide probe,
  \* without a lease, and without remembering anything.
  /\ (A2LocalStagingBelt => stagedAt = vaNode)
  /\ raidHosts' = raidHosts \cup {vaNode}
  \* The repair serves from what is recorded in_sync.  It deliberately does
  \* NOT readmit a leg the records do not vouch for: assembling a stale leg
  \* under a live consumer is the phantom-assembly class that
  \* "superblock": false exists to avoid, and A2 must not reintroduce it by
  \* another route.
  /\ serving' = UpInSync
  /\ raidSeen' = TRUE               \* the process that built it has seen it
  /\ relocating' = FALSE            \* an assembly ends the window
  \* `staged`/`stagedAt` UNTOUCHED, and this is not an oversight — it is the
  \* difference between A2 and NodeStage.  A2 is a node agent re-creating an
  \* SPDK object; it does not participate in kubelet's staging bookkeeping and
  \* cannot alter it.  Which is exactly why it can invert a class-3 death
  \* (staged was left TRUE, so nothing needs changing) and exactly why it must
  \* not be allowed to CLAIM it: the first draft here wrote stagedAt' = vaNode,
  \* which would let A2 satisfy the local-staging belt with its own previous
  \* action.  Self-justifying bookkeeping — the same defect as F63's two
  \* fidelity bugs, for the third time in this module, and this time caught
  \* while reading a counterexample rather than by a property failing.
  \* PROVENANCE: this host's composition was built by A2, not by NodeStage.
  \* Needed because the adopt hazard is entirely about WHO created the object
  \* NodeStage later finds — and today, with NodeStage the only creator, an
  \* online raid of that name is one NodeStage itself made, which is why
  \* reusing it unconditionally is correct in the shipped world.
  /\ a2Created' = a2Created \cup {vaNode}
  \* THE LOOP, stamped only when A2 puts back exactly what the validating
  \* NodeStage removed.  An order, not a count — see the `flapped` comment.
  /\ flapped' = (flapped \/ vaNode \in validateRemoved)
  /\ UNCHANGED <<staged, stagedAt, raidLostOnce, localLegs, vaNode,
                 adoptedA2, validateRemoved>>
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* THE REFUSAL, SURFACED FROM THE LIVE CONDITION (2026-07-29).  Mirrors     *)
(* plan_roll's terminal `RollStep::Refused`, whose node list is derived      *)
(* every tick from `pods.filter(!current_rev).filter(local_consumer_nodes)`  *)
(* — NOT from whoever happened to take the skip path.                       *)
(*                                                                         *)
(* Found by RefusalEventuallyClears, and it is the SAME mistake as the       *)
(* eligibility gate one screen up, made twice in one tranche: bookkeeping    *)
(* keyed on a remembered event instead of the live condition.  Trace: l1 is  *)
(* drained while remotely consumed, so it enters `processed`; the consumer   *)
(* then RELOCATES ONTO l1, so it becomes refusable — but MaintDrainSkip      *)
(* requires l \notin processed and can never record it, so `maintSkipped`    *)
(* never contains l1, the one-node-in-flight gate                           *)
(* processed \subseteq (rolled \cup maintSkipped) is poisoned forever, and   *)
(* NO further node can drain.  A campaign killed by its own bookkeeping.     *)
(*                                                                         *)
(* The code is not exposed to this: it keeps no `processed` set at all and   *)
(* re-derives every list from the gather, which is why the live gate saw a   *)
(* refusal clear in 14 seconds.  `processed` is a model construct added for  *)
(* F61 fidelity, so this repair belongs in the model.                       *)
(***************************************************************************)
MaintRefuse(l) ==
  /\ MaintEnabled /\ MaintLocalRefuse
  /\ ~rollerDead
  /\ l \in localLegs                      \* hosts the composition RIGHT NOW
  /\ l \notin rolled                      \* still pending
  /\ l \notin maintSkipped                \* not already surfaced
  /\ rolling = {}
  /\ maintSkipped' = maintSkipped \cup {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes, rolling,
                 rolled, suppress, processed, rollerDead, stalePlan,
                 leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

(***************************************************************************)
(* F61: the drain pass that legitimately drains NOTHING.  maint_roll.rs    *)
(* iterates the node's volumes and `continue`s on each one whose consumer  *)
(* IS this node (emitting MaintenanceLocalConsumer), so the pass completes *)
(* with drained=0 and stamps no mark.  The node is still PROCESSED — the   *)
(* roll is meant to restart its tgt anyway; the local half is a documented *)
(* data-path gap, not a reason to skip the node.  With MaintProcessedGate  *)
(* FALSE (the shipped predicate) RollStart can never fire for such a node  *)
(* and the campaign livelocks: RollCampaignCompletes fails.                *)
(***************************************************************************)
MaintDrainSkip(l) ==
  /\ MaintEnabled /\ MaintFence
  /\ ~rollerDead
  /\ l \in localLegs
  /\ l \notin rolled
  /\ l \notin processed
  \* F61: ONE node in flight. The roller takes pending.first() and finishes
  \* that node before starting the next, so a node cannot be processed while
  \* an earlier processed node is still unrolled. Without this the model
  \* interleaves two in-flight nodes — a state plan_roll cannot produce.
  \* F62: ...or REFUSED (fix B).  Without maintSkipped here a refused node
  \* would block every later node — F61's livelock rebuilt out of its own fix.
  /\ processed \subseteq (rolled \cup maintSkipped)
  /\ rolling = {} /\ suppress = {}
  /\ (MaintBarrier =>
        IF BarrierRaidAware THEN FullRedundancy ELSE RecordRedundancy)
  /\ processed' = processed \cup {l}
  \* F62 FIX B.  The pass ran and marked nothing — but the ROLL is what
  \* would break this volume, not the drain.  So refuse the node, record it
  \* for the operator ("N nodes host serving compositions and need manual
  \* handling"), and keep converging every other node.  Strictly better than
  \* both alternatives: better than F61's silent wedge, and better than
  \* F61's-fix-alone, which converts the wedge into a silent OUTAGE.
  /\ maintSkipped' = IF MaintLocalRefuse THEN maintSkipped \cup {l}
                                         ELSE maintSkipped
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes, rolling,
                 rolled, suppress, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

RollStart(l) ==
  /\ MaintEnabled
  /\ ~rollerDead
  /\ legUp[l] = "up"
  /\ rolling = {} /\ l \notin rolled
  \* F61: what makes a node ELIGIBLE to have its pod deleted.  The fix
  \* asks "did the drain pass run?"; the shipped code asked "is there a
  \* mark?" — and a mark only exists if some volume was actually drained.
  /\ (MaintFence =>
        IF MaintProcessedGate
          THEN
            \* F61, third attempt — and the first two were instructive.
            \* `processed` alone is WRONG: it is monotone, so a leg that was
            \* drained, rolled-past and later READMITTED stays eligible
            \* forever and RollStart fires while it is serving —
            \* Inv_MaintFenceHolds fails.  That is this very bug mirrored:
            \* I swapped a TRANSIENT eligibility token (suppress = "drained
            \* right now") for a PERMANENT one.  Eligibility must be
            \* transient, so state it on the ground truth the fence cares
            \* about — is this leg out of the serving raid?
            /\ l \in processed                 \* the drain pass ran
            /\ \/ l \notin serving             \* drained: the fence proper
               \* The local half, knowingly: its drain was skipped, so it
               \* IS still serving.  Roll it anyway (that is the documented
               \* design) but never as the last serving member, or the roll
               \* manufactures an outage with zero real failures — TLC found
               \* that too, via Inv_PlannedRollNeverCausesOutage.
               \/ /\ l \in localLegs
                  /\ serving \ {l} # {}
          ELSE l \in suppress)
  \* F62 FIX B: a node hosting the composition is never rolled.
  \*
  \* Gated on the LIVE condition, not on the remembered `maintSkipped` set —
  \* and the first draft got this wrong in a way only RefusalEventuallyClears
  \* could catch.  Reading `l \notin maintSkipped` here makes a refusal
  \* PERMANENT: the set is monotone, so a node refused once could never roll
  \* even after its consumer left.  The shipped code does the opposite —
  \* maint_roll.rs rebuilds local_consumer_nodes from the gather every tick,
  \* which is why runap rolled the refused node 14s after the NFS server
  \* moved off it.  maintSkipped is an OBSERVABILITY record (what was
  \* surfaced to the operator), never an eligibility gate.
  /\ (MaintLocalRefuse =>
        IF RefusalSticky THEN l \notin maintSkipped   \* the bug
                         ELSE l \notin localLegs)     \* shipped
  /\ rolling' = {l}
  \* ===================================================================
  \* THE F62 DESTROYER (class 3).  Deleting this node's csi-node pod kills
  \* its spdk-tgt, and if that process is the one holding the composition,
  \* the composition dies with it.  Note what is NOT here: no RPC, no
  \* base-bdev removal, no leg fault, no record write.  The lvols are fine
  \* — 9 bdevs still present on runao — and `state` still reads every leg
  \* in_sync, which is why F62b's record-only barrier waved the campaign
  \* on one tick later.  Above all, `staged` is UNTOUCHED: kubelet still
  \* believes the volume staged, so Assemble — the only creator — stays
  \* disabled forever.  THAT is the difference between this destroyer and
  \* the other two, and it is the entire bug.
  \* ===================================================================
  /\ IF RaidLifetimeArm /\ l \in raidHosts
       THEN /\ raidHosts' = raidHosts \ {l}
            \* SPDK's own rule, not an extra assumption: with the composition
            \* gone there is nothing for a base to be a member of
            \* (raid_bdev_deconfigure, bdev_raid.c:2069-2074).
            /\ serving' = {}
            \* The detector's HashSet dies with the process.  Rehydrating it
            \* from the STAGED set (repair A1) is what lets the fresh process
            \* know a raid is owed; seeding it from live SPDK would read an
            \* empty raid list and be a no-op exactly here.
            /\ raidSeen' = DpSeenRehydrate
            /\ raidLostOnce' = TRUE
            \* The VA is UNTOUCHED — it still names this node, because the
            \* volume really is still attached here.  That is what makes A2
            \* fire on exactly the right node in the ordinary case, and it is
            \* also why A2's guard alone cannot tell this apart from a stale
            \* VA left by a relocation.
            /\ a2Created' = a2Created \ {l}   \* provenance dies with the composition
            /\ UNCHANGED <<adoptedA2, validateRemoved, flapped>>
            /\ UNCHANGED <<staged, stagedAt, localLegs, relocating, vaNode>>
       ELSE UNCHANGED <<a2Created, adoptedA2, validateRemoved, flapped,
                        serving, raidHosts, staged, stagedAt, raidSeen,
                        raidLostOnce, relocating, localLegs, vaNode>>
       \* localLegs is the consumer's business, never the roller's — the
       \* roller only ever READS it, which is precisely why fix B must gate
       \* on it live rather than on a set it remembers.
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 rolled, suppress, processed, maintSkipped, rollerDead, stalePlan, leaderMoved>>
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
                 suppress, processed, maintSkipped, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

MaintClear(l) ==
  /\ ~rollerDead
  /\ l \in suppress
  \* F63: the mark lifts on EITHER the restart completing, or the node having
  \* become un-rollable under the refusal.  The second disjunct is not a
  \* convenience — without it a node drained-and-marked while remotely
  \* consumed, onto which the consumer then relocates, is refused by fix B
  \* while its mark is renewed by the live roller every tick: its leg parks at
  \* reduced redundancy FOREVER (MaintenanceEventuallyLifts, the MaintPark
  \* lasso re-created by the refusal).  Deleting the pod instead would be F62.
  \* Abandoning the node — lift the suppression, let hot-rejoin readmit, let
  \* the standing refusal report name it — is the only step that is neither.
  \* maint_roll.rs's marked-node branch returns ClearMarks for exactly this.
  /\ (l \in rolled \/ (MaintLocalRefuse /\ l \in localLegs))
  /\ suppress' = suppress \ {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 rolling, rolled, processed, maintSkipped, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
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
                 rolling, rolled, suppress, processed, maintSkipped, stalePlan,
                 leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
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
                 rolling, rolled, processed, maintSkipped, rollerDead, stalePlan, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
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
                 rolling, rolled, suppress, processed, maintSkipped, rollerDead>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
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
                 crashes, rolling, rolled, processed, maintSkipped, rollerDead, leaderMoved>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
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
  \* F62 CORRECTION, and it was the old conflation biting again.  This read
  \*     /\ serving # {}                  \* something to tear down
  \*     /\ (BounceDataPathArm \/ BounceAdmissionArm)
  \* which is right for the ADMISSION arm — cutting a standby in needs a
  \* live assembly to cut over from — and exactly backwards for the
  \* DATA-PATH arm, which fires BECAUSE the data path is gone.  The code has
  \* no raid-membership term there at all: plan_cutover's first branch
  \* returns BounceNfsPod on the strength of a pvc_backed NFS pod alone, and
  \* says so — "The bounce IS the remediation — a restage rebuilds the raid
  \* from the in-sync replicas — so the standby/lag gates below do not apply
  \* (there is nothing to admit, only a data path to rebuild)"
  \* (cutover.rs:485-489).  `serving # {}` only looked correct while
  \* `serving` doubled as "the volume is up"; once the composition became
  \* its own object the guard disabled the remediation in precisely the
  \* state it exists to remediate.  Found by the A1 run failing for the
  \* WRONG REASON — the repair chain was armed, willing, and modelled shut.
  /\ IF RaidLifetimeArm
       THEN \/ (BounceDataPathArm /\ staged)
            \/ (BounceAdmissionArm /\ serving # {})
       ELSE /\ serving # {}
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
  \* CLASS-1/2 DESTROYER: the server pod goes away, so NodeUnstage runs
  \* (teardown_volume_spdk_state step 2 deletes the raid, driver.rs:3494)
  \* and kubelet's bookkeeping flips to NOT-staged.  Because it does, this
  \* destroyer HAS an inverse: Assemble is enabled and the volume comes
  \* back.  Contrast RollStart's class-3 destroyer, which leaves `staged`
  \* alone and therefore has none.  All three assignments are no-ops when
  \* RaidLifetimeArm is FALSE.
  \* Consumer-scoped teardown, written as a clear because in every cfg that
  \* enables a bounce arm raidHosts is a singleton (A2 is the only source of
  \* a second host, and no bounce cfg arms it).  Combining the bounce tranche
  \* with A2Arm would need this narrowed to \ {HostFor(localLegs)} first, or
  \* an unstage here could tidy away a phantom and mask it.
  /\ raidHosts' = IF RaidLifetimeArm THEN {} ELSE raidHosts
  /\ staged'   = ~RaidLifetimeArm
  /\ stagedAt' = IF RaidLifetimeArm THEN "none" ELSE stagedAt
  /\ raidSeen' = IF RaidLifetimeArm THEN DpSeenRehydrate ELSE raidSeen
  /\ a2Created' = IF RaidLifetimeArm THEN {} ELSE a2Created
  /\ UNCHANGED <<raidLostOnce, localLegs, relocating, vaNode,
                 adoptedA2, validateRemoved, flapped>>
  /\ UNCHANGED gateVars

\* THE REFUSAL BOUND (cutover.rs: `refusal_expired`).  freshness_gate::evaluate
\* is deliberately deadline-bounded — "Never hang (the 2.4 obligation)" — and
\* the belt above it must be too: on the data-path arm the volume is ALREADY
\* down, so refusing lengthens an outage instead of preventing one, and a leg
\* that oscillates unavailable/available never crosses the node_gone_secs line
\* that would make it honestly excusable (the flapping-kubelet case).  This is
\* Bounce WITHOUT BounceSafe, enabled only while a refusal would be standing.
\* Weakly fair: the bound is a wall clock, and clocks advance.
\* NOTE the guard is BouncePlannable alone, deliberately NOT
\* `BouncePlannable /\ ~BounceSafe`.  That first draft made this action and
\* Bounce ALTERNATE as the writer oscillated, so neither was ever
\* CONTINUOUSLY enabled and weak fairness obligated neither — the run came
\* back violated with the bound ON, which is the same WF ping-pong trap this
\* module already documents for the Acquire/Release claim pair.  The bound in
\* the code is a wall clock that does not consult the belt's current verdict
\* either: once a refusal has stood long enough, the bounce proceeds.
BounceOverride ==
  /\ RefusalBounded
  /\ ~PodLayer
  /\ BouncePlannable
  /\ consecutiveBounces < MaxBounces
  /\ serving' = {}
  /\ bounceWindow' = BounceRiskAtCommit    \* honestly "risky": we know it
  /\ consecutiveBounces' = consecutiveBounces + 1
  /\ bounceDoomed' = (bounceDoomed \/ BounceDoomedAtCommit)
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim,
                 deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bouncePlan, bounceRisk, dpFlag, everServed, podUp>>
  /\ UNCHANGED maintVars /\ UNCHANGED expandVars /\ UNCHANGED gateVars
  \* CLASS-1/2 DESTROYER: the server pod goes away, so NodeUnstage runs
  \* (teardown_volume_spdk_state step 2 deletes the raid, driver.rs:3494)
  \* and kubelet's bookkeeping flips to NOT-staged.  Because it does, this
  \* destroyer HAS an inverse: Assemble is enabled and the volume comes
  \* back.  Contrast RollStart's class-3 destroyer, which leaves `staged`
  \* alone and therefore has none.  All three assignments are no-ops when
  \* RaidLifetimeArm is FALSE.
  \* Consumer-scoped teardown, written as a clear because in every cfg that
  \* enables a bounce arm raidHosts is a singleton (A2 is the only source of
  \* a second host, and no bounce cfg arms it).  Combining the bounce tranche
  \* with A2Arm would need this narrowed to \ {HostFor(localLegs)} first, or
  \* an unstage here could tidy away a phantom and mask it.
  /\ raidHosts' = IF RaidLifetimeArm THEN {} ELSE raidHosts
  /\ staged'   = ~RaidLifetimeArm
  /\ stagedAt' = IF RaidLifetimeArm THEN "none" ELSE stagedAt
  /\ raidSeen' = IF RaidLifetimeArm THEN DpSeenRehydrate ELSE raidSeen
  /\ a2Created' = IF RaidLifetimeArm THEN {} ELSE a2Created
  /\ UNCHANGED <<raidLostOnce, localLegs, relocating, vaNode,
                 adoptedA2, validateRemoved, flapped>>

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
                 rolling, rolled, suppress, processed, maintSkipped, rollerDead, stalePlan>>
  /\ UNCHANGED <<bounceWindow, bounceRisk, consecutiveBounces, dpFlag,
                 everServed, podUp, bounceDoomed>>
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
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
  \* CLASS-1/2 DESTROYER: the server pod goes away, so NodeUnstage runs
  \* (teardown_volume_spdk_state step 2 deletes the raid, driver.rs:3494)
  \* and kubelet's bookkeeping flips to NOT-staged.  Because it does, this
  \* destroyer HAS an inverse: Assemble is enabled and the volume comes
  \* back.  Contrast RollStart's class-3 destroyer, which leaves `staged`
  \* alone and therefore has none.  All three assignments are no-ops when
  \* RaidLifetimeArm is FALSE.
  \* Consumer-scoped teardown, written as a clear because in every cfg that
  \* enables a bounce arm raidHosts is a singleton (A2 is the only source of
  \* a second host, and no bounce cfg arms it).  Combining the bounce tranche
  \* with A2Arm would need this narrowed to \ {HostFor(localLegs)} first, or
  \* an unstage here could tidy away a phantom and mask it.
  /\ raidHosts' = IF RaidLifetimeArm THEN {} ELSE raidHosts
  /\ staged'   = ~RaidLifetimeArm
  /\ stagedAt' = IF RaidLifetimeArm THEN "none" ELSE stagedAt
  /\ raidSeen' = IF RaidLifetimeArm THEN DpSeenRehydrate ELSE raidSeen
  /\ a2Created' = IF RaidLifetimeArm THEN {} ELSE a2Created
  /\ UNCHANGED <<raidLostOnce, localLegs, relocating, vaNode,
                 adoptedA2, validateRemoved, flapped>>
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
  /\ UNCHANGED raidVars

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
  \* CLASS-1/2 DESTROYER: the server pod goes away, so NodeUnstage runs
  \* (teardown_volume_spdk_state step 2 deletes the raid, driver.rs:3494)
  \* and kubelet's bookkeeping flips to NOT-staged.  Because it does, this
  \* destroyer HAS an inverse: Assemble is enabled and the volume comes
  \* back.  Contrast RollStart's class-3 destroyer, which leaves `staged`
  \* alone and therefore has none.  All three assignments are no-ops when
  \* RaidLifetimeArm is FALSE.
  \* Consumer-scoped teardown, written as a clear because in every cfg that
  \* enables a bounce arm raidHosts is a singleton (A2 is the only source of
  \* a second host, and no bounce cfg arms it).  Combining the bounce tranche
  \* with A2Arm would need this narrowed to \ {HostFor(localLegs)} first, or
  \* an unstage here could tidy away a phantom and mask it.
  /\ raidHosts' = IF RaidLifetimeArm THEN {} ELSE raidHosts
  /\ staged'   = ~RaidLifetimeArm
  /\ stagedAt' = IF RaidLifetimeArm THEN "none" ELSE stagedAt
  /\ raidSeen' = IF RaidLifetimeArm THEN DpSeenRehydrate ELSE raidSeen
  /\ a2Created' = IF RaidLifetimeArm THEN {} ELSE a2Created
  /\ UNCHANGED <<raidLostOnce, localLegs, relocating, vaNode,
                 adoptedA2, validateRemoved, flapped>>

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
  /\ UNCHANGED raidVars

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
  /\ UNCHANGED raidVars

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
  \* F62a.  CollapseEvent::Lost requires data_path_raid_seen.contains(pv) —
  \* "I saw this raid, and now it is gone".  The set is a plain HashSet in
  \* the agent PROCESS, so the restart that destroys the composition also
  \* empties the evidence that it ever existed, and the detector is disabled
  \* EXACTLY when the hazard fires.  Unlike its neighbours (exported_targets
  \* is seeded from live SPDK at startup, expected_ublk is backfilled by
  \* ground-truth rehydration) this one is never rehydrated at all.  The
  \* third time this cycle a guard has silently disabled progress with no
  \* property asking — so RaidEventuallyReassembled asks.
  /\ (RaidLifetimeArm => raidSeen)
  /\ dpFlag' = l
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED <<bounceWindow, bouncePlan, bounceRisk, consecutiveBounces,
                 everServed, podUp, bounceDoomed>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars

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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
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
  /\ UNCHANGED raidVars
  /\ UNCHANGED bounceVars

\* Wall-clock passage on a deferring volume: the persisted
\* chert.us/f36c-defer deadline (default 180s) elapses.  WF — time
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
  /\ UNCHANGED raidVars
  /\ UNCHANGED bounceVars

\* The legacy next-state relation, character-for-character as it stood before
\* the kube-DS tranche.  Wrapped by Next below.
NextLegacy ==
  \* The consumer relocating to a node that owns NO leg — the ordinary RWX
  \* shape, and unreachable from the per-leg quantifier below.
  \/ RelocateConsumer("remote")
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
  \/ BounceOverride
  \/ RogueBouncePlan
  \/ RogueBounceCommit
  \/ BounceDelete
  \/ BounceUnstage
  \/ BounceRecreate
  \/ ReconcilerRecreate
  \* ---- the A2 tranche.  AgentBootReconcile and VaCatchUp are not
  \* per-leg: A2 acts on whatever node the VA names, which may be
  \* "remote" (a node owning no leg — the ordinary RWX shape).
  \/ AgentBootReconcile
  \/ StrikeRepair
  \/ AssembleAdopt
  \/ AssembleValidate
  \/ VaCatchUp
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
       \/ MaintDrainSkip(l)
       \/ MaintRefuse(l)
       \* The consumer relocating TO l (and to "remote" below, outside the
       \* per-leg quantifier's reach only in name — dest ranges over both).
       \/ RelocateConsumer(l)
       \/ TgtDie(l)
       \/ RollStart(l)
       \/ RollFinish(l)
       \/ MaintClear(l)
       \/ SuppressExpire(l)
       \/ RoguePlanDrain(l)
       \/ ExpandLeg(l)
       \/ AgentFlag(l)
       \/ AgentClear(l)
       \/ AdmitAtStage(l)

(***************************************************************************)
(* THE KUBE-DS AXIS joins here, and this is the ONLY place the tranche      *)
(* touches the legacy next-state relation.                                  *)
(*                                                                          *)
(* Every previous tranche added `/\ UNCHANGED <group>` to each pre-existing  *)
(* action.  That route was measured and rejected for this one: the groups    *)
(* do not nest uniformly (gateVars at 55 sites, only 35 followed by          *)
(* bounceVars), so no scripted insertion reaches every action, and a MISSED  *)
(* UNCHANGED is not loud — TLC reports an incompletely-specified successor    *)
(* only when that action FIRES, so a rarely-taken action passes the cheap    *)
(* runs and fails at run 60 of 91.  Sixty individually-silent edits is the   *)
(* wrong risk in a module with four historical voids.                        *)
(*                                                                          *)
(* Wrapping the whole disjunction once is semantically exact and cannot      *)
(* miss an action by construction.                                           *)
(***************************************************************************)
(***************************************************************************)
(* THE KUBE-DS ACTIONS.  Every one is guarded on KubeDsArm, so with the     *)
(* legacy value FALSE none is enabled and no pre-existing behaviour graph   *)
(* can move — the property the zero-perturbation acceptance run pins.       *)
(***************************************************************************)

\* Shorthand for the DS actions that touch no data-plane state.  Mirrors the
\* UNCHANGED shape TgtDie uses; deferExpired is covered by gateVars.
LegacyUnchanged ==
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED raidVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* Availability, exactly as podutil.IsPodAvailable computes it: Ready, AND
\* Ready for longer than minReadySeconds.
\*
\* TWO DISTINCT INPUTS, and conflating them breaks the tranche in both
\* directions.  probeGreen is FLINT'S OWN probe verdict; extRed is some OTHER
\* container in the pod being NotReady.  A pod's Ready condition is the AND
\* over its containers, so extRed makes the POD unavailable while saying
\* nothing about flint's probe.  Folding extRed into the verdict would stamp
\* probeReddened even under selfLatch and fire Inv_ProbeNeverReddensLive on
\* the FIX arm — the guard would look broken when it is sound.
Avail(n) == /\ n \in probeGreen
            /\ n \notin extRed
            /\ (MinReadySecsArm => n \in settled)

\* THE BUG, transcribed from pkg/controller/daemon/update.go rollingUpdate().
\* numUnavailable counts a node whose CURRENT (new-revision) pod is not
\* available, or whose pod is missing.  An OLD-revision unavailable pod does
\* NOT increment it — it goes to allowedReplacementPods, which is appended
\* UNCLIPPED.  That asymmetry is the entire finding, and it is why V below
\* can exceed MaxUnavail.
NumUnavail ==
  Cardinality({n \in Nodes : \/ ~PodPresent(n)
                             \/ (PodPresent(n) /\ n \notin oldRev /\ ~Avail(n))})

\* The self-scoped recovery predicate.  Guarded on n \in Legs so it is not
\* vacuously TRUE for "remote" — an unguarded version latches for free and
\* the barrier run passes without the readmission path ever running.
SelfRecovered(n) ==
  /\ n \in Legs
  /\ (n \in raidHosts \/ state[n] = "insync")

ProbeVerdict(n) ==
  CASE ReadyScope = "socket"    -> TRUE
    [] ReadyScope = "volume"    -> FullRedundancy
    [] ReadyScope = "selfLive"  -> SelfRecovered(n)
    [] ReadyScope = "selfLatch" -> n \in readyLatch
    [] OTHER                    -> TRUE

\* ---- 1. the trigger that READINESS PACES ---------------------------------
TemplateBump ==
  /\ KubeDsArm
  /\ PodTrigger = "template"
  /\ kubeEvents < MaxKubeEvents
  /\ oldRev' = {n \in Nodes : PodPresent(n)}
  /\ kubeEvents' = kubeEvents + 1
  /\ UNCHANGED <<podPhase, probeGreen, readyLatch, settled, extRed,
                 dsDeleted, probeReddened, relatched, budgetBroken>>
  /\ LegacyUnchanged

\* ---- 2. the controller's one sync.  THE HAZARD. --------------------------
\* \E over C rather than CHOOSE: the controller's node iteration order is not
\* exposed, so every selection must be explored rather than one canonical pick.
DsRollingUpdate ==
  /\ KubeDsArm
  /\ PodTrigger = "template"
  /\ LET Old        == {n \in Nodes : n \in oldRev /\ PodPresent(n)}
         OldUnavail == {n \in Old : ~Avail(n)}
         OldAvail   == {n \in Old : Avail(n)}
         Remaining  == IF MaxUnavail > NumUnavail THEN MaxUnavail - NumUnavail
                                                  ELSE 0
         Clip       == IF Remaining < Cardinality(OldAvail) THEN Remaining
                                                            ELSE Cardinality(OldAvail)
     IN \E C \in SUBSET OldAvail :
          /\ Cardinality(C) = Clip
          /\ LET V == OldUnavail \cup C IN
               /\ V # {}
               /\ podPhase' = [n \in Nodes |-> IF n \in V THEN "gone" ELSE podPhase[n]]
               /\ dsDeleted' = dsDeleted \cup V
               \* THE HARM, scoped to tgts that were actually UP.  TgtDie
               \* leaves podPhase alone, so an unscoped count would fire on a
               \* node the model is pretending is up — arithmetic, not harm.
               /\ budgetBroken' = (budgetBroken
                                   \/ Cardinality({m \in V : TgtUp(m)}) > MaxUnavail)
               /\ probeGreen' = probeGreen \ V
               /\ readyLatch' = readyLatch \ V
               /\ settled'    = settled \ V
               /\ extRed'     = extRed \ V
               \* A pod delete kills spdk-tgt, and that IS a class-3 death:
               \* the composition dies, `staged` does NOT, so kubelet never
               \* calls NodeStage again and the destroyer has no inverse.
               \* WITHOUT THIS, VolumeUp can never fail and the whole
               \* grace-deadline triple is vacuous.  Mirrors TgtDie(:2093).
               /\ IF RaidLifetimeArm /\ (V \cap raidHosts) # {}
                  THEN /\ raidHosts' = raidHosts \ V
                       /\ serving' = {}
                       /\ raidSeen' = DpSeenRehydrate
                       /\ raidLostOnce' = TRUE
                       /\ a2Created' = a2Created \ V
                  ELSE UNCHANGED <<raidHosts, serving, raidSeen, raidLostOnce,
                                   a2Created>>
  /\ UNCHANGED <<oldRev, kubeEvents, probeReddened, relatched>>
  \* UNPAIRED, deliberately: staged/stagedAt/vaNode untouched.
  /\ UNCHANGED <<staged, stagedAt, vaNode, localLegs, relocating, adoptedA2,
                 validateRemoved, flapped>>
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim,
                 deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* ---- 3. kubelet recreates the pod.  The ONLY clearer of the latch. -------
PodStart(n) ==
  /\ KubeDsArm
  /\ ~PodPresent(n)
  /\ podPhase' = [podPhase EXCEPT ![n] = "up"]
  /\ oldRev'     = oldRev \ {n}          \* the replacement is born CURRENT
  /\ readyLatch' = readyLatch \ {n}
  /\ probeGreen' = probeGreen \ {n}
  /\ settled'    = settled \ {n}
  /\ extRed'     = extRed \ {n}
  /\ UNCHANGED <<dsDeleted, kubeEvents, probeReddened, relatched, budgetBroken>>
  /\ LegacyUnchanged

\* ---- 4. the FAILURE-DRIVEN trigger, which readiness does NOT pace. -------
\* manage() -> podsShouldBeOnNode -> syncNodes(createDiff): zero availability
\* accounting.  Carries the SAME composition-destroying clause, because a spot
\* reclaim kills the tgt exactly as a template roll does.
PodEvict(n) ==
  /\ KubeDsArm
  /\ PodTrigger = "evict"
  /\ PodPresent(n)
  /\ kubeEvents < MaxKubeEvents
  /\ podPhase' = [podPhase EXCEPT ![n] = "gone"]
  /\ dsDeleted' = dsDeleted \cup {n}
  /\ kubeEvents' = kubeEvents + 1
  /\ probeGreen' = probeGreen \ {n}
  /\ readyLatch' = readyLatch \ {n}
  /\ settled'    = settled \ {n}
  /\ extRed'     = extRed \ {n}
  /\ IF RaidLifetimeArm /\ n \in raidHosts
     THEN /\ raidHosts' = raidHosts \ {n}
          /\ serving' = {}
          /\ raidSeen' = DpSeenRehydrate
          /\ raidLostOnce' = TRUE
          /\ a2Created' = a2Created \ {n}
     ELSE UNCHANGED <<raidHosts, serving, raidSeen, raidLostOnce, a2Created>>
  /\ UNCHANGED <<oldRev, probeReddened, relatched, budgetBroken>>
  /\ UNCHANGED <<staged, stagedAt, vaNode, localLegs, relocating, adoptedA2,
                 validateRemoved, flapped>>
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim,
                 deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars
  /\ UNCHANGED expandVars
  /\ UNCHANGED gateVars
  /\ UNCHANGED bounceVars

\* ---- 5/6. THE PRODUCER: readiness reddened by a NON-data-path cause. -----
\* A csi-node pod is Ready only when all four containers are.  This is how
\* runaq made 2 of 4 pods NotReady on the SHIPPED chart.
ExtProbeRed(n) ==
  /\ KubeDsArm
  /\ PodPresent(n)
  /\ n \notin extRed
  /\ kubeEvents < MaxKubeEvents
  /\ extRed' = extRed \cup {n}
  /\ kubeEvents' = kubeEvents + 1
  /\ UNCHANGED <<podPhase, oldRev, probeGreen, readyLatch, settled, dsDeleted,
                 probeReddened, relatched, budgetBroken>>
  /\ LegacyUnchanged

\* The GREEN transition CLEARS settled: IsPodAvailable reads the Ready
\* condition's LastTransitionTime, so every red->green flip restarts the
\* minReadySeconds clock.  Clearing only at pod start would be bookkeeping
\* keyed on a REMEMBERED EVENT instead of the LIVE CONDITION, and would make
\* the MEASURED state (four healthy Ready=True pods, all deleted) unreachable.
ExtProbeGreen(n) ==
  /\ KubeDsArm
  /\ n \in extRed
  /\ extRed' = extRed \ {n}
  /\ settled' = settled \ {n}
  /\ UNCHANGED <<podPhase, oldRev, probeGreen, readyLatch, dsDeleted,
                 kubeEvents, probeReddened, relatched, budgetBroken>>
  /\ LegacyUnchanged

\* ---- 7. kubelet stores the verdict; the controller reads the STORED one. --
ProbeEval(n) ==
  /\ KubeDsArm
  /\ PodPresent(n)
  /\ LET g == ProbeVerdict(n) IN
       /\ (g <=> n \notin probeGreen)              \* fire only on CHANGE
       /\ probeGreen' = IF g THEN probeGreen \cup {n} ELSE probeGreen \ {n}
       /\ settled'    = IF g THEN settled \ {n} ELSE settled
       \* THE ANTI-CROSS-NODE TOOTH.  Single writer, stamped only when a
       \* probe reddens a pod whose tgt is UP.  Under selfLatch this is
       \* STRUCTURALLY unreachable — g is (n \in readyLatch), which only
       \* GROWS within an incarnation while its only clearers (PodStart,
       \* DsRollingUpdate, PodEvict) clear probeGreen in the SAME step, so
       \* the red direction of the change-guard is unsatisfiable.  Under
       \* "volume"/"selfLive" it fires in three steps.  A NAME-CHECKED
       \* INVARIANT, not a temporal property: the harness greps only
       \* "Temporal properties were violated" and names nothing.
       /\ probeReddened' = (probeReddened \/ (~g /\ TgtUp(n)))
  /\ UNCHANGED <<podPhase, oldRev, readyLatch, extRed, dsDeleted, kubeEvents,
                 relatched, budgetBroken>>
  /\ LegacyUnchanged

\* ---- 8. the latch's honest arm ------------------------------------------
ReadyOnRecovery(n) ==
  /\ KubeDsArm
  /\ ReadyScope = "selfLatch"
  /\ PodPresent(n)
  /\ n \notin readyLatch
  /\ SelfRecovered(n)
  /\ readyLatch' = readyLatch \cup {n}
  \* Stamped when a DELETED leg latches again, so the barrier probe names the
  \* ACTION (readmission ran) rather than the SITUATION (|dsDeleted| > 1).
  /\ relatched' = (relatched \/ (n \in dsDeleted /\ n \in Legs))
  /\ UNCHANGED <<podPhase, oldRev, probeGreen, settled, extRed, dsDeleted,
                 kubeEvents, probeReddened, budgetBroken>>
  /\ LegacyUnchanged

\* ---- 9. the grace expires and the pod latches green anyway ---------------
\* Unguarded, this is enabled in the very state PodStart produces, so the
\* "deadline" is TRUE, every socket-arm behaviour has a step-for-step
\* selfLatch counterpart, and no run can show the proposal beats shipping
\* nothing.  GraceExceedsRecovery = TRUE encodes "the configured grace
\* outlives readmission latency".
ReadyOnDeadline(n) ==
  /\ KubeDsArm
  /\ ReadyScope = "selfLatch"
  /\ PodPresent(n)
  /\ n \notin readyLatch
  /\ (GraceExceedsRecovery => ~WarmWaitingFor(n))
  /\ readyLatch' = readyLatch \cup {n}
  /\ UNCHANGED <<podPhase, oldRev, probeGreen, settled, extRed, dsDeleted,
                 kubeEvents, probeReddened, relatched, budgetBroken>>
  /\ LegacyUnchanged

\* ---- 10. the minReadySeconds window elapses ------------------------------
PodSettle(n) ==
  /\ KubeDsArm
  /\ MinReadySecsArm
  /\ PodPresent(n)
  /\ n \in probeGreen
  /\ n \notin settled
  /\ settled' = settled \cup {n}
  /\ UNCHANGED <<podPhase, oldRev, probeGreen, readyLatch, extRed, dsDeleted,
                 kubeEvents, probeReddened, relatched, budgetBroken>>
  /\ LegacyUnchanged

DsNext ==
  \/ TemplateBump
  \/ DsRollingUpdate
  \/ \E n \in Nodes :
       \/ PodStart(n)
       \/ PodEvict(n)
       \/ ExtProbeRed(n)
       \/ ExtProbeGreen(n)
       \/ ProbeEval(n)
       \/ ReadyOnRecovery(n)
       \/ ReadyOnDeadline(n)
       \/ PodSettle(n)

Next ==
  \/ (NextLegacy /\ UNCHANGED dsVars)
  \/ DsNext

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
       \* F61: the no-op drain pass is a pure roller tick (it drains
       \* nothing, so there is no operator-pacing decision in it). Fair,
       \* so the FIXED run actually reaches the interesting world rather
       \* than satisfying RollProcessedNodeRolls vacuously.
       /\ WF_vars(MaintDrainSkip(l))
       \* Surfacing a refusal is not optional: an operator cannot act on a
       \* refusal nobody reports, which is the whole reason maintSkipped
       \* exists rather than the refusal being a bare guard.
       /\ WF_vars(MaintRefuse(l))
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

\* The cutover orchestrator's own obligation, split out (the FairnessKubelet
\* pattern) so it applies ONLY to the runs that ask about the belt's liveness.
\* WF(Bounce) is the honest abstraction of a default-ON 60s timer-driven tick
\* (the same correction the 2026-07-29 audit applied to HotRejoin): once a
\* bounce is continuously plannable AND safe, the orchestrator eventually
\* issues it.  WF(BounceOverride) is the wall clock behind the refusal bound.
\* Deliberately NOT in FairnessCore: initiating a bounce is elsewhere treated
\* as the orchestrator's choice, and adding an obligation there would change
\* every existing bounce run's behaviour graph.
FairnessBounce == WF_vars(Bounce) /\ WF_vars(BounceOverride)

(***************************************************************************)
(* THE DETECTOR'S OWN OBLIGATION (F62a), and its absence was a hole in the  *)
(* module rather than a modelling choice.  AgentClear has had weak fairness *)
(* since the bounce tranche landed; AgentFlag never did.  So TLC was always *)
(* free to simply decline to flag, which means NO run in this module could  *)
(* ever have concluded that the data-path repair chain works — its liveness *)
(* was unfalsifiable in both directions.  The F62a A1 run failed for that   *)
(* reason before failing for a real one, which is how it was noticed.       *)
(*                                                                         *)
(* WF is the right strength and matches the code: detect_lost_data_paths is *)
(* a periodic node-agent loop that flags after CONSECUTIVE raid-missing     *)
(* strikes under a live attachment — eventual, not immediate — the same     *)
(* honest abstraction of a default-ON timer the 2026-07-29 audit applied to *)
(* HotRejoin and Bounce.  Split out rather than added to FairnessCore so    *)
(* the 52 pre-F62 runs keep their exact behavior graphs.                    *)
(***************************************************************************)
(* STRONG fairness, and the reason is worth recording because WF was tried  *)
(* first and failed for a fake reason.  Per-leg WF obligates only a leg      *)
(* CONTINUOUSLY enabled, so an environment that alternates which leg is      *)
(* blackholed leaves neither leg continuously flaggable and both WF          *)
(* conjuncts vacuous — TLC returned exactly that lasso (l1 up/l2 blackhole,  *)
(* flapping, composition gone, dpFlag never set).  Nothing about that trace  *)
(* is a reason the repair fails; it is the per-leg WF idiom being defeated   *)
(* by an adversary.  A periodic poller does fire against a flapping          *)
(* environment, which is what SF says: enabled infinitely often => occurs.   *)
(* Same justification the expansion tranche's SF upgrade carries.            *)
(*                                                                         *)
(* Residual scope limit, honestly: the flag is written by the agent on the   *)
(* RAID HOST, whose liveness is not any leg's Responsive() — and once the    *)
(* composition is destroyed raidHost is "none", so the model no longer names *)
(* the node whose agent is alive and looking.  AgentFlag stays per-leg (its  *)
(* shipped shape: dpFlag is "<node>|<since>") and SF stands in for "some     *)
(* live agent eventually looks".                                            *)
FairnessAgent == \A l \in Legs : SF_vars(AgentFlag(l))

(***************************************************************************)
(* THE ROLLER FINISHES WHAT IT STARTED.  WF on the drain itself, split out  *)
(* so only the mobility run carries it and no legacy behavior graph moves.  *)
(*                                                                         *)
(* The module has deliberately withheld this, on the grounds that "STARTING *)
(* each node's drain is operator-paced".  That argument is about the        *)
(* operator triggering a CAMPAIGN — the helm upgrade — and it silently      *)
(* widened into "the roller may abandon a node mid-campaign", which is not  *)
(* what the code does and not what runap did: with pods pending, the roller *)
(* drained one node per 60s tick with no human in the loop at any point.    *)
(*                                                                         *)
(* Needed here because RefusalEventuallyClears asks whether a node whose    *)
(* refusal LIFTED eventually rolls, and such a node is serving again, so it *)
(* must be drained before RollStart can fire.  Without drain fairness TLC   *)
(* answers by simply declining to drain — true of the spec, and nothing to  *)
(* do with the question.                                                   *)
(***************************************************************************)
FairnessMaintDrain == \A l \in Legs : WF_vars(MaintDrain(l))

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

\* The belt-liveness world: the orchestrator is a persistent retrier and the
\* refusal bound's clock advances.  Used only by the two BounceStarve/
\* BounceBounded runs.
SpecBounceLive == Init /\ [][Next]_vars /\ Fairness /\ FairnessBounce

\* The F62a repair world: the orchestrator bounces when a bounce is
\* continuously plannable AND the node agent's detector eventually fires.
\* Both halves are needed to ask "does repair A1 recover the composition?" —
\* without the detector's obligation the answer is always no, for a reason
\* that has nothing to do with the fix under test.
SpecRaidRepair ==
  Init /\ [][Next]_vars /\ Fairness /\ FairnessBounce /\ FairnessAgent

\* The consumer-mobility world: the roller finishes nodes it has started, so
\* "does a lifted refusal actually roll?" is answerable.  See
\* FairnessMaintDrain for why that obligation is faithful rather than
\* generous.
SpecConsumerMobile ==
  Init /\ [][Next]_vars /\ Fairness /\ FairnessMaintDrain

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
\* F62 strengthening: read SERVICE, not membership.  In every arm-off world
\* RaidPresent is invariantly TRUE, so this is literally the old theorem
\* there; under RaidLifetimeArm it additionally catches a roll that leaves
\* the members healthy and takes the composition — which is the shape the
\* old statement would have missed had the composition been destroyed with
\* `serving` left populated.  Cheap, and it removes the reliance on the two
\* halves happening to fall together.
Inv_PlannedRollNeverCausesOutage ==
  (crashes = 0 /\ ~relocating) => VolumeUp

(***************************************************************************)
(* THE KUBE-DS THEOREMS.  Neither carries a `KubeDsArm =>` antecedent:      *)
(* craft rule 3 says never condition an invariant on the arm it evaluates.  *)
(* With the arm FALSE both ghosts stay at their Init values and these hold  *)
(* trivially, WITHOUT being conditioned on the arm.                         *)
(***************************************************************************)

\* THE HARM.  A claim about a STEP — how many LIVE tgts died in one sync —
\* which is why it needs a ghost: a state invariant cannot see a step.
\* Scoped to TgtUp nodes because TgtDie leaves podPhase alone, so an
\* unscoped count would fire on a node the model is pretending is up.
Inv_DsBudgetNeverBroken == ~budgetBroken

\* THE ANTI-CROSS-NODE TOOTH, and the tranche's answer to "what stops the
\* next reviewer adding an in_sync term to the predicate?".  Under selfLatch
\* this is structurally unreachable; under "volume" or "selfLive" TLC finds
\* it in a handful of steps.  So a future cross-node predicate does not get
\* argued about — it FAILS THE GATE.
Inv_ProbeNeverReddensLive == ~probeReddened

\* SPDK's own coupling, as a checkable claim: with no composition there is
\* nothing for a base to be a member of (raid_bdev_deconfigure below the
\* operational floor, bdev_raid.c:2069-2074).  Only this direction is a
\* global theorem — see the VolumeUp comment for why the converse is a
\* recorded scope limit rather than an invariant.
Inv_RaidCompositionCoupled ==
  ~RaidPresent => serving = {}

(***************************************************************************)
(* THE A2 THEOREM.  At most one composition over these lvols exists,        *)
(* anywhere in the cluster, ever.                                          *)
(*                                                                         *)
(* This is the statement that was UNCHECKABLE for the whole F62/F63 cycle,  *)
(* because `raidHost` was a scalar and two hosts could not be written down. *)
(* `FlintReplicationRaidReconcile.cfg` went green against A2 with this      *)
(* hazard structurally invisible, and that green was then cited as "A2 is   *)
(* modelled" — which is the failure mode this codebase has now hit three    *)
(* times (the pod layer's two creators of the nfs pod; F62a's detector       *)
(* disabled exactly when it was needed; and this).                         *)
(*                                                                         *)
(* WHY CARDINALITY 2 IS HARMFUL, stated carefully, because the obvious     *)
(* claim is wrong.  It is NOT "two guests write the same lvols and          *)
(* diverge": at the moment A2 creates a phantom, that node has no consumer  *)
(* issuing I/O — the pod left, which is why the VA was stale — and a raid1  *)
(* with no opener is passive.  I wrote the divergence version here first    *)
(* and it does not survive reading the code.                               *)
(*                                                                         *)
(* The two harms that DO survive it:                                       *)
(*                                                                         *)
(*  1. THE PHANTOM IS A CONTROL-PLANE LIE, and it lies to the belt this     *)
(*     very cycle installed.  Fix C's barrier probes bdev_raid_get_bdevs    *)
(*     on the consumer and requires configured bases >= 1 (maint_roll.rs    *)
(*     gather_volume_maint).  A phantom answers that probe exactly like a   *)
(*     healthy composition.                                                *)
(*                                                                         *)
(*  2. A LATER NodeStage ADOPTS IT WITHOUT VALIDATION.  ensure_raid1_bdev   *)
(*     (driver.rs:3105) reuses any raid of that name in state "online" —    *)
(*     "already ONLINE (N base(s) configured) — reusing" — and compares     *)
(*     nothing against the base set NodeStage intended; the count it reads  *)
(*     goes into the log line and nowhere else.  raid_name derives from the *)
(*     volume handle alone, so the phantom is NAME-IDENTICAL to the raid    *)
(*     the real host needs.  If the consumer ever returns to that node it   *)
(*     inherits a composition whose members were chosen by a boot-time      *)
(*     snapshot of UpInSync taken while the volume was somebody else's.     *)
(*     The adopt-or-mint family (F44/F46) reached by a new road.            *)
(*                                                                         *)
(* So this is worth holding for control-plane integrity and a blind adopt,  *)
(* not for a simultaneous-writer story.  A concrete consequence for the     *)
(* implementation: if A2 lands, ensure_raid1_bdev's ONLINE-reuse must       *)
(* validate the base set first, or A2 must never leave a raid NodeStage     *)
(* could adopt.                                                            *)
(*                                                                         *)
(* Every creator other than A2 assigns a singleton, so a violation here is  *)
(* attributable to A2 by construction.                                     *)
(***************************************************************************)
Inv_SingleComposition ==
  Cardinality(raidHosts) <= 1

\* The sharper form, and the one that fails FIRST: no composition anywhere
\* except where the consumer actually is.
\*
\* The first draft of this read
\*   \A h \in raidHosts : h = vaNode \/ h = VaTruth
\* which is worthless — it admits exactly the split-brain state it was meant
\* to catch (the phantom satisfies h = vaNode, the real one satisfies
\* h = VaTruth, and the conjunction is happy).  The same shape of mistake as
\* Inv_MaintFenceHoldsUnderRefusal one tranche ago: an invariant written to
\* accommodate the situation instead of to judge it.
\*
\* Stated against truth alone, it catches a state Inv_SingleComposition
\* cannot: A2 assembling on a node the consumer has LEFT, while no other
\* composition exists yet.  Cardinality is 1 there, so the theorem above is
\* satisfied, and yet a raid is now open over these lvols on a node with no
\* consumer — waiting for the real host to stage and make it two.
Inv_A2AssemblesOnlyAtTruth ==
  RaidLifetimeArm => (\A h \in raidHosts : h = VaTruth)

(***************************************************************************)
(* THE ADOPT THEOREM.  NodeStage never inherits a composition that A2       *)
(* built.                                                                  *)
(*                                                                         *)
(* Attributable by construction: the ghost is stamped only by              *)
(* AssembleAdopt, and only when the object it short-circuits onto carries   *)
(* A2's provenance.  That matters because of the pod-layer tranche's rule — *)
(* a mutation run whose invariant is violable for reasons other than the    *)
(* mutation proves nothing about the mutation — and the harm-shaped         *)
(* alternatives here (a serving member the record does not vouch for) are   *)
(* all violable through doors this module already has.                     *)
(***************************************************************************)
Inv_NoAdoptOfA2Composition ==
  ~adoptedA2

(***************************************************************************)
(* THE ANTI-FLAP THEOREM, and the reason the validating fix is not simply   *)
(* "the safe option".                                                      *)
(*                                                                         *)
(* Its remedy is to DELETE the other creator's object. So the two creators  *)
(* can undo each other: A2 builds, NodeStage validates and deletes,        *)
(* NodeStage builds, the consumer moves away, A2 builds again on the same   *)
(* stale VA — a create/delete cycle that costs a real raid teardown on a    *)
(* node that then hosts a consumer, every time round.                      *)
(*                                                                         *)
(* Stated as REACHABILITY (violation = flapping is possible), because that  *)
(* is the form this module has learned to trust: no fairness argument, no   *)
(* environment noise, and the counterexample is the loop itself. A liveness *)
(* "the composition eventually stabilises" would have been defeated four    *)
(* times over by a flapping leg and an oscillating consumer, exactly as     *)
(* RefusalEventuallyClears was.                                            *)
(*                                                                         *)
(* A2 having built TWICE with at least one validate-delete in between is    *)
(* the signature: the second build cannot be the original one, and the      *)
(* delete is what made room for it.                                        *)
(***************************************************************************)
Inv_NoValidateFlap ==
  ~flapped

\* Consistency tooth for the pair of staging variables: `staged` is exactly
\* "kubelet believes this volume is staged SOMEWHERE".  Cheap, and it keeps a
\* later edit from letting the two drift apart silently — which is how the
\* scalar raidHost went unexamined for two tranches.
Inv_StagedAgrees ==
  staged <=> stagedAt # "none"

\* A2 must not become a second route into the phantom-assembly class that
\* "superblock": false exists to keep shut: it serves only what the records
\* vouch for, never a leg whose state is stale or unknown.  Holds on both
\* arms — it is a property of how AgentBootReconcile computes `serving'`,
\* not of the belt — and it is here so that a later, looser A2 (one that
\* assembles every RECORDED leg rather than every in_sync one) cannot land
\* without a tooth already in the gate waiting for it.
Inv_A2NeverServesUnvouched ==
  RaidReconcileArm => serving \subseteq (UpInSync \cup {l \in Legs : state[l] = "insync"})

(***************************************************************************)
(* IS THE REPAIR PATH REACHABLE AT ALL?  Deliberately an invariant whose    *)
(* VIOLATION is the good news — the Inv_NoStaleServe idiom, where the       *)
(* counterexample trace is the proof of reachability.                       *)
(*                                                                         *)
(* Asked this way on purpose, after three failed attempts to ask it as      *)
(* liveness.  "The composition always comes back" is genuinely FALSE in a   *)
(* world with real leg deaths, a flapping environment and a finite bounce   *)
(* budget, and each round of weakening the antecedent to dodge that noise   *)
(* moved the property closer to vacuous while answering nothing.  What      *)
(* actually distinguishes the shipped code from the fix is not how often    *)
(* recovery happens but whether it is POSSIBLE: with an empty              *)
(* data_path_raid_seen the detector cannot fire, so no interleaving         *)
(* whatsoever reaches a recovered state, and this invariant HOLDS —         *)
(* the bug, stated as unreachability.  Rehydrate the set and TLC returns a  *)
(* trace ending in a rebuilt composition: VIOLATED, which is repair A1      *)
(* working.  Immune to every fairness argument, because reachability does   *)
(* not depend on fairness at all.                                          *)
(***************************************************************************)
Inv_RaidRecoveryUnreachable ==
  ~(raidLostOnce /\ RaidPresent)

(***************************************************************************)
(* IS THE REFUSAL STICKY?  The Inv_NoStaleServe idiom again — a VIOLATION   *)
(* is the good news, and the counterexample trace is the proof that a node  *)
(* whose refusal lifted can actually roll.                                 *)
(*                                                                         *)
(* With the shipped gate (RefusalSticky = FALSE) TLC must FIND a state       *)
(* where a node that was refused, and whose consumer has since left, has    *)
(* rolled — the 14-second behaviour measured on runap.  With the remembered *)
(* gate (RefusalSticky = TRUE) no interleaving reaches it and this HOLDS:    *)
(* the refusal is permanent, the node keeps an old driver forever, and the   *)
(* operator's only recourse is to disable the feature.                      *)
(*                                                                         *)
(* Reachability, so it is immune to every fairness argument — which is the   *)
(* whole reason it replaced the liveness form.                              *)
(***************************************************************************)
Inv_RefusalNeverClears ==
  ~(\E l \in Legs : l \in maintSkipped /\ l \notin localLegs /\ l \in rolled)

\* WHAT FIX B BUYS: the fence with NO carve-out.  Inv_MaintFenceHolds has to
\* exclude LocalLegs from its scope, because the post-F61 roller restarts a
\* local half's tgt while that leg is serving — the fence provably cannot
\* hold for those legs.  Refusing them instead restores full strength, and
\* this is the statement that can tell the two apart.
\*
\* Stated UNCONDITIONALLY on purpose.  The first draft read
\*   (MaintFence /\ MaintLocalRefuse) => serving \cap rolling = {}
\* which is worthless: conditioning an invariant on the very arm it is
\* meant to evaluate makes it vacuous on the bug side, so it could never
\* fail and would have looked like a passing tooth forever.  The same trap
\* as a canary that only fires with the fix on.
Inv_MaintFenceStrict ==
  MaintFence => serving \cap rolling = {}

\* The fence, as an invariant: a serving leg's tgt is never down for a
\* planned restart.  Under MaintFence the suppression mark gates
\* RollStart, a suppressed leg is out of serving (drained) and cannot
\* re-enter (HotRejoin/Admit/Assemble/LastResortServe all refuse), so
\* the intersection stays empty — checked in the strict maintenance run.
\*
\* F61 SCOPE, and it is not a weakening for convenience — the model
\* PROVED the two halves are coupled.  LocalLegs are legs whose drain is
\* deliberately skipped (consumer == node): maint_roll.rs emits
\* MaintenanceLocalConsumer and restarts that tgt anyway, by design, so
\* such a leg IS serving while rolling.  When the F61 fix let RollStart
\* fire for a processed-but-unmarked node, this invariant failed at once
\* — which is the model saying: you cannot fix the campaign-progress bug
\* without either (a) accepting a fence violation on local-half nodes,
\* or (b) implementing the local half (staged-device continuity /
\* ublk user-recovery).  The design doc treated "orchestration half" and
\* "local half" as independent workstreams; they are not.  We take (a)
\* consciously — the gap is documented, warned per volume at runtime, and
\* the alternative is a DaemonSet that can never converge.  Excluding
\* LocalLegs keeps the invariant's TEETH for every remotely-consumed leg,
\* which is where the fence is the whole point.
Inv_MaintFenceHolds ==
  MaintFence => (serving \ localLegs) \cap rolling = {}

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
\* Same `~relocating` exemption as the outage theorem above, for the same
\* reason: this bounds what MAINTENANCE costs, and a consumer reschedule is
\* an external event that legitimately takes every leg out of service at once
\* (the composition is gone until the new host stages).  Without the
\* exemption the invariant fires on state 2 of any mobility run and says
\* nothing about the roll.  RelocationWindowCloses carries the obligation the
\* exemption would otherwise drop.
Inv_PlannedRollBoundedImpact ==
  (crashes = 0 /\ ~relocating) => Cardinality(Legs \ serving) <= 1

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
(* THE BELT'S OWN LIVENESS — the property whose ABSENCE the 2026-07-29 code  *)
(* review exposed.  BouncePreflight is a GUARD, so a blocked bounce is       *)
(* merely a DISABLED ACTION, and nothing in this module ever asked whether   *)
(* the remediation it blocks ever happens.  That is why an unbounded belt    *)
(* was invisible here and had to be found by reading code.                   *)
(*                                                                          *)
(* NOTE WHAT THIS DOES **NOT** SAY, because the first draft said it and was  *)
(* wrong: not "the data-path flag eventually clears".  Clearing it needs the *)
(* flagged leg back in a serving assembly, which needs an ADMISSION          *)
(* (catch-up, the claim, then Admit) — machinery the bounce does not own and *)
(* whose own convergence is a separate theorem.  A flag-clearing property    *)
(* therefore has a lasso in BOTH worlds and cannot isolate the belt, the     *)
(* same defect the BouncePlanner canary had before it got its own ghost.     *)
(*                                                                          *)
(* What the belt actually blocks is the TEARDOWN, so that is what this       *)
(* states: a flagged volume that is still serving eventually either gets its *)
(* remediation attempted (serving driven to {}) or stops needing it.  With   *)
(* an unbounded belt and a writer that oscillates unavailable/available —    *)
(* LegRecover is the environment's choice and carries no fairness, so        *)
(* WF(LegPerish) never obligates the leg to die and it never becomes         *)
(* honestly excusable — BounceSafe is only INTERMITTENTLY true, which weak   *)
(* fairness never obligates.  That is the flapping-kubelet case, faithfully. *)
(***************************************************************************)
\* The `consecutiveBounces < MaxBounces` conjunct is the BUDGET caveat, and it
\* is load-bearing: MaxBounces bounds the state space (as GenBound bounds
\* incarnations), so once it is spent NO bounce can fire and any "a bounce
\* eventually happens" property is false by construction — the bounded run's
\* first lasso was exactly that artifact, cycling HotRejoin/Acquire/Release
\* with the budget exhausted.  Conditioning on remaining budget is the same
\* move the roll invariants make with `crashes = 0`: the theorem is about the
\* modeled world, and says so.
RemediationNotStarved ==
  (dpFlag # "none" /\ serving # {} /\ consecutiveBounces < MaxBounces)
    ~> (serving = {} \/ dpFlag = "none")

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
(* THE ROLL-PROGRESS THEOREM (F61, found LIVE 2026-07-30 — drill 3.14's    *)
(* first run — because NO property in this module could fail on it).        *)
(*                                                                         *)
(* Why the gap existed, exactly: the fairness comment above states that    *)
(* "the campaign completes" is deliberately NOT a theorem, since STARTING  *)
(* each node's drain is operator-paced.  That reasoning is right about     *)
(* pacing and wrong about its blast radius — it also erased any way to     *)
(* distinguish a campaign that has not STARTED from one that CANNOT        *)
(* FINISH.  A wedged roll leaves the volume perfectly healthy, so          *)
(* EventuallyServingAgain, EventuallyWritable and AdmissionNotStarved all  *)
(* hold; and MaintenanceEventuallyLifts holds VACUOUSLY, because the       *)
(* wedge never mints a mark for the stuck node.  Four green properties, a  *)
(* roll that never converges.                                              *)
(*                                                                         *)
(* This property obligates the ROLLER, not the operator: it says nothing   *)
(* about whether a campaign begins, only that once the roller has          *)
(* PROCESSED a node — run its drain pass, marked or not — that node        *)
(* eventually rolls.  With MaintProcessedGate FALSE (the shipped           *)
(* predicate: RollStart needs a MARK) a LocalLegs node is processed by     *)
(* MaintDrainSkip and can never be rolled: TLC returns the livelock.       *)
(* Run with MaxCrashes = 0 — RollStart needs legUp = "up", so a real       *)
(* failure legitimately stalls a roll and would mask the bug.              *)
(***************************************************************************)
(* F62 amendment.  A REFUSAL is a legitimate terminal outcome — but only     *)
(* because it is recorded where an operator can act on it.  If maintSkipped  *)
(* were dropped from this disjunction the property would still pass, and     *)
(* would then be blind to a roller that silently gave up: F61's livelock     *)
(* with better manners.  The disjunct is the whole reason the variable       *)
(* exists rather than the refusal being a bare guard.                        *)
RollProcessedNodeRolls ==
  \A l \in Legs : [](l \in processed => <>(l \in rolled \/ l \in maintSkipped))

(***************************************************************************)
(* THE REFUSAL IS NOT TERMINAL (2026-07-29).  The property the F62 tranche  *)
(* should have carried and did not.                                        *)
(*                                                                         *)
(* RollProcessedNodeRolls above accepts `maintSkipped` as a final answer,   *)
(* which was fine as a statement that the campaign converges and useless as *)
(* a statement about what happens NEXT.  With LocalLegs constant it could   *)
(* not have been otherwise: a refused node was refused in every reachable   *)
(* state, so "refuses forever" and "re-examines every tick" were            *)
(* indistinguishable — and the shipped code does the second while the model *)
(* only demanded the first.  A model weaker than its implementation is the  *)
(* direction that lets a regression land silently: someone could replace    *)
(* the per-tick recompute with a remembered set and every run would stay    *)
(* green.                                                                  *)
(*                                                                         *)
(* Live evidence for the claim: 14 seconds after the NFS server left the    *)
(* refused node on runap, the roller rolled it, unprompted, because         *)
(* local_consumer_nodes is rebuilt from the gather on every tick.  The      *)
(* obligation below is exactly that, and nothing more: a node refused for a *)
(* reason that has since GONE AWAY must eventually roll.  It says nothing   *)
(* about a node whose consumer never moves — that one legitimately waits.   *)
(***************************************************************************)
\* Stated as eventually-ALWAYS, not as "whenever".  The `whenever` form
\* ([](refused /\ not-local => <>rolled)) is too strong and TLC says so with a
\* fair counterexample that has nothing to do with the roller: the consumer
\* OSCILLATES, leaving a node just long enough for the obligation to attach and
\* returning before the roll can finish, forever.  RelocateConsumer carries no
\* fairness — correctly, it is the environment — so no implementation can win
\* that race, and demanding it would be demanding the impossible.
\*
\* What is actually claimed, and what runap demonstrated in 14 seconds: if the
\* consumer eventually stays off a node for good, that node eventually rolls.
\* A node the consumer never leaves is legitimately never rolled, which is the
\* refusal doing its job.
\* NOT GATED BY ANY CFG, and kept only as a record of what was tried.  Every
\* form of this liveness statement measures the ENVIRONMENT rather than the
\* roller, because progress here can be blocked by things the roller must
\* respect: a dead leg, the freshness gate's correct Defer, a consumer that
\* oscillates (RelocateConsumer is unfair, correctly — it is the world), and
\* finally the drain belt refusing to drain the LAST serving member while the
\* other leg is a standby.  Each dodge widened the antecedent toward vacuity.
\* The third time that happened in one session was enough: the question is not
\* "does it always roll" but "CAN a cleared refusal ever roll", which is
\* reachability, needs no fairness at all, and is stated below.
RefusalEventuallyClears ==
  \A l \in Legs :
    <>[](l \notin localLegs /\ legUp[l] = "up") => <>(l \in rolled)

(***************************************************************************)
(* THE RELOCATION WINDOW IS BOUNDED.  The other half of exempting           *)
(* `relocating` from the maintenance theorem — an exemption with no          *)
(* obligation attached would be a licence for the window to stay open        *)
(* forever, which is F62 again in a costume (a volume down with every leg    *)
(* healthy).  Same death/gate escapes as RaidEventuallyReassembled, for the  *)
(* same reasons: no assembly can be demanded out of dead or stale-only       *)
(* material, and the freshness gate's Defer is correct behavior.             *)
(***************************************************************************)
RelocationWindowCloses ==
  [](relocating /\ UpInSync # {} /\ writerSet \subseteq UpInSync => <>VolumeUp)

(***************************************************************************)
(* THE COMPOSITION-LIFETIME THEOREM (F62).  A volume kubelet believes       *)
(* STAGED eventually has a raid composition again.  This is the property    *)
(* whose absence made F62 invisible: the F62 state trips no safety          *)
(* invariant forever-after (serving = {} is reachable in a dozen benign     *)
(* ways) and every other liveness property is satisfied by a volume that    *)
(* is merely quiet.  What is wrong is specifically that the ONE creator is  *)
(* disabled while the ONE thing it creates is missing — a liveness claim,   *)
(* and nothing but a liveness claim can catch it.                           *)
(*                                                                         *)
(* Under RaidLifetimeArm with neither repair armed, TLC must FIND the       *)
(* violation.  With DpSeenRehydrate (repair A1: the detector's HashSet      *)
(* rehydrated from the staged set, so the existing data-path-lost -> bounce *)
(* -> restage chain fires) or RaidReconcileArm (repair A2: the agent        *)
(* re-creates it directly on boot) it must HOLD.  Which of the two          *)
(* suffices, and under which world, is exactly what this tranche is for —   *)
(* A1 reuses shipped machinery but routes through an outage-shaped bounce   *)
(* and a controller sweep gated on is_rwx, while A2 needs new code but is   *)
(* local, quiet and works for RWO.                                         *)
(***************************************************************************)
\* The DEATH ESCAPE, the same one MaintenanceEventuallyLifts carries.  The
\* first draft omitted it and TLC produced a perfectly fair counterexample
\* that had nothing to do with F62: the only in-sync leg dead, the only live
\* leg stale.  No re-creation path can assemble a composition out of that —
\* only the LastResortServe runbook can, and that is an operator step by
\* design.  UpInSync # {} says "there is live, in-sync material to build
\* from", which is precisely the precondition every repair assumes.
\* ...and the FRESHNESS-GATE escape, for the same reason.  The second draft
\* still failed on a trace where the repair chain worked perfectly — the
\* bounce fired, staged cleared — and then Assemble was held by the F36c gate
\* because a RECORDED writer was missing with no evidence of its death.  That
\* Defer is correct behavior, the module's documented "Deferred liveness
\* escape", and a re-creation path is not entitled to override it.
\* writerSet \subseteq UpInSync is exactly the condition under which the
\* gate's own first disjunct passes, so the antecedent now says: when nothing
\* LEGITIMATELY blocks a reassembly, the composition must come back.  The F62
\* state satisfies it fully — every leg alive, in-sync, and in the writer set
\* — so the teeth are untouched.
RaidEventuallyReassembled ==
  [](staged /\ ~RaidPresent /\ UpInSync # {} /\ writerSet \subseteq UpInSync
       => <>RaidPresent)

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
