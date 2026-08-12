--------------------------- MODULE FlintComposition ---------------------------
(***************************************************************************)
(* The block-tier SERVING-COMPOSITION machine — the arbiter that the       *)
(* 2026-08-12 replication review found missing between SPDK raid1 and a    *)
(* survivable failover (docs/plans/pnfs-block-layout-design.md §12,        *)
(* "Replication for the block tier").  Like FlintExtents tranche 1, this   *)
(* models code that DOES NOT EXIST YET, deliberately: every green here is  *)
(* a statement about the DESIGN, and the implementation must be written    *)
(* against these runs, not vice versa.                                     *)
(*                                                                         *)
(* THE WORLD: one volume = raid1(leg on node A, leg on node B), exported   *)
(* to pNFS block-layout clients by the COMPOSING tgt (initially A; B       *)
(* hosts a leg-export that admits A's inter-tgt initiator).  This is the   *)
(* world FlintExtents' abstraction note 1 deferred: "if two tgt            *)
(* incarnations can ever expose one extent range, the serving-target       *)
(* state is a SET from day one" (design doc §9).  Here they can, so it     *)
(* is: the MDS's durable RECORD [epoch, composer] is the single-writer     *)
(* serving-target state, advanced by one CAS, and every belt in this       *)
(* module is an enforcement point of that record.                          *)
(*                                                                         *)
(* WHY raid1 CANNOT ARBITRATE THIS ITSELF (all verified in the v26.05      *)
(* checkout during the review):                                            *)
(*   - A survivor leg's superblock still lists the dead peer CONFIGURED,   *)
(*     so auto-assembly counts discovered < operational and parks in       *)
(*     CONFIGURING forever (bdev_raid.c:3384-3396, 3730-3737).  Promotion  *)
(*     therefore NEEDS a force-degraded decision — and if "dead" was       *)
(*     actually "partitioned", two such decisions mint mutual solo-online  *)
(*     compositions that seq_number arbitration can never reconcile,       *)
(*     because neither process ever sees the other's leg.                  *)
(*   - NVMe reservations live at lib/nvmf per NODE, PTPL to a local file;  *)
(*     legs carry no reservation state, and an empty PTPL dir on the       *)
(*     survivor loads zero state SILENTLY (subsystem.c:3154-3158).  So a   *)
(*     fence "confirms" trivially at the survivor while the victim's       *)
(*     registration at the deposed tgt stands untouched.                   *)
(*                                                                         *)
(* THE VICTIM CLASS FlintReplication HAS NO VOCABULARY FOR: pNFS           *)
(* block-layout clients hold DIRECT nvme-tcp sessions to the composing     *)
(* tgt, with the MDS nowhere on the data path.  A partitioned composer     *)
(* (MDS cannot reach it, clients CAN) keeps acking their writes.  The     *)
(* zombie of FlintReplication's F48 is a consumer-node raid head; this     *)
(* zombie is a SERVER, and its clients are innocent third parties who      *)
(* never hear about the failover unless something redirects them.          *)
(*                                                                         *)
(* FIX ARMS (TRUE = the belt exists), each with a run that fails without   *)
(* it:                                                                     *)
(*   LegAdmissionGate   assembly requires the deposed composer's           *)
(*                      inter-tgt hostnqn EVICTED at the survivor's        *)
(*                      leg-export first (the FenceZombie transfer:        *)
(*                      admission cuts the old head before the new one     *)
(*                      serves).  Without it a late-suspending zombie      *)
(*                      fans its clients' writes INTO the leg the new      *)
(*                      composition is serving — split-brain divergence    *)
(*                      on the promoted device itself.                     *)
(*   DeadmanGate        the composer holds a serving LEASE against the     *)
(*                      MDS; on lapse it SUSPENDS its client-facing        *)
(*                      export (a local watchdog — works exactly when      *)
(*                      partitioned), and promotion WAITS for the lapse    *)
(*                      horizon.  This is the only exclusion the           *)
(*                      composer's LOCAL leg has: a local bdev write has   *)
(*                      no admission gate to evict from.                   *)
(*   DeadmanCertain     the synchrony AXIOM (EvidenceStrict's shape): the  *)
(*                      composer's self-suspend provably precedes the      *)
(*                      MDS's observation of the lapse.  TRUE is bounded   *)
(*                      clock skew; FALSE is the honest world where the    *)
(*                      suspend can arrive late.  The Skew run prices      *)
(*                      what the axiom buys, and it is LESS than first     *)
(*                      appears: writes in the skew window cannot doom or  *)
(*                      diverge (eviction makes them solo, and the         *)
(*                      DegradeBarrier refuses a solo ack the partitioned  *)
(*                      composer cannot mark) — the residual is a window   *)
(*                      of stale READS (Inv_NoStaleServe fails), the       *)
(*                      documented price of leasing without consensus.     *)
(*   RecordAssemblyOnly a recovered tgt consults the record before        *)
(*                      serving: deposed => tear down (the epoch-checked   *)
(*                      teardown verb), never re-converge the old export   *)
(*                      over the stale leg.  SPDK auto-examine never       *)
(*                      arbitrates assembly.                               *)
(*   ElectInSync        the CAS elects only a leg the record carries as    *)
(*                      in-sync (FlintReplication's GateStrict at the      *)
(*                      serving-target record).                            *)
(*   DegradeBarrier     the composer's raid acks a solo-landing write      *)
(*                      only AFTER the record carries the peer leg's       *)
(*                      stale mark — mark-then-degrade, FlintReplication's *)
(*                      RecordBarrier.  Stock raid1 does the opposite      *)
(*                      (ack on any-one-leg, record the miss async:        *)
(*                      bdev_raid.c:705-718, 2440-2444), so flint must     *)
(*                      interpose on leg failure.                          *)
(*   FreeWaitsActive    an epoch-current confirmation licenses a free      *)
(*                      only once that epoch's composition is ACTIVE.      *)
(*   FenceReplayOnAssemble                                                 *)
(*                      the new export opens with standing fences          *)
(*                      converged into its allow-list, fail-closed.        *)
(*   DeliveredEpochKeyed                                                   *)
(*                      the review's delivered-keyed-by-epoch schema       *)
(*                      change.  NOT ADOPTED (FALSE in the shipped cfg):   *)
(*                      finding 4 below.                                   *)
(*                                                                         *)
(* WHAT TLC REFUTED WHILE THIS MODULE WAS WRITTEN — four findings, each    *)
(* now a guard, an arm, or a dropped recommendation:                       *)
(*   1. EVICTION MUST NOT PRECEDE THE LEASE HORIZON.  Severing the         *)
(*      zombie's fan-in while it can still ack manufactures silent loss    *)
(*      (its clients' acked writes suddenly land only on the doomed        *)
(*      local leg); before the horizon those acks still reach the          *)
(*      surviving leg and are honest.  EvictAtLeg guards on the lapse,     *)
(*      and the implementation must carry the same order: CAS -> wait      *)
(*      horizon -> evict -> assemble -> replay -> redirect.                *)
(*   2. DOOM-AT-RECORD WAS THE MODEL'S OWN ABSTRACTION ERROR (first        *)
(*      strict run): a CAS'd-but-never-assembled composition dooms         *)
(*      nothing — the record can move again.  The loss is sealed at        *)
(*      ASSEMBLY (the force-degraded decision that marks the deposed leg   *)
(*      rebuild-only), which is where the doomed ghost latches — and       *)
(*      chasing that honest latch exposed the REAL missing mechanisms:     *)
(*      the review's three-arm arbiter has no answer to a                  *)
(*      degraded-window failover (elect the stale leg, discard every       *)
(*      acked solo write).  ElectInSync + DegradeBarrier are that          *)
(*      answer, and they are FlintReplication's GateStrict +               *)
(*      RecordBarrier arriving at the serving-target record.               *)
(*   3. AN EPOCH-VALID CONFIRMATION IS NOT YET A LICENSE.  Between CAS     *)
(*      and assembly the deposed composer's fan-in still reaches the       *)
(*      surviving leg, and at that leg-export the victim's writes travel   *)
(*      under the COMPOSER'S inter-tgt hostnqn — indistinguishable, so     *)
(*      no per-client preempt helps.  The free waits for ActiveNew         *)
(*      (FreeWaitsActive).                                                 *)
(*   4. THE REVIEW'S "delivered_unix KEYED BY EPOCH" IS THE WRONG          *)
(*      ENFORCEMENT POINT (second strict run).  Its counterexample: a      *)
(*      fence confirmed at A and the range legally freed at epoch 1 —      *)
(*      then failover, and the victim re-attaches to B unfenced (PTPL      *)
(*      never travels).  No keying of the delivered bit can close a free   *)
(*      that was LEGAL when it happened; the only thing that stops the     *)
(*      victim is the new export refusing it at the mouth.  Hence          *)
(*      FenceReplayOnAssemble — and with it, epoch-keying is REDUNDANT:    *)
(*      the shipped cfg holds DeliveredEpochKeyed = FALSE, and             *)
(*      FlintCompositionEpochKeyedToo.cfg is the MergeMin-style            *)
(*      machine-checked verdict that turning it on changes nothing.  The   *)
(*      code keeps delivered_unix exactly as FlintExtents'                 *)
(*      FreeRequiresDelivered has it; what MUST be built instead is the    *)
(*      allow-list replay at export-up.                                    *)
(*                                                                         *)
(* GHOSTS carry the theorems (the FlintExtents gen lesson — fire at the    *)
(* earliest HONEST point, which finding 2 taught is not always the         *)
(* write):                                                                 *)
(*   divergent   a stale composer's write landed on a leg of the ACTIVE    *)
(*               new composition (split-brain corruption of served         *)
(*               bytes).  Fires at the write.                              *)
(*   doomed      an un-fenced client's acked writes were discarded by a    *)
(*               promotion: latches at ASSEMBLE, the force-degraded        *)
(*               decision that seals the deposed leg as rebuild-only,      *)
(*               via the soloAcked bookkeeping (which leg holds acked      *)
(*               bytes the other lacks).                                   *)
(*   staleWrite  a client whose held extent's generation has moved         *)
(*               (freed + re-granted) landed a write on the current        *)
(*               composition: FlintExtents' Inv_NoStaleExtentWrite         *)
(*               arriving by the cross-incarnation door.                   *)
(*   staleServe  a deposed tgt accepted client IO while the new            *)
(*               composition was ACTIVE (stale reads served, acks minted   *)
(*               on a lineage the record has abandoned).                   *)
(*                                                                         *)
(* ABSTRACTIONS, STATED:                                                   *)
(*   1. Content is not tracked; harm is landing-set membership.  The       *)
(*      write-hole / divergent-legs-assemble-clean hazard (raid1 acks on   *)
(*      any-one-leg success and records the miss ASYNC after the ack —     *)
(*      bdev_raid.c:705-718, 2440-2444) is a DATA-level belt (forced       *)
(*      rebuild on unclean assembly) owned by tranche 2, not here.         *)
(*   2. Leg failure and rebuild-rejoin WITHIN one epoch are SPDK's own     *)
(*      machinery (a failed leg rejoins only as a rebuild target while     *)
(*      the survivor is ONLINE, bdev_raid.c:3949-3957) and are not         *)
(*      modelled; after a failover the composition is the survivor's leg   *)
(*      alone until tranche 2 models record-driven re-add.                 *)
(*   3. One volume, one extent range, whole-volume fences.  The dangerous  *)
(*      client never RETURNS its layout (a conforming reachable client     *)
(*      frees cleanly — the quarantine tranche proved that world; the      *)
(*      victim here is precisely the unreachable one).                     *)
(*   4. Redirect exists as an ACTION (ReAttach) with free timing — the     *)
(*      review's finding that no redirect ACTOR exists is a liveness       *)
(*      debt, deferred with the rest of liveness (promotion progress,      *)
(*      the forward livelock of a never-confirmable fence, lease           *)
(*      renewal); this tranche is safety only, CHECK_DEADLOCK FALSE.       *)
(*   5. MaxEpoch bounds failovers (one, in the shipped cfgs): epoch 3      *)
(*      would need the fail-back/rebuild story tranche 2 owns.             *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Tgts,         \* the two composing-capable nodes, e.g. {tA, tB}
  Clients,      \* pNFS block-layout clients, e.g. {c1, c2}
  MaxEpoch,     \* bound on record.epoch (2 = one failover)
  MaxCrashes,   \* bound on partition/death events
  LegAdmissionGate,        \* TRUE = evict-before-assemble (FenceZombie transfer)
  DeadmanGate,             \* TRUE = serving lease + self-suspend + promotion wait
  DeadmanCertain,          \* TRUE = the synchrony axiom (suspend beats the observation)
  RecordAssemblyOnly,      \* TRUE = a recovered tgt serves only what the record says
  ElectInSync,             \* TRUE = the CAS elects only an in-sync leg (GateStrict transfer)
  DegradeBarrier,          \* TRUE = mark-stale-then-solo-ack (RecordBarrier transfer)
  FreeWaitsActive,         \* TRUE = a free licenses only while the confirming
                           \*        epoch's composition is ACTIVE
  FenceReplayOnAssemble,   \* TRUE = the new composer's export opens with every
                           \*        standing fence already converged into its
                           \*        allow-list (fail-closed: converge failure =
                           \*        no listener), so a fenced client is excluded
                           \*        at the new namespace BEFORE it can re-attach
  DeliveredEpochKeyed      \* TRUE = the review's delivered-keyed-by-epoch
                           \*        refinement (scoped to the current epoch,
                           \*        key captured at preempt execution).  FALSE
                           \*        in the shipped cfg: under the replay belt
                           \*        it is machine-checked REDUNDANT — see the
                           \*        header's finding 4

ASSUME Cardinality(Tgts) = 2

InitComposer == CHOOSE t \in Tgts : TRUE
InitHolder   == CHOOSE c \in Clients : TRUE
Peer(t)      == CHOOSE u \in Tgts : u # t
MaxGen       == Cardinality(Clients) + 1

VARIABLES
  tgt,        \* [Tgts -> {"ok","part","dead"}] — ground truth; "part" = MDS
              \* cannot reach it, clients CAN (the fallible-verdict state)
  record,     \* [epoch, composer] — the MDS-sqlite serving-target record
  serving,    \* [Tgts -> 0..MaxEpoch] — the epoch a tgt's client-facing
              \* export is live under (0 = not serving)
  lastServed, \* [Tgts -> 0..MaxEpoch] — highest epoch this tgt ever assembled
  legAdmit,   \* [Tgts -> SUBSET Tgts] — which remote composers this node's
              \* leg-export admits (the local leg has NO gate; that asymmetry
              \* is why DeadmanGate exists)
  lease,      \* [Tgts -> {"live","lapsed"}] — the serving lease
  session,    \* [Clients -> Tgts] — where the client's nvme-tcp session points
  excl,       \* [Tgts -> SUBSET Clients] — clients preempted AT that tgt.
              \* Per-node BY CONSTRUCTION: this is PTPL locality
  fenced,     \* SUBSET Clients — MDS fence records
  pendingEpoch, \* [Clients -> 0..MaxEpoch] — the epoch an executed-but-unmarked
              \* preempt ran under (0 = none); the two-step: RPC+verify at a
              \* tgt captures this, THEN the sqlite row is updated
  delivered,  \* [Clients -> 0..MaxEpoch] — the epoch key the delivered mark
              \* carries (0 = unconfirmed)
  gen,        \* the range's ownership generation
  clientHeld, \* [Clients -> 0..MaxGen] — the gen the CLIENT believes it holds
              \* (cleared only by a return, which the victim never does)
  owner,      \* the range's current legitimate holder
  crashes,
  divergent, doomed, staleWrite, staleServe, \* the theorem ghosts
  zombieWrote, \* probe ghost: a deposed composer accepted a write
  legSync,    \* [Tgts -> {"insync","stale"}] — the RECORD's view of each
              \* node's leg; "stale" is sticky in this tranche (clearing it
              \* is tranche 2's record-driven rebuild)
  soloAcked   \* [Tgts -> BOOLEAN] — some un-fenced client's acked write
              \* landed ONLY on this node's leg (the degraded-window bytes
              \* a promotion of the OTHER leg would discard)

vars == <<tgt, record, serving, lastServed, legAdmit, lease, session, excl,
          fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
          divergent, doomed, staleWrite, staleServe, zombieWrote,
          legSync, soloAcked>>

CurrentLegs == IF record.epoch = 1 THEN Tgts ELSE {record.composer}
ActiveNew   == serving[record.composer] = record.epoch
Deposed     == Peer(record.composer)

TypeOK ==
  /\ tgt \in [Tgts -> {"ok","part","dead"}]
  /\ record \in [epoch: 1..MaxEpoch, composer: Tgts]
  /\ serving \in [Tgts -> 0..MaxEpoch]
  /\ lastServed \in [Tgts -> 0..MaxEpoch]
  \* structural tripwire: a tgt serves either nothing or exactly the epoch
  \* it last assembled — serving an epoch it never assembled is corruption
  /\ \A t \in Tgts : serving[t] \in {0, lastServed[t]}
  /\ legAdmit \in [Tgts -> SUBSET Tgts] /\ \A t \in Tgts : t \notin legAdmit[t]
  /\ lease \in [Tgts -> {"live","lapsed"}]
  /\ session \in [Clients -> Tgts]
  /\ excl \in [Tgts -> SUBSET Clients]
  /\ fenced \subseteq Clients
  /\ pendingEpoch \in [Clients -> 0..MaxEpoch]
  /\ delivered \in [Clients -> 0..MaxEpoch]
  /\ gen \in 1..MaxGen
  /\ clientHeld \in [Clients -> 0..MaxGen]
  /\ owner \in Clients
  /\ crashes \in 0..MaxCrashes
  /\ {divergent, doomed, staleWrite, staleServe, zombieWrote}
       \subseteq BOOLEAN
  /\ legSync \in [Tgts -> {"insync", "stale"}]
  /\ soloAcked \in [Tgts -> BOOLEAN]

Init ==
  /\ tgt = [t \in Tgts |-> "ok"]
  /\ record = [epoch |-> 1, composer |-> InitComposer]
  /\ serving = [t \in Tgts |-> IF t = InitComposer THEN 1 ELSE 0]
  /\ lastServed = serving
  /\ legAdmit = [t \in Tgts |-> IF t = InitComposer THEN {} ELSE {InitComposer}]
  /\ lease = [t \in Tgts |-> "live"]
  /\ session = [c \in Clients |-> InitComposer]
  /\ excl = [t \in Tgts |-> {}]
  /\ fenced = {}
  /\ pendingEpoch = [c \in Clients |-> 0]
  /\ delivered = [c \in Clients |-> 0]
  /\ gen = 1
  /\ clientHeld = [c \in Clients |-> IF c = InitHolder THEN 1 ELSE 0]
  /\ owner = InitHolder
  /\ crashes = 0
  /\ divergent = FALSE /\ doomed = FALSE /\ staleWrite = FALSE
  /\ staleServe = FALSE /\ zombieWrote = FALSE
  /\ legSync = [t \in Tgts |-> "insync"]
  /\ soloAcked = [t \in Tgts |-> FALSE]

(***************************************************************************)
(* Failure events.  "part" is the load-bearing state: the MDS's only       *)
(* evidence is its own unreachability, so every promotion below is         *)
(* justified by evidence that cannot distinguish a dead composer from a    *)
(* live one still acking its clients.                                      *)
(***************************************************************************)
Partition(t) ==
  /\ tgt[t] = "ok" /\ crashes < MaxCrashes
  /\ tgt' = [tgt EXCEPT ![t] = "part"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<record, serving, lastServed, legAdmit, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

Die(t) ==
  /\ tgt[t] \in {"ok", "part"} /\ crashes < MaxCrashes
  /\ tgt' = [tgt EXCEPT ![t] = "dead"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<record, serving, lastServed, legAdmit, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

(***************************************************************************)
(* Recovery is where RecordAssemblyOnly bites: the recovered node's        *)
(* reconciler re-converges from durable local config, and without the      *)
(* record check it happily re-exports the same subnqn+NGUID over the       *)
(* stale leg (the review's "nothing tears down A's export" finding).       *)
(* With the check, a deposed node's first contact IS the teardown verb.    *)
(* A node the record still names resumes only what it actually             *)
(* assembled (lastServed = record.epoch) — a CAS'd-but-never-assembled     *)
(* composer must come up through Assemble's guards, not through reboot.    *)
(***************************************************************************)
Recover(t) ==
  /\ tgt[t] \in {"part", "dead"}
  /\ tgt' = [tgt EXCEPT ![t] = "ok"]
  /\ lease' = [lease EXCEPT ![t] = "live"]
  /\ serving' = [serving EXCEPT ![t] =
       IF record.composer = t
         THEN IF lastServed[t] = record.epoch THEN record.epoch ELSE 0
         ELSE IF RecordAssemblyOnly THEN 0 ELSE lastServed[t]]
  /\ UNCHANGED <<record, lastServed, legAdmit, session, excl, fenced, pendingEpoch,
                 delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

(***************************************************************************)
(* The lease horizon.  The lapse is one FACT both sides key off — the      *)
(* MDS-side expiry observation, and (DeadmanGate) the composer's own       *)
(* watchdog suspending the client-facing export.  DeadmanCertain is the    *)
(* skew axiom: TRUE fuses the suspend into the lapse (the suspend          *)
(* provably precedes anything the MDS does with its observation); FALSE    *)
(* leaves the suspend to DeadmanFire, arbitrarily late.                    *)
(***************************************************************************)
LeaseLapse(t) ==
  /\ tgt[t] # "ok" /\ lease[t] = "live"
  /\ lease' = [lease EXCEPT ![t] = "lapsed"]
  /\ serving' = IF DeadmanGate /\ DeadmanCertain
                  THEN [serving EXCEPT ![t] = 0]
                  ELSE serving
  /\ UNCHANGED <<tgt, record, lastServed, legAdmit, session, excl, fenced,
                 pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

DeadmanFire(t) ==
  /\ DeadmanGate /\ ~DeadmanCertain
  /\ lease[t] = "lapsed" /\ serving[t] > 0 /\ tgt[t] # "dead"
  /\ serving' = [serving EXCEPT ![t] = 0]
  /\ UNCHANGED <<tgt, record, lastServed, legAdmit, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

(***************************************************************************)
(* Promotion, as the pipeline the implementation must be: CAS the record,  *)
(* wait the horizon, evict at the surviving leg-export, assemble.  Each    *)
(* step is its own action so TLC interleaves the zombie against every      *)
(* gap.  The CAS is justified by unreachability alone — deliberately: a    *)
(* verdict that could tell dead from partitioned does not exist (review    *)
(* finding, sequencing dimension).                                         *)
(***************************************************************************)
PromoteCAS ==
  /\ tgt[record.composer] \in {"part", "dead"}
  /\ record.epoch < MaxEpoch
  /\ \E s \in Tgts :
       /\ s # record.composer /\ tgt[s] = "ok"
       /\ ElectInSync => legSync[s] = "insync"
       /\ record' = [epoch |-> record.epoch + 1, composer |-> s]
  /\ UNCHANGED <<tgt, serving, lastServed, legAdmit, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

\* THE ORDERING GUARD: never sever the zombie's fan-in while it can still
\* ack (lease[d] = "lapsed" is the horizon).  Dropping this guard makes the
\* STRICT world fail Inv_NoDoomedAck — the model's own refutation of
\* evict-early, kept as a comment because the guard IS the finding.
EvictAtLeg ==
  /\ record.epoch >= 2
  /\ serving[record.composer] # record.epoch
  /\ tgt[record.composer] = "ok"
  /\ Deposed \in legAdmit[record.composer]
  /\ lease[Deposed] = "lapsed"
  /\ legAdmit' = [legAdmit EXCEPT ![record.composer] = @ \ {Deposed}]
  /\ UNCHANGED <<tgt, record, serving, lastServed, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

\* Assembly is the force-degraded decision: the survivor's raid comes up on
\* its own leg and the deposed leg is marked FAILED in the superblock —
\* rebuild-only rejoin from here on (bdev_raid.c:3949-3957).  That is why
\* DOOM latches HERE and not at the write or the CAS: the first strict run
\* of this module refuted doom-at-record (a CAS'd-but-never-assembled
\* composition dooms nothing — the record can move again), and doom-at-heal
\* would be later than the decision that seals it.  Assembling while the
\* deposed leg holds solo-acked bytes IS the loss.
Assemble ==
  /\ record.epoch >= 2
  /\ serving[record.composer] < record.epoch
  /\ tgt[record.composer] = "ok"
  /\ LegAdmissionGate => Deposed \notin legAdmit[record.composer]
  /\ DeadmanGate => lease[Deposed] = "lapsed"
  /\ serving' = [serving EXCEPT ![record.composer] = record.epoch]
  /\ lastServed' = [lastServed EXCEPT ![record.composer] = record.epoch]
  /\ legSync' = [legSync EXCEPT ![Deposed] = "stale"]
  /\ doomed' = (doomed \/ soloAcked[Deposed])
  \* THE FENCE REPLAY (finding 4): the exclusion a fence earned lives in
  \* the OLD node's PTPL and does not travel.  The new export therefore
  \* opens with every standing fence converged into its allow-list —
  \* fail-closed, one MDS-side computation (admissions minus fenced), not
  \* a per-client best-effort RPC.  Without this, a client fenced, freed
  \* and re-granted a whole epoch ago re-attaches to the survivor
  \* UNFENCED — the trace TLC found on this module's second strict run,
  \* which no delivered-keying can close because the free was legal.
  /\ excl' = IF FenceReplayOnAssemble
               THEN [excl EXCEPT ![record.composer] = @ \cup fenced]
               ELSE excl
  /\ UNCHANGED <<tgt, record, legAdmit, lease, session, fenced, pendingEpoch,
                 delivered, gen, clientHeld, owner, crashes,
                 divergent, staleWrite, staleServe, zombieWrote, soloAcked>>

(***************************************************************************)
(* The fence, in the code's real granularity.  FenceClient is the MDS      *)
(* fence record (grant rows dropped; the CLIENT's belief is untouched —    *)
(* it keeps writing).  PreemptExecute is the RPC + post-verify AT the      *)
(* record's current composer — and ONLY there: confirming at B says        *)
(* nothing about A's registrations, which is PTPL locality made            *)
(* structural.  DeliveredMark is the separate sqlite update; it stores     *)
(* the epoch CAPTURED AT EXECUTION (the honest two-step — stamping the     *)
(* record's epoch at row-update time would forge currency across a         *)
(* failover), though under finding 4 nothing downstream reads it as a      *)
(* key unless DeliveredEpochKeyed re-arms the refinement.                  *)
(***************************************************************************)
FenceClient(c) ==
  /\ c \notin fenced
  /\ fenced' = fenced \cup {c}
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

PreemptExecute(c) ==
  /\ c \in fenced
  /\ tgt[record.composer] = "ok"
  /\ excl' = [excl EXCEPT ![record.composer] = @ \cup {c}]
  /\ pendingEpoch' = [pendingEpoch EXCEPT ![c] = record.epoch]
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 fenced, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

DeliveredMark(c) ==
  /\ pendingEpoch[c] # 0
  /\ delivered' = [delivered EXCEPT ![c] = pendingEpoch[c]]
  /\ pendingEpoch' = [pendingEpoch EXCEPT ![c] = 0]
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

(***************************************************************************)
(* The free/regrant, gated exactly as the code's FreeRequiresDelivered     *)
(* graduation, plus the ActiveNew wait (finding 3) and, behind the         *)
(* not-adopted arm, the review's epoch-keying (finding 4).  The new owner  *)
(* is any un-fenced client; the old owner's clientHeld persists — it       *)
(* never returned, which is the only client quarantine/fencing exists      *)
(* for (quarantine tranche, 2026-08-12).                                   *)
(***************************************************************************)
RegrantRange ==
  /\ owner \in fenced
  /\ delivered[owner] # 0
  /\ DeliveredEpochKeyed => delivered[owner] = record.epoch
  \* THE THIRD MODULE FINDING: an epoch-valid confirmation is NOT yet a
  \* license.  Between the CAS and the assembly, the deposed composer's
  \* fan-in still reaches the surviving leg — and at that leg-export the
  \* victim's writes arrive under the COMPOSER'S inter-tgt hostnqn, which
  \* no per-client preempt can distinguish.  The free must wait until the
  \* confirming epoch's composition is the only reachable one: ACTIVE
  \* (assembled => evicted + horizon passed).  At epoch 1 ActiveNew is
  \* trivially true and this clause costs nothing.
  /\ FreeWaitsActive => ActiveNew
  /\ gen < MaxGen
  /\ \E c \in Clients \ fenced :
       /\ owner' = c
       /\ clientHeld' = [clientHeld EXCEPT ![c] = gen + 1]
  /\ gen' = gen + 1
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

(***************************************************************************)
(* The client write — the entire data path in one action, because that is  *)
(* what it is: client -> its session's tgt -> that tgt's raid fans to the  *)
(* legs it can reach (local always; the peer leg iff the peer's            *)
(* leg-export still admits it and the peer node is up).  The MDS appears   *)
(* nowhere.  Ghosts fire here, at the earliest honest point:               *)
(*   - doomed has NO ActiveNew gate: once the record has moved, a write    *)
(*     landing on zero record-composition legs is lost by construction     *)
(*     (its only home rejoins as a rebuild target), whether or not the     *)
(*     survivor has assembled yet.                                         *)
(*   - divergent DOES gate on ActiveNew: before the survivor serves, the   *)
(*     zombie's fan-in still writes the epoch-1 composition, and those     *)
(*     bytes are honest (they reach the leg the survivor will assemble     *)
(*     from).                                                              *)
(***************************************************************************)
ClientWrite(c) ==
  LET t == session[c]
      landed == {t} \cup {p \in Tgts \ {t} :
                            t \in legAdmit[p] /\ tgt[p] # "dead"}
  IN
  /\ tgt[t] # "dead"
  /\ serving[t] > 0
  /\ c \notin excl[t]
  \* THE DEGRADE BARRIER (RecordBarrier transfer): the raid refuses to ack
  \* a write it can only land on its own leg until the record carries the
  \* peer leg's stale mark.  This is mark-then-degrade, and it is also what
  \* silences a partitioned composer whose fan-in has been evicted: it
  \* cannot mark through the partition, so it cannot ack solo — its
  \* clients' writes fail instead of quietly landing on a doomed leg.
  /\ DegradeBarrier => (landed # {t} \/ legSync[Peer(t)] = "stale")
  /\ divergent' = (divergent \/ (/\ ActiveNew
                                 /\ serving[t] < record.epoch
                                 /\ landed \cap CurrentLegs # {}))
  /\ staleWrite' = (staleWrite \/ (/\ clientHeld[c] > 0
                                   /\ clientHeld[c] < gen
                                   /\ landed \cap CurrentLegs # {}))
  /\ staleServe' = (staleServe \/ (ActiveNew /\ serving[t] < record.epoch))
  /\ zombieWrote' = (zombieWrote \/ (serving[t] < record.epoch))
  /\ soloAcked' = IF landed = {t} /\ c \notin fenced
                    THEN [soloAcked EXCEPT ![t] = TRUE]
                    ELSE soloAcked
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, doomed, legSync>>

\* Reads take the same door minus the mirror fan-out: a deposed-but-alive
\* composer serving READS of an abandoned lineage is the harm the dead-man
\* exists for (writes are already stopped by barrier + eviction), and it
\* is why the Deadman A/B's counterexample arrives through this action.
ClientRead(c) ==
  LET t == session[c] IN
  /\ tgt[t] # "dead"
  /\ serving[t] > 0
  /\ c \notin excl[t]
  /\ staleServe' = (staleServe \/ (ActiveNew /\ serving[t] < record.epoch))
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, divergent, doomed, staleWrite, zombieWrote,
                 legSync, soloAcked>>

\* The composer reports a lost peer leg to the MDS — the stale mark the
\* barrier waits on and the election gate reads.  It requires the composer
\* to REACH the MDS: a partitioned composer cannot mark, which is exactly
\* why the barrier turns its degradation into a stall instead of a doom.
MarkStale ==
  LET t == record.composer IN
  /\ serving[t] > 0 /\ tgt[t] = "ok"
  /\ tgt[Peer(t)] = "dead"
  /\ legSync[Peer(t)] = "insync"
  /\ legSync' = [legSync EXCEPT ![Peer(t)] = "stale"]
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, divergent, doomed, staleWrite, staleServe,
                 zombieWrote, soloAcked>>

(***************************************************************************)
(* The redirect lane, timing-free.  The review found no ACTOR exists to    *)
(* run this (the csi-node session record replays the dead traddr with a    *)
(* deliberate "No MDS call"); modelling it as always-available is the      *)
(* ADVERSARIAL choice for safety — TLC re-attaches fenced clients to the   *)
(* survivor at the worst moments.  Whether it happens EVENTUALLY is the    *)
(* deferred liveness tranche.                                              *)
(***************************************************************************)
ReAttach(c) ==
  /\ tgt[record.composer] = "ok"
  /\ session[c] # record.composer
  /\ session' = [session EXCEPT ![c] = record.composer]
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked>>

Next ==
  \/ \E t \in Tgts : Partition(t) \/ Die(t) \/ Recover(t)
                       \/ LeaseLapse(t) \/ DeadmanFire(t)
  \/ PromoteCAS \/ EvictAtLeg \/ Assemble \/ MarkStale
  \/ \E c \in Clients : FenceClient(c) \/ PreemptExecute(c)
                          \/ DeliveredMark(c) \/ ClientWrite(c)
                          \/ ClientRead(c) \/ ReAttach(c)
  \/ RegrantRange

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* THE THEOREMS.                                                           *)
(***************************************************************************)

\* Split-brain: no deposed composer's write ever lands on a leg the active
\* new composition serves.  The FenceZombie transfer, stated at the leg.
Inv_NoDivergentServing == ~divergent

\* No silent loss: no promotion discards an un-fenced client's acked
\* writes.  Its A/Bs are the election pair — ElectStale (promote a leg the
\* record knows is stale) and DegradeBlind (stock raid1's unmarked solo
\* ack leaves the record ignorant) — and the Skew run shows the
\* DeadmanCertain axiom is NOT what carries it: even with a late suspend,
\* eviction + the barrier keep writes off the doomed path.
Inv_NoDoomedAck == ~doomed

\* Cross-incarnation extent safety: a freed-and-re-granted range is never
\* written by its previous holder through ANY composition.  FlintExtents'
\* headline theorem, arriving here by the cross-epoch doors: the
\* interregnum free (FreeEarly) and the unreplayed fence (NoReplay).
Inv_NoStaleExtentWrite == ~staleWrite

\* A deposed tgt accepts no client IO once the new composition is active:
\* the record is the only door to serving (heal-reconverge is the mutation).
Inv_NoStaleServe == ~staleServe

(***************************************************************************)
(* NON-VACUITY PROBES (the standing rule: a green safety run proves        *)
(* nothing without a witness that the guarded machinery actually fires).   *)
(* Each is an invariant TLC must VIOLATE.                                  *)
(***************************************************************************)

\* Witnesses a COMPLETE failover: the record moved and the survivor serves.
ProbeFailoverCompletes == ~(record.epoch >= 2 /\ ActiveNew)

\* Witnesses the zombie interleaving: a deposed composer really accepts a
\* write in this state graph (else every divergence green means "zombies
\* never write", not "zombies are contained").
ProbeZombieWriteReachable == ~zombieWrote

\* Witnesses the dangerous free: a fenced holder's range re-granted AFTER
\* a failover (the path the free belts and the fence replay police).
ProbePostFailoverFreeReachable == ~(gen >= 2 /\ record.epoch >= 2)

================================================================================
