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
(* step (the code itself Defers; an operator may serve the freshest        *)
(* stale survivor with the risk surfaced) — verified sound after the       *)
(* override.  Content-level snapshot semantics (epoch deltas, walk         *)
(* order, retention relink) live in FlintSnapshots.tla, where they are     *)
(* non-trivial; here content is a write-set and epochCut a single cut.     *)
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
  BarrierRaidAware \* TRUE = the barrier reads GROUND TRUTH (raid membership
               \* + responsiveness); FALSE = the record only (all replicas
               \* insync) — the shortcut the implementation actually takes.
               \* The record can lag the raid by one monitor tick, so the
               \* weaker barrier can drain into a race with an undetected
               \* failure; the RecordBarrier strict run verifies that costs
               \* availability (an honest Defer), never safety.

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
  \* ---- control plane (the k8s record; each action = one CAS) ------------
  state,         \* [Legs -> {"insync", "stale", "standby"}]
  writerSet,     \* SUBSET Legs — recorded serving-assembly membership
  epochCut,      \* SUBSET 1..MaxWrites — content captured at the last cut
  claim,         \* {"none", "catchup", "admission"} — the R2 volume claim
  crashes,       \* failure budget spent
  \* ---- planned maintenance (the csi-node roll) ---------------------------
  rolling,       \* SUBSET Legs: node whose tgt is down for a PLANNED restart
  rolled,        \* SUBSET Legs: nodes already rolled this campaign (monotone)
  suppress,      \* SUBSET Legs: readmission suppressed (the maintenance mark)
  rollerDead     \* TRUE once the roll orchestrator died mid-campaign

vars == <<serving, zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
          lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes,
          rolling, rolled, suppress, rollerDead>>

maintVars == <<rolling, rolled, suppress, rollerDead>>

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
  /\ crashes \in 0..MaxCrashes
  /\ rolling \subseteq Legs
  /\ rolled \subseteq Legs
  /\ suppress \subseteq Legs
  /\ rollerDead \in BOOLEAN

\* A leg's data path answers: its node is up AND its tgt is not down for a
\* planned restart.  The raid cannot tell the two apart — that symmetry is
\* the whole landmine, and every data-plane guard below uses this, not
\* legUp alone.  With MaintEnabled = FALSE, rolling = {} always and this
\* reduces to the old legUp = "up".
Responsive(l) == legUp[l] = "up" /\ l \notin rolling

UpInSync == {l \in Legs : state[l] = "insync" /\ Responsive(l)}

\* SPDK examine over an attached set: only the newest generation serves.
NewestOf(A) == {l \in A : \A m \in A : legGen[l] >= legGen[m]}

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
    /\ l \notin suppress
    /\ epochCut \subseteq legData[l]
    /\ serving # {}
    /\ \E src \in serving :
         /\ Responsive(src)
         /\ (RejoinGuard => legData[l] \subseteq legData[src])

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
  /\ crashes = 0
  /\ rolling = {}
  /\ rolled = {}
  /\ suppress = {}
  /\ rollerDead = FALSE

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

\* Verified death straight away (terminated AND observed so).
LegDie(l) ==
  /\ legUp[l] = "up"
  /\ crashes < MaxCrashes
  /\ legUp' = [legUp EXCEPT ![l] = "dead"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars

\* Silent unreachability: maybe a dying node, maybe a transient partition.
LegBlackhole(l) ==
  /\ legUp[l] = "up"
  /\ crashes < MaxCrashes
  /\ legUp' = [legUp EXCEPT ![l] = "blackhole"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars

\* The transient case: the leg returns, data intact, whatever the record
\* now says about it.  (The F36c ingredient.)
LegRecover(l) ==
  /\ legUp[l] = "blackhole"
  /\ legUp' = [legUp EXCEPT ![l] = "up"]
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

\* GROUND TRUTH: the silently-unreachable node actually dies (the cloud
\* reaped it).  WF here is the axiom that a blackhole eventually RESOLVES
\* — it perishes or it recovers; it does not hang forever.
LegPerish(l) ==
  /\ legUp[l] = "blackhole"
  /\ legUp' = [legUp EXCEPT ![l] = "dead"]
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

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

\* The whole assembly dies cleanly (process gone, connections dropped).
ServerCrash ==
  /\ serving # {}
  /\ crashes < MaxCrashes
  /\ serving' = {}
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk>>
  /\ UNCHANGED maintVars

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

