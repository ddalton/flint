--------------------------- MODULE FlintReplication ---------------------------
(***************************************************************************)
(* The flint replica-lifecycle / writer-set machine — the durability core  *)
(* every orchestrator mutates.  This is the machine whose design-level     *)
(* bugs each cost a live campaign to find: F36c (assembly without a        *)
(* transiently-absent writer-set leg = the 6-write-tail loss), the C2 pin  *)
(* (writer-set exits only via stale-mark / replacement / assembly stamp),  *)
(* and P4 (omission failures stall writes until DETECTED).                 *)
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
(* GateStrict = FALSE is the pre-F36c bug (no gate, nothing surfaced).     *)
(* FlintReplicationF36c.cfg runs it and scripts/check-tla.sh REQUIRES      *)
(* that run to find the loss — the model must be able to rediscover the    *)
(* bug class it exists for.                                                *)
(*                                                                         *)
(* Out of scope this tranche: the F48 zombie / two concurrent assemblies   *)
(* (needs per-process views), hot-rejoin esnap window internals, epoch     *)
(* chains deeper than one cut, the stale-only-survivor catastrophe path,   *)
(* identity domains (killed at compile time by the newtypes).              *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Legs,        \* replica slots, e.g. {"l1", "l2", "l3"}
  MaxWrites,   \* bound on the write counter
  MaxCrashes,  \* bound on failure events (die/blackhole/server crash)
  GateStrict   \* TRUE = F36c gate on; FALSE = pre-F36c bug (must fail)

VARIABLES
  \* ---- data plane -------------------------------------------------------
  serving,       \* SUBSET Legs: legs configured in the serving raid; {} = down
  legData,       \* [Legs -> SUBSET 1..MaxWrites]
  legUp,         \* [Legs -> {"up", "blackhole", "dead"}]
  raidGen,       \* current raid incarnation (bumped on deconfigure/assemble)
  legGen,        \* [Legs -> Nat]: newest incarnation each leg participated in
  acked,         \* SUBSET 1..MaxWrites
  nextWrite,
  riskSurfaced,  \* TRUE once a ServeWithRisk assembly was chosen
  \* ---- control plane (the k8s record; each action = one CAS) ------------
  state,         \* [Legs -> {"insync", "stale", "standby"}]
  writerSet,     \* SUBSET Legs — recorded serving-assembly membership
  epochCut,      \* SUBSET 1..MaxWrites — content captured at the last cut
  crashes        \* failure budget spent

vars == <<serving, legData, legUp, raidGen, legGen, acked, nextWrite,
          riskSurfaced, state, writerSet, epochCut, crashes>>

TypeOK ==
  /\ serving \subseteq Legs
  /\ legData \in [Legs -> SUBSET (1..MaxWrites)]
  /\ legUp \in [Legs -> {"up", "blackhole", "dead"}]
  /\ raidGen \in Nat
  /\ legGen \in [Legs -> Nat]
  /\ acked \subseteq 1..MaxWrites
  /\ nextWrite \in 1..(MaxWrites + 1)
  /\ riskSurfaced \in BOOLEAN
  /\ state \in [Legs -> {"insync", "stale", "standby"}]
  /\ writerSet \subseteq Legs
  /\ epochCut \subseteq 1..MaxWrites
  /\ crashes \in 0..MaxCrashes

UpInSync == {l \in Legs : state[l] = "insync" /\ legUp[l] = "up"}

\* SPDK examine over an attached set: only the newest generation serves.
NewestOf(A) == {l \in A : \A m \in A : legGen[l] >= legGen[m]}

Init ==
  /\ serving = Legs
  /\ legData = [l \in Legs |-> {}]
  /\ legUp = [l \in Legs |-> "up"]
  /\ raidGen = 1
  /\ legGen = [l \in Legs |-> 1]
  /\ acked = {}
  /\ nextWrite = 1
  /\ riskSurfaced = FALSE
  /\ state = [l \in Legs |-> "insync"]
  /\ writerSet = Legs
  /\ epochCut = {}
  /\ crashes = 0

(***************************************************************************)
(* Data plane                                                              *)
(***************************************************************************)

\* Synchronous mirror: an ack requires the write on EVERY serving leg, all
\* responsive.  A blackholed serving leg stalls writes — the P4 150-177s
\* ledger stall, observed live.
Write ==
  /\ serving # {}
  /\ nextWrite <= MaxWrites
  /\ \A l \in serving : legUp[l] = "up"
  /\ legData' = [l \in Legs |->
                   IF l \in serving THEN legData[l] \cup {nextWrite}
                                    ELSE legData[l]]
  /\ acked' = acked \cup {nextWrite}
  /\ nextWrite' = nextWrite + 1
  /\ UNCHANGED <<serving, legUp, raidGen, legGen, riskSurfaced, state,
                 writerSet, epochCut, crashes>>

