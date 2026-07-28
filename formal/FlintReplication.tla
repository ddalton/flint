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
(* Out of scope: esnap-window INTERNALS (crash inside catch-up/scrub is    *)
(* the crash-sweep sim harness's job — here those steps are atomic);       *)
(* identity domains (killed at compile time by the newtypes).              *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Legs,        \* replica slots, e.g. {"l1", "l2", "l3"}
  MaxWrites,   \* bound on the write counter
  MaxCrashes,  \* bound on failure events (die/blackhole/crash/partition/torn)
  GateStrict,  \* TRUE = F36c gate on; FALSE = pre-F36c bug (must fail)
  RejoinGuard, \* TRUE = hot-rejoin shared-base ancestry check on
  FenceZombie  \* TRUE = assembly severs the previous head first (F48 fix)

VARIABLES
  \* ---- data plane -------------------------------------------------------
  serving,       \* SUBSET Legs: legs configured in the serving raid; {} = down
  zombie,        \* SUBSET Legs: a partitioned old head's assembly view (F48)
  legData,       \* [Legs -> SUBSET 1..MaxWrites]
  legUp,         \* [Legs -> {"up", "blackhole", "dead"}]
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
  crashes        \* failure budget spent

vars == <<serving, zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
          lineage, riskSurfaced, state, writerSet, epochCut, crashes>>

TypeOK ==
  /\ serving \subseteq Legs
  /\ zombie \subseteq Legs
  /\ legData \in [Legs -> SUBSET (1..MaxWrites)]
  /\ legUp \in [Legs -> {"up", "blackhole", "dead"}]
  /\ raidGen \in Nat
  /\ legGen \in [Legs -> Nat]
  /\ acked \subseteq 1..MaxWrites
  /\ nextWrite \in 1..(MaxWrites + 1)
  /\ lineage \subseteq 1..MaxWrites
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
  /\ zombie = {}
  /\ legData = [l \in Legs |-> {}]
  /\ legUp = [l \in Legs |-> "up"]
  /\ raidGen = 1
  /\ legGen = [l \in Legs |-> 1]
  /\ acked = {}
  /\ nextWrite = 1
  /\ lineage = {}
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
  /\ lineage' = lineage \cup {nextWrite}
  /\ nextWrite' = nextWrite + 1
  /\ UNCHANGED <<serving, zombie, legUp, raidGen, legGen, riskSurfaced,
                 state, writerSet, epochCut, crashes>>

\* The head crashes between replicating a block and acking the client: the
\* block lands on SOME serving legs, the client never hears.  Either outcome
\* is legitimate for the client — but the legs now disagree, and a holder
\* that misses the next assembly carries a dead-lineage phantom (the raw
\* material of the rejoin-divergence hazard).
WriteTorn ==
  /\ serving # {}
  /\ nextWrite <= MaxWrites
  /\ crashes < MaxCrashes
  /\ \A l \in serving : legUp[l] = "up"
  /\ \E S \in (SUBSET serving) \ {{}} :
       legData' = [l \in Legs |->
                     IF l \in S THEN legData[l] \cup {nextWrite}
                                ELSE legData[l]]
  /\ nextWrite' = nextWrite + 1
  /\ serving' = {}
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<zombie, legUp, raidGen, legGen, acked, lineage,
                 riskSurfaced, state, writerSet, epochCut>>

\* The F48 zombie: the partitioned old head still holds its leg
\* connections and still acks client writes.  It writes OUTSIDE the
\* record — no CAS, no lineage growth.  Its own raid is a sync mirror
\* too: one downed view-member stalls it.
ZombieWrite ==
  /\ zombie # {}
  /\ nextWrite <= MaxWrites
  /\ \A l \in zombie : legUp[l] = "up"
  /\ legData' = [l \in Legs |->
                   IF l \in zombie THEN legData[l] \cup {nextWrite}
                                   ELSE legData[l]]
  /\ acked' = acked \cup {nextWrite}
  /\ nextWrite' = nextWrite + 1
  /\ UNCHANGED <<serving, zombie, legUp, raidGen, legGen, lineage,
                 riskSurfaced, state, writerSet, epochCut, crashes>>

\* Verified death straight away (terminated AND observed so).
LegDie(l) ==
  /\ legUp[l] = "up"
  /\ crashes < MaxCrashes
  /\ legUp' = [legUp EXCEPT ![l] = "dead"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut>>

\* Silent unreachability: maybe a dying node, maybe a transient partition.
LegBlackhole(l) ==
  /\ legUp[l] = "up"
  /\ crashes < MaxCrashes
  /\ legUp' = [legUp EXCEPT ![l] = "blackhole"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut>>

\* The transient case: the leg returns, data intact, whatever the record
\* now says about it.  (The F36c ingredient.)
LegRecover(l) ==
  /\ legUp[l] = "blackhole"
  /\ legUp' = [legUp EXCEPT ![l] = "up"]
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, crashes>>

\* The permanent case, confirmed: node object gone / instance verified
\* terminated.  This — and only this — is the evidence Replace accepts.
ConfirmDead(l) ==
  /\ legUp[l] = "blackhole"
  /\ legUp' = [legUp EXCEPT ![l] = "dead"]
  /\ UNCHANGED <<serving, zombie, legData, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, crashes>>

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
  /\ UNCHANGED <<zombie, legData, legUp, acked, nextWrite, lineage,
                 riskSurfaced, state, writerSet, epochCut, crashes>>

\* The whole assembly dies cleanly (process gone, connections dropped).
ServerCrash ==
  /\ serving # {}
  /\ crashes < MaxCrashes
  /\ serving' = {}
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<zombie, legData, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut>>

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
                 lineage, riskSurfaced, state, writerSet, epochCut>>

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
                 nextWrite, lineage, riskSurfaced, epochCut, crashes>>

\* Epoch scheduler: cut a consistent snapshot of the served content.
EpochCut ==
  /\ serving # {}
  /\ \A l \in serving : legUp[l] = "up"
  /\ epochCut' = acked
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, state, writerSet, crashes>>

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
  /\ UNCHANGED <<serving, zombie, raidGen, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, crashes>>

\* hot_rejoin_volume: a stale leg on a LIVE node re-enters as a standby
\* KEEPING its identity and payload (contrast Replace).  Whether the
\* payload is usable is decided downstream — by the RejoinGuard ancestry
\* check at catch-up/admission, or by Scrub when it diverges.
HotRejoin(l) ==
  /\ state[l] = "stale"
  /\ legUp[l] = "up"
  /\ state' = [state EXCEPT ![l] = "standby"]
  /\ UNCHANGED <<serving, zombie, legData, legUp, raidGen, legGen, acked,
                 nextWrite, lineage, riskSurfaced, writerSet, epochCut,
                 crashes>>

\* HotRejoinScrubbed: no usable shared history with ANY live in-sync
\* source — wipe the payload and rebuild from scratch.  Requires a live
\* source to rebuild from: a scrub with nothing to rebuild from would
\* destroy the last copy.
Scrub(l) ==
  /\ state[l] = "standby"
  /\ legUp[l] = "up"
  /\ legData[l] # {}
  /\ UpInSync # {}
  /\ ~\E src \in UpInSync : legData[l] \subseteq legData[src]
  /\ legData' = [legData EXCEPT ![l] = {}]
  /\ legGen' = [legGen EXCEPT ![l] = 0]
  /\ UNCHANGED <<serving, zombie, legUp, raidGen, acked, nextWrite, lineage,
                 riskSurfaced, state, writerSet, epochCut, crashes>>

\* Catch-up: build to the last epoch cut from an in-sync source.  A block
\* copy fills holes and never erases — union semantics — so the shared-base
\* ancestry check (RejoinGuard) is what keeps a kept payload honest.
CatchUp(l) ==
  /\ state[l] = "standby"
  /\ legUp[l] = "up"
  /\ \E src \in UpInSync :
       /\ epochCut \subseteq legData[src]
       /\ (RejoinGuard => legData[l] \subseteq legData[src])
  /\ legData' = [legData EXCEPT ![l] = legData[l] \cup epochCut]
  /\ UNCHANGED <<serving, zombie, legUp, raidGen, legGen, acked, nextWrite,
                 lineage, riskSurfaced, state, writerSet, epochCut, crashes>>

\* Admission (hot-rejoin window / cutover reassembly) + mark_in_sync's
\* writer-set add: quiesced delta copy from a healthy serving survivor,
\* then join the raid at its current incarnation.  The ancestry check is
\* re-run HERE against the actual serving source — the admission-window
\* verification — because the world may have reassembled since catch-up.
Admit(l) ==
  /\ state[l] = "standby"
  /\ legUp[l] = "up"
  /\ epochCut \subseteq legData[l]        \* warm standby (caught up)
  /\ serving # {}
  /\ \E src \in serving :
       /\ legUp[src] = "up"
       /\ (RejoinGuard => legData[l] \subseteq legData[src])
       /\ legData' = [legData EXCEPT ![l] = legData[l] \cup legData[src]]
  /\ serving' = serving \cup {l}
  /\ legGen' = [legGen EXCEPT ![l] = raidGen]
  /\ state' = [state EXCEPT ![l] = "insync"]
  /\ writerSet' = writerSet \cup {l}
  /\ UNCHANGED <<zombie, legUp, raidGen, acked, nextWrite, lineage,
                 riskSurfaced, epochCut, crashes>>

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
             /\ UNCHANGED riskSurfaced
          \/ /\ GateStrict
             /\ writerSet \ A # {}
             /\ \A w \in writerSet \ A : legUp[w] = "dead"
             /\ riskSurfaced' = TRUE
          \/ /\ ~GateStrict
             /\ UNCHANGED riskSurfaced
       /\ serving' = NewestOf(A)
       /\ writerSet' = NewestOf(A)
       /\ lineage' = UNION {legData[m] : m \in NewestOf(A)}
       /\ legGen' = [m \in Legs |-> IF m \in NewestOf(A) THEN raidGen + 1
                                                         ELSE legGen[m]]
  /\ raidGen' = raidGen + 1
  /\ zombie' = IF FenceZombie THEN {} ELSE zombie
  /\ UNCHANGED <<legData, legUp, acked, nextWrite, state, epochCut, crashes>>

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
  /\ legUp[l] = "up"
  /\ l \in NewestOf({m \in Legs : state[m] = "stale" /\ legUp[m] = "up"})
  /\ serving' = {l}
  /\ writerSet' = {l}
  /\ lineage' = legData[l]
  /\ legGen' = [legGen EXCEPT ![l] = raidGen + 1]
  /\ raidGen' = raidGen + 1
  /\ riskSurfaced' = TRUE
  /\ state' = [state EXCEPT ![l] = "insync"]
  /\ zombie' = IF FenceZombie THEN {} ELSE zombie
  /\ UNCHANGED <<legData, legUp, acked, nextWrite, epochCut, crashes>>