\* Epoch scheduler: cut a consistent snapshot of the served content.
EpochCut ==
  /\ serving # {}
  /\ \A l \in serving : Responsive(l)
  /\ epochCut' = acked
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet, claim,
                 deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

\* replica_replace + prune_writers_for_replacement: swap the identity of a
\* stale leg whose node the record DEEMS dead (the C2 justification is
\* EVIDENCE, not an oracle); the freed slot returns as an empty standby
\* with a new identity on a fresh node (so legUp resets to "up" and the
\* deemed flag is cleared — it referred to the old identity's node).
\* When the evidence was FALSE, the old node later resurrects with the
\* old identity's lvol and exports intact — the F44-F46/F49 residue
\* family, fixed live by the teardown/identity-domain work and below
\* this abstraction; the record-level machine stays safe because the old
\* identity is no longer referenced anywhere.
Replace(l) ==
  /\ state[l] = "stale"
  /\ l \in deemedDead
  /\ l \notin rolling                     \* no identity swap mid-restart
  /\ UpInSync # {}                        \* something to rebuild from
  /\ legUp' = [legUp EXCEPT ![l] = "up"]
  /\ legData' = [legData EXCEPT ![l] = {}]
  /\ legGen' = [legGen EXCEPT ![l] = 0]
  /\ state' = [state EXCEPT ![l] = "standby"]
  /\ writerSet' = writerSet \ {l}
  /\ deemedDead' = deemedDead \ {l}
  \* The slot's new identity lives on a FRESH node: the old node's roll
  \* flags do not apply to it (and the fresh node has not been rolled).
  /\ rolled' = rolled \ {l}
  /\ suppress' = suppress \ {l}
  /\ UNCHANGED <<serving, zombie, raidGen, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, claim, falseRisk, crashes,
                 rolling, rollerDead>>

\* hot_rejoin_volume: a stale leg on a LIVE node re-enters as a standby
\* KEEPING its identity and payload (contrast Replace).  Whether the
\* payload is usable is decided downstream — by the RejoinGuard ancestry
\* check at catch-up/admission, or by Scrub when it diverges.
HotRejoin(l) ==
  /\ state[l] = "stale"
  /\ Responsive(l)
  /\ l \notin suppress                    \* the maintenance mark: no re-entry
  /\ state' = [state EXCEPT ![l] = "standby"]
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, writerSet, epochCut,
                 claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

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
  /\ legData' = [legData EXCEPT ![l] = legData[l] \cup epochCut]
  /\ UNCHANGED <<serving, zombie, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

\* Admission (hot-rejoin window / cutover reassembly) + mark_in_sync's
\* writer-set add: quiesced delta copy from a healthy serving survivor,
\* then join the raid at its current incarnation.  The ancestry check is
\* re-run HERE against the actual serving source — the admission-window
\* verification — because the world may have reassembled since catch-up.
Admit(l) ==
  /\ claim = "admission"                  \* the window holds its claim
  /\ state[l] = "standby"
  /\ Responsive(l)
  /\ l \notin suppress                    \* the second door (mirrors RejoinGuard)
  /\ epochCut \subseteq legData[l]        \* warm standby (caught up)
  /\ serving # {}
  /\ \E src \in serving :
       /\ Responsive(src)
       /\ (RejoinGuard => legData[l] \subseteq legData[src])
       /\ legData' = [legData EXCEPT ![l] = legData[l] \cup legData[src]]
  /\ serving' = serving \cup {l}
  /\ legGen' = [legGen EXCEPT ![l] = raidGen]
  /\ state' = [state EXCEPT ![l] = "insync"]
  /\ writerSet' = writerSet \cup {l}
  /\ claim' = "none"                      \* mark_in_sync closes the window
  /\ UNCHANGED <<zombie, legUp, raidGen, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

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
  /\ \E A \in (SUBSET UpInSync) \ {{}} :
       /\ \/ /\ writerSet \subseteq A
             /\ UNCHANGED <<riskSurfaced, falseRisk>>
          \/ /\ GateStrict
             /\ writerSet \ A # {}
             /\ \A w \in writerSet \ A : w \in deemedDead
             /\ riskSurfaced' = TRUE
             \* The harm ghost: excusing a writer that was NOT truly
             \* dead makes the surfaced risk hollow — the acked tail was
             \* recoverable all along.
             /\ falseRisk' = (falseRisk \/ \E w \in writerSet \ A : legUp[w] # "dead")
          \/ /\ ~GateStrict
             /\ UNCHANGED <<riskSurfaced, falseRisk>>
       /\ serving' = NewestOf(A)
       /\ writerSet' = NewestOf(A)
       /\ lineage' = UNION {legData[m] : m \in NewestOf(A)}
       /\ legGen' = [m \in Legs |-> IF m \in NewestOf(A) THEN raidGen + 1
                                                         ELSE legGen[m]]
  /\ raidGen' = raidGen + 1
  /\ zombie' = IF FenceZombie THEN {} ELSE zombie
  /\ UNCHANGED <<legData, legUp, acked, nextWrite, state, epochCut, claim,
                 deemedDead, crashes>>
  /\ UNCHANGED maintVars

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
  /\ serving' = {l}
  /\ writerSet' = {l}
  /\ lineage' = legData[l]
  /\ legGen' = [legGen EXCEPT ![l] = raidGen + 1]
  /\ raidGen' = raidGen + 1
  /\ riskSurfaced' = TRUE
  /\ state' = [state EXCEPT ![l] = "insync"]
  /\ zombie' = IF FenceZombie THEN {} ELSE zombie
  /\ UNCHANGED <<legData, legUp, acked, nextWrite, epochCut, claim, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

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