\* Verified death straight away (terminated AND observed so).
LegDie(l) ==
  /\ legUp[l] = "up"
  /\ crashes < MaxCrashes
  /\ legUp' = [legUp EXCEPT ![l] = "dead"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, legData, raidGen, legGen, acked, nextWrite,
                 riskSurfaced, state, writerSet, epochCut>>

\* Silent unreachability: maybe a dying node, maybe a transient partition.
LegBlackhole(l) ==
  /\ legUp[l] = "up"
  /\ crashes < MaxCrashes
  /\ legUp' = [legUp EXCEPT ![l] = "blackhole"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, legData, raidGen, legGen, acked, nextWrite,
                 riskSurfaced, state, writerSet, epochCut>>

\* The transient case: the leg returns, data intact, whatever the record
\* now says about it.  (The F36c ingredient.)
LegRecover(l) ==
  /\ legUp[l] = "blackhole"
  /\ legUp' = [legUp EXCEPT ![l] = "up"]
  /\ UNCHANGED <<serving, legData, raidGen, legGen, acked, nextWrite,
                 riskSurfaced, state, writerSet, epochCut, crashes>>

\* The permanent case, confirmed: node object gone / instance verified
\* terminated.  This — and only this — is the evidence Replace accepts.
ConfirmDead(l) ==
  /\ legUp[l] = "blackhole"
  /\ legUp' = [legUp EXCEPT ![l] = "dead"]
  /\ UNCHANGED <<serving, legData, raidGen, legGen, acked, nextWrite,
                 riskSurfaced, state, writerSet, epochCut, crashes>>

\* The data plane faults an unresponsive leg out; survivors continue at a
\* NEW incarnation (their superblocks record the shrink).  WF on this
\* action IS the P4 fix (TCP_USER_TIMEOUT + fast_io_fail bound detection).
RaidDeconfigure(l) ==
  /\ l \in serving
  /\ legUp[l] # "up"
  /\ serving' = serving \ {l}
  /\ raidGen' = raidGen + 1
  /\ legGen' = [m \in Legs |-> IF m \in serving \ {l} THEN raidGen + 1
                                                      ELSE legGen[m]]
  /\ UNCHANGED <<legData, legUp, acked, nextWrite, riskSurfaced, state,
                 writerSet, epochCut, crashes>>

\* The whole assembly dies (server node loss / bounce teardown).
ServerCrash ==
  /\ serving # {}
  /\ crashes < MaxCrashes
  /\ serving' = {}
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<legData, legUp, raidGen, legGen, acked, nextWrite,
                 riskSurfaced, state, writerSet, epochCut>>

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
  /\ UNCHANGED <<serving, legData, legUp, raidGen, legGen, acked, nextWrite,
                 riskSurfaced, epochCut, crashes>>

\* Epoch scheduler: cut a consistent snapshot of the served content.
EpochCut ==
  /\ serving # {}
  /\ \A l \in serving : legUp[l] = "up"
  /\ epochCut' = acked
  /\ UNCHANGED <<serving, legData, legUp, raidGen, legGen, acked, nextWrite,
                 riskSurfaced, state, writerSet, crashes>>

\* replica_replace + prune_writers_for_replacement: swap the identity of a
\* stale leg on a VERIFIABLY dead node; the freed slot returns as an empty
\* standby with a new identity (its old sb generation dies with the old
\* identity).
Replace(l) ==
  /\ state[l] = "stale"
  /\ legUp[l] = "dead"
  /\ UpInSync # {}                        \* something to rebuild from
  /\ legUp' = [legUp EXCEPT ![l] = "up"]
  /\ legData' = [legData EXCEPT ![l] = {}]
  /\ legGen' = [legGen EXCEPT ![l] = 0]
  /\ state' = [state EXCEPT ![l] = "standby"]
  /\ writerSet' = writerSet \ {l}
  /\ UNCHANGED <<serving, raidGen, acked, nextWrite, riskSurfaced,
                 epochCut, crashes>>

\* catch-up: full build to the last epoch cut from an in-sync source.
CatchUp(l) ==
  /\ state[l] = "standby"
  /\ legUp[l] = "up"
  /\ \E src \in UpInSync : epochCut \subseteq legData[src]
  /\ legData' = [legData EXCEPT ![l] = epochCut]
  /\ UNCHANGED <<serving, legUp, raidGen, legGen, acked, nextWrite,
                 riskSurfaced, state, writerSet, epochCut, crashes>>