Next ==
  \/ Write
  \/ WriteTorn
  \/ ZombieWrite
  \/ EpochCut
  \/ ServerCrash
  \/ ServerPartition
  \/ Assemble
  \/ \E l \in Legs :
       \/ LegDie(l)
       \/ LegBlackhole(l)
       \/ LegRecover(l)
       \/ ConfirmDead(l)
       \/ RaidDeconfigure(l)
       \/ MonitorMarkStale(l)
       \/ Replace(l)
       \/ HotRejoin(l)
       \/ Scrub(l)
       \/ CatchUp(l)
       \/ Admit(l)
       \/ LastResortServe(l)

\* Recovery actions are weakly fair.  WF(RaidDeconfigure) is P4;
\* WF(ConfirmDead) is the replace-after threshold (a blackholed node is
\* eventually confirmed gone unless it recovers first — recovery is the
\* environment's choice and gets no fairness).  WF(Scrub) is the
\* HotRejoinScrubbed arm: a divergent standby is eventually demoted to a
\* full rebuild rather than parking forever.  Failures and writes are
\* the environment; HotRejoin is the orchestrator's choice.
Fairness ==
  /\ \A l \in Legs :
       /\ WF_vars(ConfirmDead(l))
       /\ WF_vars(RaidDeconfigure(l))
       /\ WF_vars(MonitorMarkStale(l))
       /\ WF_vars(Replace(l))
       /\ WF_vars(Scrub(l))
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

\* No serving leg holds a block outside the served lineage (rejoin /
\* split-brain divergence): raid1 serves reads from ANY leg, so a phantom
\* block from a dead lineage or a zombie head is a split-read surface.
\* This is what the RejoinGuard ancestry check and the F48 sever protect.
Inv_NoDivergentServing ==
  ~riskSurfaced =>
    \A l \in serving : legData[l] \subseteq lineage

Inv == TypeOK /\ Inv_NoSilentLoss /\ Inv_InsyncServingIsCurrent
             /\ Inv_ServingCurrentGen /\ Inv_NoDivergentServing

(***************************************************************************)
(* Liveness: availability after the storm.  Once the failure budget is    *)
(* exhausted, the system converges to a serving assembly with the acked   *)
(* content intact (or the risk surfaced) — and stays there.  Remove the   *)
(* P4/ConfirmDead fairness and this fails: a blackholed leg stalls        *)
(* everything forever.  Remove WF(Scrub) and a divergent rejoiner parks   *)
(* as a standby forever instead of demoting to a full rebuild.            *)
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

\* State-space bound for TLC (raidGen grows with deconfigures/assemblies,
\* both bounded by the crash budget in any real trace).
GenBound == raidGen <= (3 * MaxCrashes) + 3

================================================================================