AcquireCatchup ==
  /\ claim = "none"
  /\ (ClaimArb => ~WarmWaiting)           \* the F43 yield rule
  /\ claim' = "catchup"
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

ReleaseCatchup ==
  /\ claim = "catchup"
  /\ claim' = "none"
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

AcquireAdmission ==
  /\ claim = "none"
  /\ WarmWaiting                          \* something to admit
  /\ claim' = "admission"
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, deemedDead, falseRisk, crashes>>
  /\ UNCHANGED maintVars

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
  \* DATA-PLANE BELT, unconditional: never drain the last serving member.
  \* Found by the RecordBarrier run: with a record-only barrier, a
  \* deconfigured-but-not-yet-stale-marked survivor makes the record lie
  \* ("both insync") and a record-level last-leg check passes — the drain
  \* then removes the SOLE serving leg holding the acked tail and prunes
  \* it from the writer set: silent loss in 7 states.  The belt must read
  \* GROUND TRUTH (the code probes the raid BEFORE the record round).
  /\ serving \ {l} # {}
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
                 crashes, rolling, rolled, rollerDead>>

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
                 rolled, suppress, rollerDead>>

\* kubelet completes the restart — weakly fair, roller-independent.
RollFinish(l) ==
  /\ rolling = {l}
  /\ rolling' = {}
  /\ rolled' = rolled \cup {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 suppress, rollerDead>>

MaintClear(l) ==
  /\ ~rollerDead
  /\ l \in suppress
  /\ l \in rolled                         \* restart done for this node
  /\ suppress' = suppress \ {l}
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet,
                 epochCut, claim, deemedDead, falseRisk, crashes,
                 rolling, rolled, rollerDead>>

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
                 rolling, rolled, suppress>>

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
                 rolling, rolled, rollerDead>>

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
  \/ ExpireClaim
  \/ RollerDie
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
  /\ WF_vars(Assemble)
  /\ WF_vars(EpochCut)
  /\ WF_vars(AcquireCatchup)
  /\ WF_vars(ReleaseCatchup)
  /\ WF_vars(AcquireAdmission)

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
Inv_NoSilentLoss ==
  (serving # {} /\ ~riskSurfaced) =>
    \A l \in serving : acked \subseteq legData[l]

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
    \A l \in serving : legData[l] \subseteq lineage

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
Inv_NoFalseRisk == ~falseRisk

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

Inv == TypeOK /\ Inv_NoSilentLoss /\ Inv_InsyncServingIsCurrent
             /\ Inv_ServingCurrentGen /\ Inv_NoDivergentServing
             /\ Inv_EvidenceSound /\ Inv_NoFalseRisk

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

\* State-space bound for TLC (raidGen grows with deconfigures/assemblies,
\* bounded by the crash budget plus the roll campaign — each node rolls
\* at most once, and a roll adds at most a drain bump, a deconfigure
\* bump (unfenced), and a reassembly bump).
GenBound == raidGen <= (3 * MaxCrashes) + (3 * Cardinality(Legs)) + 3

================================================================================