\* Admission (hot-rejoin window / cutover reassembly) + mark_in_sync's
\* writer-set add: quiesced delta copy from a healthy serving survivor,
\* then join the raid at its current incarnation.
Admit(l) ==
  /\ state[l] = "standby"
  /\ legUp[l] = "up"
  /\ epochCut \subseteq legData[l]        \* warm standby (caught up)
  /\ serving # {}
  /\ \E src \in serving :
       /\ legUp[src] = "up"
       /\ legData' = [legData EXCEPT ![l] = legData[src]]
  /\ serving' = serving \cup {l}
  /\ legGen' = [legGen EXCEPT ![l] = raidGen]
  /\ state' = [state EXCEPT ![l] = "insync"]
  /\ writerSet' = writerSet \cup {l}
  /\ UNCHANGED <<legUp, raidGen, acked, nextWrite, riskSurfaced, epochCut,
                 crashes>>

\* NodeStage reassembly: the F36c freshness gate over the ATTACHABLE
\* in-sync legs A, then SPDK examine serves only A's newest generation
\* (set_writer_set stamps the actually-serving membership).  Proceed:
\* every writer-set leg attaches.  ServeWithRisk: every missing writer
\* VERIFIABLY dead — serve and surface.  Defer is the absence of this
\* action.  GateStrict = FALSE is the pre-F36c bug: any in-sync legs will
\* do — and a lone returned stale leg is its own newest generation, which
\* is precisely the 6-write-tail loss.
Assemble ==
  /\ serving = {}
  /\ \E A \in (SUBSET UpInSync) \ {{}} :
       /\ \/ /\ writerSet \subseteq A
             /\ UNCHANGED riskSurfaced
          \/ /\ GateStrict
             /\ writerSet \ A # {}
             /\ \A w \in writerSet \ A : legUp[w] = "dead"
             /\ riskSurfaced' = TRUE
          \/ /\ ~GateStrict
             /\ UNCHANGED riskSurfaced
       /\ serving' = NewestOf(A)
       /\ writerSet' = NewestOf(A)
       /\ legGen' = [m \in Legs |-> IF m \in NewestOf(A) THEN raidGen + 1
                                                         ELSE legGen[m]]
  /\ raidGen' = raidGen + 1
  /\ UNCHANGED <<legData, legUp, acked, nextWrite, state, epochCut, crashes>>

Next ==
  \/ Write
  \/ EpochCut
  \/ ServerCrash
  \/ Assemble
  \/ \E l \in Legs :
       \/ LegDie(l)
       \/ LegBlackhole(l)
       \/ LegRecover(l)
       \/ ConfirmDead(l)
       \/ RaidDeconfigure(l)
       \/ MonitorMarkStale(l)
       \/ Replace(l)
       \/ CatchUp(l)
       \/ Admit(l)

\* Recovery actions are weakly fair.  WF(RaidDeconfigure) is P4;
\* WF(ConfirmDead) is the replace-after threshold (a blackholed node is
\* eventually confirmed gone unless it recovers first — recovery is the
\* environment's choice and gets no fairness).  Failures and writes are
\* the environment.
Fairness ==
  /\ \A l \in Legs :
       /\ WF_vars(ConfirmDead(l))
       /\ WF_vars(RaidDeconfigure(l))
       /\ WF_vars(MonitorMarkStale(l))
       /\ WF_vars(Replace(l))
       /\ WF_vars(CatchUp(l))
       /\ WF_vars(Admit(l))
  /\ WF_vars(Assemble)
  /\ WF_vars(EpochCut)

Spec == Init /\ [][Next]_vars /\ Fairness

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

Inv == TypeOK /\ Inv_NoSilentLoss /\ Inv_InsyncServingIsCurrent
             /\ Inv_ServingCurrentGen

(***************************************************************************)
(* Liveness: availability after the storm.  Once the failure budget is    *)
(* exhausted, the system converges to a serving assembly with the acked   *)
(* content intact (or the risk surfaced) — and stays there.  Remove the   *)
(* P4/ConfirmDead fairness and this fails: a blackholed leg stalls        *)
(* everything forever.                                                    *)
(***************************************************************************)
GoodServing ==
  \/ riskSurfaced
  \/ /\ serving # {}
     /\ \A l \in serving : acked \subseteq legData[l]

EventuallyServingAgain == <>[](crashes < MaxCrashes \/ GoodServing)

\* State-space bound for TLC (raidGen grows with deconfigures/assemblies,
\* both bounded by the crash budget in any real trace).
GenBound == raidGen <= (3 * MaxCrashes) + 3

================================================================================
