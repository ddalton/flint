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
(* TRANCHE 2 (2026-08-12, same day): RECORD-DRIVEN REBUILD/REJOIN, behind  *)
(* RejoinEnabled (every tranche-1 cfg keeps its state space BIT-IDENTICAL  *)
(* — flagship distinct-count 102,962 verified unchanged).  members         *)
(* becomes real state; the composition's round trip becomes reachable      *)
(* (MaxEpoch = 3: promote away -> rebuild -> lose the survivor -> promote  *)
(* BACK, ProbeFailBackCompletes is its witness); and three belts get       *)
(* teeth:                                                                  *)
(*   RecordRejoinOnly   a stale mark clears ONLY through a record-driven   *)
(*                      rebuild.  The mutation is auto-examine self-       *)
(*                      rejoin: seq arbitration (which only ever sees its  *)
(*                      own leg) declares the leg clean, and the honest    *)
(*                      election gate then trusts corrupt bookkeeping —    *)
(*                      DegradeBlind's shape one layer up.                 *)
(*   UncleanResync      the write-hole belt: an unclean composer death     *)
(*                      comes back SOLO, peer leg stale, rebuild-only.     *)
(*                      Stock raid1 reassembles equal-seq divergent legs   *)
(*                      as clean equals with no resync; the code cannot    *)
(*                      see divergence, only "died serving", so the belt   *)
(*                      is conservative by construction.                   *)
(*   AncestryGuard      the RejoinGuard transfer: the DELTA rejoin door    *)
(*                      opens only for a leg provably AT its cut (no solo  *)
(*                      bytes of its own, no divergence) — the delta       *)
(*                      copies the SOURCE'S dirty regions and cannot       *)
(*                      erase what the target wrote alone.  Everything     *)
(*                      else takes the full rebuild, the one divergence    *)
(*                      eraser (flint-driven and sparse-aware per §12's    *)
(*                      decided rebuild engine — raid1's allocation-blind  *)
(*                      process is never the copy).                        *)
(* New theorem: Inv_NoSplitRead — no read is served through a composition  *)
(* whose member legs diverge (raid1's balancer coin-flips those,           *)
(* raid1.c:227-233).  RebuildStart doubles as the epoch-checked            *)
(* leg-admission grant: only record.composer is ever admitted at the       *)
(* rebuilding leg's export, because the admitting reconciler consults      *)
(* the record — the same one-door discipline Recover enforces.             *)
(*                                                                         *)
(* TRANCHE 3 (2026-08-12, same day): LIVENESS — SpecLive, four post-storm  *)
(* progress theorems (promotion, fence confirmation, client redirect,      *)
(* rebuild), and three REQUIRED-TO-FAIL runs: NoActor (the shipped world   *)
(* has no redirect actor — SpecNoRedirect withholds exactly that one       *)
(* fairness obligation and the parked-client lasso appears, the SpecNoP4   *)
(* pattern), StaticTraddr (the review's FORWARD livelock: preempts pinned  *)
(* to the constructor traddr never confirm after a failover, so the        *)
(* quarantine sweep parks forever — the target-registry requirement with   *)
(* teeth), and WaitsPrice (ElectInSync's availability bill as a lasso:     *)
(* a degraded volume whose composer then partitions is DOWN until that     *)
(* composer returns; the operator override is the undesigned escape).      *)
(*                                                                         *)
(* THE TRANCHE'S FINDING — THE LEASE BELONGS TO THE EPOCH, NOT THE NODE,   *)
(* in BOTH directions, and each half came from a counterexample:           *)
(*   (a) renewal is RECORD-CONDITIONED (first liveness lasso): a deposed   *)
(*       node that recovers must NOT get its serving lease back — the      *)
(*       MDS refuses renewal to anyone the record does not name — or       *)
(*       eviction waits for a horizon that never comes and promotion       *)
(*       wedges with every process healthy;                                *)
(*   (b) ASSEMBLY IS THE GRANT (the safety re-run's counterexample):       *)
(*       activating the composition and granting the epoch's serving       *)
(*       lease are one act, or a composer whose lease lapsed under an      *)
(*       earlier epoch serves leaseless — and when IT is deposed,          *)
(*       promotion reads the ancient lapse as an already-passed horizon    *)
(*       and assembles over a still-serving zombie.                        *)
(* Implementation shape: the lease names (volume, epoch, composer);        *)
(* Assemble writes it, renewal requests are validated against the          *)
(* record, and the deadman horizon promotion waits for is the lapse of    *)
(* THAT lease, never a node-liveness heartbeat.                            *)
(*                                                                         *)
(* TRANCHE 4 (2026-08-14): THE WITNESS — and the admission that tranches   *)
(* 1-3 had already assumed one.  The implementation found (9fcdd05,        *)
(* the_leg_host_cannot_elect_itself...) that the record this module CAS's  *)
(* does not exist: MDS shards share nothing, so a two-copy volume has TWO  *)
(* records, and every fact the survivor's election reads lives in the      *)
(* dead composer's sqlite.  This module never noticed because its record   *)
(* was implicitly a THIRD PARTY: PromoteCAS needs no reachability,         *)
(* LeaseLapse observes expiry spontaneously, MarkStale is guarded on the   *)
(* ACTOR's health alone.  A store both targets reach independently of      *)
(* each other's health IS a witness — sqlite on one of the targets         *)
(* cannot implement this module; etcd (K8s rv-CAS, the file tier's         *)
(* replica_sync.rs pattern) can.  DECIDED 2026-08-14: the witness holds    *)
(* the arbitration facts — seat, leg sync marks, serving lease — and THE   *)
(* FENCE IDENTITIES (the FenceLocal run corrected the first scoping,       *)
(* which left fences "sqlite-local": a survivor's fail-closed replay      *)
(* then reads an EMPTY table and the fenced victim re-attaches — the       *)
(* NoReplay counterexample arriving through fact LOCALITY with the belt    *)
(* ON).  The fence ENFORCEMENT lane — preempt RPCs, delivered marks, the   *)
(* quarantine sweep — stays sqlite-local and witness-free, visible below   *)
(* as the actions that carry no apiCut guard.                              *)
(*                                                                         *)
(* The tranche adds the witness's REACHABILITY, because that is the price  *)
(* of the decision:                                                        *)
(*   apiCut   targets that cannot reach the witness (API-server outage,    *)
(*            per-node control-plane partition).  A cut composer's         *)
(*            heartbeat fails, its lease lapses, and the dead-man          *)
(*            suspends a perfectly healthy export: ProbeHealthySuspend     *)
(*            REQUIRES TLC to produce that bill (the WaitsPrice pattern)   *)
(*            — availability spent on arbitration, the etcd decision's     *)
(*            one honest cost.  The TTL is the knob trading failover       *)
(*            latency against API-outage tolerance; there is no third      *)
(*            setting.                                                     *)
(*   peerCut  the tgt<->tgt channel cut with BOTH nodes healthy (verdict   *)
(*            probe and mirror fan-in ride the same wire) — the            *)
(*            symmetric partition that REFUTED peer-arbitrated leases      *)
(*            during the design fork.  Both sides race to the witness:     *)
(*            the composer to mark its peer stale, the peer to CAS the     *)
(*            seat.  The witness SERIALIZES them — whoever lands second    *)
(*            is refused (the mark: no longer the composer; the CAS:       *)
(*            ElectInSync reads the fresh stale mark) — which is the       *)
(*            whole argument for a third party, machine-checked by the     *)
(*            Witness strict run exploring both orders.                    *)
(* IMPLEMENTATION OBLIGATION the model's atomicity encodes: that           *)
(* serialization is real only if seat, marks and lease share ONE          *)
(* rv-CAS'd witness object per volume.  Split objects re-open the race    *)
(* between them, and no run here covers that world.                        *)
(*                                                                         *)
(* The lease's REFUSAL arm becomes load-bearing here in a way tranche 3    *)
(* could not show: under peerCut the composer's heartbeat channel is       *)
(* fine, and only the witness's refusal tells it it was deposed — and      *)
(* the refusal arrives ON the composer's own renewal call, so on that      *)
(* path DeadmanCertain is not an axiom at all, it is the mechanism.        *)
(* Required-to-fail runs: LocalMark (the mark lands in the composer's      *)
(* local record instead of the witness = peer-arbitration's degraded       *)
(* window, doomed acks), FenceLocal (above), WitnessDeadman (no dead-man   *)
(* in a pure-partition world: a condemned composer that clients still      *)
(* reach serves a moved record with ZERO process failures), NoWitness      *)
(* (SpecLive withholding the heal obligation = the shipped two-sqlite      *)
(* world: the survivor is healthy, in-sync and witnessless, and the        *)
(* 9fcdd05 finding appears as a parked-promotion lasso).                   *)
(*                                                                         *)
(* ABSTRACTIONS, STATED:                                                   *)
(*   1. Content is not tracked; harm is landing-set membership plus the    *)
(*      diverged boolean (a torn death or an unguarded delta rejoin left   *)
(*      the two legs holding different bytes).  WHICH blocks diverge is    *)
(*      below this model; the sim/rig tier owns byte-level truth.          *)
(*   2. The rebuild copy is atomic here (admit -> fan-in live writes ->    *)
(*      complete); crash INSIDE the copy is the crash-sweep sim harness's  *)
(*      job, per the FlintSnapshots esnap-window precedent.                *)
(*   3. One volume, one extent range, whole-volume fences.  The dangerous  *)
(*      client never RETURNS its layout (a conforming reachable client     *)
(*      frees cleanly — the quarantine tranche proved that world; the      *)
(*      victim here is precisely the unreachable one).                     *)
(*   4. Redirect exists as an ACTION (ReAttach) with free timing — the     *)
(*      review's finding that no redirect ACTOR exists is a liveness       *)
(*      debt, deferred with the rest of liveness (promotion progress,      *)
(*      the forward livelock of a never-confirmable fence, lease           *)
(*      renewal, rebuild progress); safety only, CHECK_DEADLOCK FALSE.     *)
(*   5. MaxEpoch = 3 in the rejoin cfgs bounds the round trip at one       *)
(*      fail-back; deeper epoch chains add no new mechanism.               *)
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
  DeliveredEpochKeyed,     \* TRUE = the review's delivered-keyed-by-epoch
                           \*        refinement (scoped to the current epoch,
                           \*        key captured at preempt execution).  FALSE
                           \*        in the shipped cfg: under the replay belt
                           \*        it is machine-checked REDUNDANT — see the
                           \*        header's finding 4
  \* ---- TRANCHE 2 (record-driven rebuild/rejoin; FALSE in every tranche-1
  \* cfg, which keeps those state spaces bit-identical — the MaintEnabled
  \* pattern, verified by flagship distinct-count match). ----
  RejoinEnabled,           \* TRUE = tranche-2 actions exist (rebuild, delta
                           \*        rejoin, torn composer death, fail-back)
  RecordRejoinOnly,        \* TRUE = a stale mark clears only through a
                           \*        record-driven rebuild — never by the
                           \*        recovered node's auto-examine winning seq
                           \*        arbitration and declaring itself clean
  UncleanResync,           \* TRUE = an unclean composer death (crash while
                           \*        serving) forces resync on recovery: come
                           \*        back solo, peer leg stale, rebuild-only —
                           \*        the write-hole belt (raid1 has NO dirty
                           \*        flag: equal-seq divergent legs assemble
                           \*        clean, bdev_raid.c:3390-3396)
  AncestryGuard,           \* TRUE = the RejoinGuard transfer: a leg may take
                           \*        the DELTA rejoin path only if provably at
                           \*        its cut state (no solo-acked bytes of its
                           \*        own, no divergence) — else full rebuild
  \* ---- TRANCHE 4 (the witness; MaxCuts = 0 in every earlier cfg, which
  \* keeps those state spaces bit-identical — the MaintEnabled pattern
  \* again: the cut actions are disabled, so apiCut/peerCut/localStale
  \* freeze at their Init values and every new guard reduces to the old
  \* text). ----
  MaxCuts,                 \* budget for control-plane cuts (witness cuts +
                           \* peer cuts share it, like crashes share theirs)
  MarkAtWitness,           \* TRUE = the degrade's stale mark lands in the
                           \*        WITNESS (the decided design).  FALSE =
                           \*        it lands in the composer's own local
                           \*        record — always writable, exactly when
                           \*        the witness is not, which is the
                           \*        temptation — and the election reads a
                           \*        witness that was never told: the
                           \*        peer-arbitration refutation as a flag
  FenceAtWitness,          \* TRUE = standing fence IDENTITIES are carried
                           \*        by the witness, so a SURVIVOR's
                           \*        assembly can replay them.  FALSE = the
                           \*        first scoping ("client fencing stays
                           \*        sqlite-local"): fence rows live only in
                           \*        the volume's home shard, and a
                           \*        survivor's fail-closed replay reads an
                           \*        empty table
  \* ---- TRANCHE 3 (liveness). ----
  PreemptFollowsRecord     \* TRUE = the fence preempt dials whichever tgt the
                           \*        record CURRENTLY names (per-pass target
                           \*        resolution).  FALSE is the shipped code's
                           \*        constructor-traddr world (block_export.rs
                           \*        traddr is reconciler config, not
                           \*        per-volume state) — the review's FORWARD
                           \*        livelock: after a failover every preempt
                           \*        dials the dead node, delivered stays 0,
                           \*        and every sweep parks in UnconfirmedFence
                           \*        forever.  The StaticTraddr run is that
                           \*        sentence as a lasso.

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
  soloAcked,  \* [Tgts -> BOOLEAN] — some un-fenced client's acked write
              \* landed ONLY on this node's leg (the degraded-window bytes
              \* a promotion of the OTHER leg would discard)
  members,    \* SUBSET Tgts — the current composition's member legs.  In
              \* tranche 1 this was derivable (Tgts at epoch 1, {composer}
              \* after any assembly); rebuild-rejoin makes it real state.
  diverged,   \* ghost: the two legs hold divergent bytes (a torn composer
              \* death split them, or an unguarded delta rejoin kept a
              \* divergent leg's own bytes).  Cleared only by a FULL rebuild
              \* — raid1 itself has no scrub to clear it with.
  splitRead,  \* theorem ghost: a read was served through a composition with
              \* two divergent member legs — raid1's least-loaded balancer
              \* (raid1.c:227-233) makes every such read a coin flip
  rejoined,   \* probe ghost: a stale leg actually re-entered the composition
  \* ---- tranche 4 ----
  apiCut,     \* SUBSET Tgts — targets that cannot reach the WITNESS.  A
              \* separate axis from tgt-state on purpose: "part" is the
              \* control plane losing the TARGET; this is a target losing
              \* the control plane, clients and peer untouched
  peerCut,    \* BOOLEAN — the tgt<->tgt channel is down, both nodes "ok".
              \* One wire on purpose: the verdict probe (probe_nvme_tcp)
              \* and the mirror fan-in dial the same registry coordinates
  localStale, \* [Tgts -> BOOLEAN] — t's LOCAL record's claim that Peer(t)
              \* is stale.  Written only under ~MarkAtWitness; the witness
              \* never hears it, which is the whole defect
  cuts        \* the cut budget spent

vars == <<tgt, record, serving, lastServed, legAdmit, lease, session, excl,
          fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
          divergent, doomed, staleWrite, staleServe, zombieWrote,
          legSync, soloAcked, members, diverged, splitRead, rejoined,
          apiCut, peerCut, localStale, cuts>>

\* The cut state as one tuple: every pre-tranche-4 action leaves it
\* unchanged, and appending this is a smaller, safer diff than editing
\* twenty-five UNCHANGED lists item by item.
cutVars == <<apiCut, peerCut, localStale, cuts>>

\* The composition's member set.  Tranche 2 made it a variable; the alias
\* keeps tranche 1's ghost predicates textually unchanged.
CurrentLegs == members
ActiveNew   == serving[record.composer] = record.epoch
Deposed     == Peer(record.composer)

\* The volume's HOME shard: where its geometry lives, where fences are
\* minted, where the free/sweep lane runs.  A fixed fact of the volume —
\* the seat moves at promotion, the home does not (the home's sqlite is
\* the extent record, and THAT failover is a different workstream).
FenceHome == InitComposer

\* What t can read of the standing fence set at assembly time.  The home
\* shard always knows its own rows; anyone else knows them only if the
\* witness carries them.  Under FenceAtWitness this is the old global
\* read, verbatim.
KnownFences(t) == IF FenceAtWitness \/ t = FenceHome THEN fenced ELSE {}

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
  /\ members \in SUBSET Tgts /\ members # {}
  \* structural tripwire: an active composer is always a member of its own
  \* composition
  /\ ActiveNew => record.composer \in members
  /\ {diverged, splitRead, rejoined} \subseteq BOOLEAN
  /\ apiCut \subseteq Tgts
  /\ peerCut \in BOOLEAN
  /\ localStale \in [Tgts -> BOOLEAN]
  /\ cuts \in 0..MaxCuts

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
  /\ members = Tgts
  /\ diverged = FALSE /\ splitRead = FALSE /\ rejoined = FALSE
  /\ apiCut = {} /\ peerCut = FALSE
  /\ localStale = [t \in Tgts |-> FALSE]
  /\ cuts = 0

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
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

Die(t) ==
  /\ tgt[t] \in {"ok", "part"} /\ crashes < MaxCrashes
  /\ tgt' = [tgt EXCEPT ![t] = "dead"]
  /\ crashes' = crashes + 1
  /\ UNCHANGED <<record, serving, lastServed, legAdmit, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

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
  LET unclean == tgt[t] = "dead" /\ record.composer = t
                   /\ lastServed[t] = record.epoch
  IN
  /\ tgt[t] \in {"part", "dead"}
  \* Recovery's first act is consulting the record (RecordAssemblyOnly's
  \* whole point), and the record is the witness: a recovered node that
  \* cannot reach it DEFERS — serving anything on local memory alone is
  \* exactly the auto-examine door this module spent tranche 2 closing.
  /\ t \notin apiCut
  /\ tgt' = [tgt EXCEPT ![t] = "ok"]
  \* Record-conditioned renewal: only the node the record names gets its
  \* serving lease back on recovery — a deposed node's renewal is refused
  \* (see LeaseLapse's header), so its lapsed lease STAYS lapsed and the
  \* eviction horizon it anchors stays passed.
  /\ lease' = [lease EXCEPT ![t] = IF record.composer = t THEN "live" ELSE @]
  /\ serving' = [serving EXCEPT ![t] =
       IF record.composer = t
         THEN IF lastServed[t] = record.epoch THEN record.epoch ELSE 0
         ELSE IF RecordAssemblyOnly THEN 0 ELSE lastServed[t]]
  \* THE WRITE-HOLE BELT (UncleanResync): raid1 has no dirty flag — after a
  \* crash between leg writes both legs' superblocks claim CONFIGURED at
  \* equal seq and reassembly serves them as equals with no resync
  \* (bdev_raid.c:3390-3396), read-flapping on the divergence.  The code
  \* cannot see `diverged`; what it CAN see is "this composer stopped
  \* uncleanly while serving", so the belt is conservative: come back solo,
  \* peer leg stale, rebuild-only rejoin.  A clean partition heal is not
  \* unclean — the process never died mid-write.
  /\ members' = IF RejoinEnabled /\ UncleanResync /\ unclean
                  THEN {t} ELSE members
  /\ legSync' = IF RejoinEnabled /\ UncleanResync /\ unclean
                  THEN [legSync EXCEPT ![Peer(t)] = "stale"] ELSE legSync
  /\ UNCHANGED <<record, lastServed, legAdmit, session, excl, fenced, pendingEpoch,
                 delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 soloAcked, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

(***************************************************************************)
(* The lease horizon.  The lapse is one FACT both sides key off — the      *)
(* MDS-side expiry observation, and (DeadmanGate) the composer's own       *)
(* watchdog suspending the client-facing export.  DeadmanCertain is the    *)
(* skew axiom: TRUE fuses the suspend into the lapse (the suspend          *)
(* provably precedes anything the MDS does with its observation); FALSE    *)
(* leaves the suspend to DeadmanFire, arbitrarily late.                    *)
(*                                                                         *)
(* THE LEASE BELONGS TO THE EPOCH, NOT THE NODE (tranche 3's finding,      *)
(* from the liveness strict run's first lasso): a lease can fail to renew  *)
(* for TWO reasons — the node is unreachable, OR the record no longer      *)
(* names it, in which case the MDS REFUSES the renewal even from a         *)
(* healthy node.  The first draft lapsed only on unreachability, and a     *)
(* deposed composer that recovered re-armed its lease forever: eviction    *)
(* waits for a horizon that never comes, assembly waits for eviction,      *)
(* and the promotion pipeline wedges with every process healthy.  The      *)
(* implementation commitment: lease renewal is RECORD-CONDITIONED — the    *)
(* grant names (volume, epoch, composer), and a renewal request from       *)
(* anyone else is refused (the same epoch-checked door Recover walks).     *)
(***************************************************************************)
LeaseLapse(t) ==
  /\ lease[t] = "live"
  \* Three roads to a lapse now: unreachable (can't renew), deposed (the
  \* witness REFUSES — and the refusal arrives on t's own renewal call,
  \* so on this road the "certain" suspend is mechanism, not axiom), and
  \* — tranche 4 — witness-cut: the heartbeat has nowhere to land, and a
  \* composer that cannot prove its entitlement must stop claiming it.
  \* That third road is the etcd bill; ProbeHealthySuspend collects it.
  /\ (tgt[t] # "ok" \/ record.composer # t \/ t \in apiCut)
  /\ lease' = [lease EXCEPT ![t] = "lapsed"]
  /\ serving' = IF DeadmanGate /\ DeadmanCertain
                  THEN [serving EXCEPT ![t] = 0]
                  ELSE serving
  /\ UNCHANGED <<tgt, record, lastServed, legAdmit, session, excl, fenced,
                 pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

\* The dead-man is LOCAL by design — it exists precisely for the node
\* that can reach nobody, so it carries no witness guard.
DeadmanFire(t) ==
  /\ DeadmanGate /\ ~DeadmanCertain
  /\ lease[t] = "lapsed" /\ serving[t] > 0 /\ tgt[t] # "dead"
  /\ serving' = [serving EXCEPT ![t] = 0]
  /\ UNCHANGED <<tgt, record, lastServed, legAdmit, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

(***************************************************************************)
(* Promotion, as the pipeline the implementation must be: CAS the record,  *)
(* wait the horizon, evict at the surviving leg-export, assemble.  Each    *)
(* step is its own action so TLC interleaves the zombie against every      *)
(* gap.  The CAS is justified by unreachability alone — deliberately: a    *)
(* verdict that could tell dead from partitioned does not exist (review    *)
(* finding, sequencing dimension).                                         *)
(***************************************************************************)
PromoteCAS ==
  \* The verdict is the peer's DATA-PATH probe, so a pure peer-cut
  \* condemns exactly like a partition — deliberately: probe_nvme_tcp and
  \* the mirror fan-in ride the same wire, and a verdict that cannot tell
  \* dead from partitioned certainly cannot tell either from a cut cable.
  /\ (tgt[record.composer] \in {"part", "dead"} \/ peerCut)
  /\ record.epoch < MaxEpoch
  /\ \E s \in Tgts :
       /\ s # record.composer /\ tgt[s] = "ok"
       \* The CAS is a WITNESS write: an elector that cannot reach the
       \* witness cannot promote — the 9fcdd05 world, where this guard is
       \* permanently unsatisfiable because the witness does not exist.
       /\ s \notin apiCut
       /\ ElectInSync => legSync[s] = "insync"
       /\ record' = [epoch |-> record.epoch + 1, composer |-> s]
  /\ UNCHANGED <<tgt, serving, lastServed, legAdmit, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

\* THE ORDERING GUARD: never sever the zombie's fan-in while it can still
\* ack (lease[d] = "lapsed" is the horizon).  Dropping this guard makes the
\* STRICT world fail Inv_NoDoomedAck — the model's own refutation of
\* evict-early, kept as a comment because the guard IS the finding.
EvictAtLeg ==
  /\ record.epoch >= 2
  /\ serving[record.composer] # record.epoch
  /\ tgt[record.composer] = "ok"
  \* The horizon this action waits on is READ FROM THE WITNESS (the
  \* deposed lease's expiry lives there), so the evictor needs reach.
  /\ record.composer \notin apiCut
  /\ Deposed \in legAdmit[record.composer]
  /\ lease[Deposed] = "lapsed"
  /\ legAdmit' = [legAdmit EXCEPT ![record.composer] = @ \ {Deposed}]
  /\ UNCHANGED <<tgt, record, serving, lastServed, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

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
  \* Assembly writes the witness twice (the deposed leg's stale mark, the
  \* epoch's lease grant) — no reach, no assembly.
  /\ record.composer \notin apiCut
  /\ LegAdmissionGate => Deposed \notin legAdmit[record.composer]
  /\ DeadmanGate => lease[Deposed] = "lapsed"
  /\ serving' = [serving EXCEPT ![record.composer] = record.epoch]
  /\ lastServed' = [lastServed EXCEPT ![record.composer] = record.epoch]
  \* ASSEMBLE IS ALSO THE LEASE GRANT (the second half of tranche 3's
  \* lease finding): activating the composition and granting the epoch's
  \* serving lease are ONE act.  Without this, a node whose lease lapsed
  \* back when the record named someone else composes with a dead lease —
  \* and when IT is later deposed, promotion reads that ancient lapse as
  \* a horizon already passed and assembles over a still-serving zombie.
  /\ lease' = [lease EXCEPT ![record.composer] = "live"]
  /\ legSync' = [legSync EXCEPT ![Deposed] = "stale"]
  /\ members' = {record.composer}
  /\ doomed' = (doomed \/ soloAcked[Deposed])
  \* THE FENCE REPLAY (finding 4): the exclusion a fence earned lives in
  \* the OLD node's PTPL and does not travel.  The new export therefore
  \* opens with every standing fence converged into its allow-list —
  \* fail-closed, one MDS-side computation (admissions minus fenced), not
  \* a per-client best-effort RPC.  Without this, a client fenced, freed
  \* and re-granted a whole epoch ago re-attaches to the survivor
  \* UNFENCED — the trace TLC found on this module's second strict run,
  \* which no delivered-keying can close because the free was legal.
  \* TRANCHE 4 refinement: the replay converges what the ASSEMBLER CAN
  \* READ.  With witness-carried fences that is the full standing set,
  \* the old semantics verbatim; under FenceLocal (the first scoping's
  \* world) a survivor reads an empty table and this belt — still ON,
  \* still fail-closed, still passing its own unit tests — replays
  \* nothing.  A belt is only as good as the fact's reach.
  /\ excl' = IF FenceReplayOnAssemble
               THEN [excl EXCEPT ![record.composer] =
                       @ \cup KnownFences(record.composer)]
               ELSE excl
  /\ UNCHANGED <<tgt, record, legAdmit, session, fenced, pendingEpoch,
                 delivered, gen, clientHeld, owner, crashes,
                 divergent, staleWrite, staleServe, zombieWrote, soloAcked,
                 diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

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
\* The fence ENFORCEMENT lane — mint, preempt, delivered-mark, free —
\* carries no apiCut guard anywhere: it is sqlite-co-transactional at the
\* home shard and tgt-RPC-driven, and keeping it witness-free is the half
\* of the "fencing must not depend on the API server" invariant that
\* SURVIVES the witness decision.  (What had to move is the fence
\* IDENTITIES — see KnownFences and the FenceLocal run.)
FenceClient(c) ==
  /\ c \notin fenced
  /\ fenced' = fenced \cup {c}
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

PreemptExecute(c) ==
  LET target == IF PreemptFollowsRecord THEN record.composer ELSE InitComposer
  IN
  /\ c \in fenced
  /\ tgt[target] = "ok"
  /\ excl' = [excl EXCEPT ![target] = @ \cup {c}]
  /\ pendingEpoch' = [pendingEpoch EXCEPT ![c] = record.epoch]
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 fenced, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

DeliveredMark(c) ==
  /\ pendingEpoch[c] # 0
  /\ delivered' = [delivered EXCEPT ![c] = pendingEpoch[c]]
  /\ pendingEpoch' = [pendingEpoch EXCEPT ![c] = 0]
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

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
  \* ActiveNew is a WITNESS read (the sweep asking "is the confirming
  \* epoch's composition the serving one?"), made by the home shard: cut
  \* from the witness, the sweep DEFERS — undelivered is the safe side
  \* quarantine already holds for, so the guard only narrows.
  /\ FreeWaitsActive => (ActiveNew /\ FenceHome \notin apiCut)
  /\ gen < MaxGen
  /\ \E c \in Clients \ fenced :
       /\ owner' = c
       /\ clientHeld' = [clientHeld EXCEPT ![c] = gen + 1]
  /\ gen' = gen + 1
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

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
      \* peerCut severs the mirror fan-in: it is the same wire the
      \* verdict probe dials, which is what makes a pure cable cut
      \* condemn AND degrade together.
      landed == {t} \cup {p \in Tgts \ {t} :
                            t \in legAdmit[p] /\ tgt[p] # "dead" /\ ~peerCut}
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
  \* TRANCHE 4: "the record" means THE WITNESS.  Under ~MarkAtWitness the
  \* barrier reads the composer's own local mark instead — always
  \* satisfiable, exactly when the witness is not, and the election never
  \* hears it: the LocalMark run's doomed acks.
  /\ DegradeBarrier => (landed # {t}
                          \/ IF MarkAtWitness THEN legSync[Peer(t)] = "stale"
                                              ELSE localStale[t])
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
                 crashes, doomed, legSync, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

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
  \* The read-flap ghost: a composition serving two divergent member legs
  \* answers this read from whichever leg is least loaded (raid1.c:227-233)
  \* — old or new bytes, nondeterministically.  Fires only when BOTH legs
  \* are members: a divergent NON-member leg is exactly what the belts
  \* force (solo + stale + rebuild-only), and it flaps nothing.
  /\ splitRead' = (splitRead \/ (/\ diverged
                                 /\ Cardinality(members) >= 2
                                 /\ serving[t] = record.epoch))
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, divergent, doomed, staleWrite, zombieWrote,
                 legSync, soloAcked, members, diverged, rejoined>>
  /\ UNCHANGED cutVars

\* The composer reports a lost peer leg — the stale mark the barrier
\* waits on and the election gate reads.  TRANCHE 4 makes the mark's HOME
\* the load-bearing choice:
\*   MarkAtWitness: the mark is a WITNESS write.  The composer needs
\*     witness reach (a fully-partitioned composer cannot mark, which is
\*     exactly why the barrier turns its degradation into a stall instead
\*     of a doom) — and the action is guarded on being the CURRENT
\*     composer, which is the model's rendering of the rv-CAS refusal: a
\*     deposed node's mark bounces off the moved seat.  Under a pure
\*     peerCut this is one side of the race the witness serializes.
\*   ~MarkAtWitness: the mark lands in the composer's OWN record — no
\*     reach required, which is the temptation (it is available exactly
\*     when the witness is not), and the refutation: the barrier opens on
\*     a fact the election will never read.  Peer-arbitration's degraded
\*     window, and the split-record world's, in one flag.
\* The trigger gains peerCut: the composer's own probe of its peer fails
\* the same way on a dead node and a cut cable.
MarkStale ==
  LET t == record.composer IN
  /\ serving[t] > 0 /\ tgt[t] = "ok"
  /\ (tgt[Peer(t)] = "dead" \/ peerCut)
  /\ IF MarkAtWitness
       THEN /\ t \notin apiCut
            /\ legSync[Peer(t)] = "insync"
            /\ legSync' = [legSync EXCEPT ![Peer(t)] = "stale"]
            /\ UNCHANGED localStale
       ELSE /\ ~localStale[t]
            /\ localStale' = [localStale EXCEPT ![t] = TRUE]
            /\ UNCHANGED legSync
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, divergent, doomed, staleWrite, staleServe,
                 zombieWrote, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED <<apiCut, peerCut, cuts>>

(***************************************************************************)
(* TRANCHE 2 — record-driven rebuild/rejoin.  A torn composer death, the   *)
(* rebuild pipeline (admit -> copy -> member), the delta-rejoin ancestry   *)
(* rule, and the auto-examine self-rejoin door.  All gated on              *)
(* RejoinEnabled so every tranche-1 cfg keeps its exact state space.       *)
(***************************************************************************)

\* The write hole made reachable: the composer dies BETWEEN leg writes of
\* a fanned write (raid1 acks on any-one-leg and records nothing durable
\* before the crash — bdev_raid.c:705-718, 2440-2444), so the two member
\* legs now hold divergent bytes and both superblocks still claim
\* CONFIGURED at equal seq.  Costs a crash from the same budget as Die.
DieMidWrite ==
  LET t == record.composer IN
  /\ RejoinEnabled
  /\ crashes < MaxCrashes
  /\ tgt[t] = "ok" /\ serving[t] > 0
  /\ Cardinality(members) = 2
  /\ tgt' = [tgt EXCEPT ![t] = "dead"]
  /\ crashes' = crashes + 1
  /\ diverged' = TRUE
  /\ UNCHANGED <<record, serving, lastServed, legAdmit, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, splitRead, rejoined>>
  /\ UNCHANGED cutVars

\* The rebuild pipeline, step 1: the STALE leg's node re-admits the current
\* composer at its leg-export.  Epoch-checked by construction — the only
\* initiator this action ever admits is record.composer, because the
\* admitting reconciler consults the record (the same record-is-the-only-
\* door discipline Recover enforces).  From here the composer's writes fan
\* to the rebuilding leg (landed picks it up via legAdmit), which is
\* exactly SPDK's live-write-plus-copied-windows rebuild shape.
RebuildStart ==
  LET s == record.composer
      d == Peer(s)
  IN
  /\ RejoinEnabled
  /\ ActiveNew
  /\ legSync[d] = "stale"
  /\ tgt[s] = "ok" /\ tgt[d] = "ok"
  \* The admitting reconciler CONSULTS THE RECORD (that is this action's
  \* whole discipline), and the record is the witness: the stale leg's
  \* node needs reach before it may open its door.
  /\ d \notin apiCut
  /\ s \notin legAdmit[d]
  /\ legAdmit' = [legAdmit EXCEPT ![d] = @ \cup {s}]
  /\ UNCHANGED <<tgt, record, serving, lastServed, lease, session, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, divergent, doomed, staleWrite, staleServe,
                 zombieWrote, legSync, soloAcked, members, diverged,
                 splitRead, rejoined>>
  /\ UNCHANGED cutVars

\* Step 2, the FULL copy (flint-driven, sparse-aware — §12's decided
\* rebuild engine; raid1's own allocation-blind process is never used).
\* The target leg's content becomes the source's: divergence and solo
\* bytes on EITHER side are resolved (the target's own divergent bytes are
\* overwritten; the source's solo bytes now exist on both legs).  Only
\* here does the stale mark clear and the leg re-enter the composition.
RebuildComplete ==
  LET s == record.composer
      d == Peer(s)
  IN
  /\ RejoinEnabled
  /\ ActiveNew
  /\ legSync[d] = "stale"
  /\ s \in legAdmit[d]
  /\ tgt[s] = "ok" /\ tgt[d] = "ok"
  \* The copy crosses the tgt<->tgt wire; the in-sync mark is a witness
  \* write by the composer.  Both reaches required, or the rebuild waits.
  /\ ~peerCut /\ s \notin apiCut
  /\ legSync' = [legSync EXCEPT ![d] = "insync"]
  /\ members' = members \cup {d}
  /\ soloAcked' = [t \in Tgts |-> FALSE]
  /\ diverged' = FALSE
  /\ rejoined' = TRUE
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld,
                 owner, crashes, divergent, doomed, staleWrite, staleServe,
                 zombieWrote, splitRead>>
  /\ UNCHANGED cutVars

\* Step 2, the DELTA path: copy only the regions the record's dirty
\* tracking knows changed since the leg's cut (the degraded-window dirty
\* set the DegradeBarrier interposition maintains — §12's incremental
\* ladder).  The delta brings the SOURCE'S bytes over, so the source's
\* solo-acked marks clear — but it knows nothing of bytes the TARGET wrote
\* on its own (a torn write that landed target-only, a degraded stint as
\* composer), and those survive the rejoin as live divergence.  That is
\* why AncestryGuard exists: the delta door opens only for a leg that is
\* provably AT its cut — no solo bytes of its own, no divergence — which
\* the block tier can prove cheaply (the leg-export admission gate means
\* nothing else could have written the absent leg; the record pins the
\* cut).  The RejoinGuard transfer, verbatim in spirit.
DeltaRejoin ==
  LET s == record.composer
      d == Peer(s)
  IN
  /\ RejoinEnabled
  /\ ActiveNew
  /\ legSync[d] = "stale"
  /\ s \in legAdmit[d]
  /\ tgt[s] = "ok" /\ tgt[d] = "ok"
  /\ ~peerCut /\ s \notin apiCut
  /\ AncestryGuard => (~soloAcked[d] /\ ~diverged)
  /\ legSync' = [legSync EXCEPT ![d] = "insync"]
  /\ members' = members \cup {d}
  /\ soloAcked' = [soloAcked EXCEPT ![s] = FALSE]
  /\ diverged' = (diverged \/ soloAcked[d])
  /\ rejoined' = TRUE
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld,
                 owner, crashes, divergent, doomed, staleWrite, staleServe,
                 zombieWrote, splitRead>>
  /\ UNCHANGED cutVars

\* The door RecordRejoinOnly closes: the recovered node's auto-examine
\* wins seq_number arbitration (bdev_raid.c:3883-3904 — it only ever sees
\* its own leg) and declares its leg clean, corrupting the RECORD'S VIEW
\* with no copy having happened.  The election gate then trusts the lie in
\* good faith — the same shape as DegradeBlind, one layer up: a belt is
\* only as honest as the bookkeeping it reads.
SelfRejoin ==
  /\ RejoinEnabled /\ ~RecordRejoinOnly
  /\ \E t \in Tgts :
       /\ tgt[t] = "ok" /\ legSync[t] = "stale"
       /\ legSync' = [legSync EXCEPT ![t] = "insync"]
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld,
                 owner, crashes, divergent, doomed, staleWrite, staleServe,
                 zombieWrote, soloAcked, members, diverged, splitRead,
                 rejoined>>
  /\ UNCHANGED cutVars

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
  \* The redirect's answer ("where does the volume live NOW?") is a
  \* record read served by the composer's shard — a witness read.
  /\ record.composer \notin apiCut
  /\ session[c] # record.composer
  /\ session' = [session EXCEPT ![c] = record.composer]
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, excl,
                 fenced, pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

(***************************************************************************)
(* TRANCHE 4 — the control-plane failure events.  Cuts spend a budget      *)
(* like crashes spend theirs (a finite budget is also what keeps the       *)
(* heal-obligated liveness spec from churning forever); heals are free     *)
(* and carry NO fairness in SpecLive — a cut may persist, which is         *)
(* exactly what the NoWitness run exploits: a witness that never heals     *)
(* is a witness that never existed, i.e. the shipped two-sqlite world.     *)
(***************************************************************************)
CutWitness(t) ==
  /\ cuts < MaxCuts
  /\ t \notin apiCut
  /\ apiCut' = apiCut \cup {t}
  /\ cuts' = cuts + 1
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined,
                 peerCut, localStale>>

HealWitness(t) ==
  /\ t \in apiCut
  /\ apiCut' = apiCut \ {t}
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined,
                 peerCut, localStale, cuts>>

CutPeer ==
  /\ cuts < MaxCuts
  /\ ~peerCut
  /\ peerCut' = TRUE
  /\ cuts' = cuts + 1
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined,
                 apiCut, localStale>>

HealPeer ==
  /\ peerCut
  /\ peerCut' = FALSE
  /\ UNCHANGED <<tgt, record, serving, lastServed, legAdmit, lease, session,
                 excl, fenced, pendingEpoch, delivered, gen, clientHeld, owner,
                 crashes, divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined,
                 apiCut, localStale, cuts>>

(***************************************************************************)
(* THE UN-SUSPEND — added because the FIRST LiveWitness run demanded it:   *)
(* TLC produced a healthy composer, suspended by an API outage, parked     *)
(* FOREVER after the heal.  Legacy tranches never needed this action       *)
(* because the suspended-while-ok-and-seated state was unreachable there   *)
(* (the lapse required unreachable-or-deposed, and both roads back run     *)
(* through Recover or Assemble); the apiCut lapse creates it, so the       *)
(* protocol owes the road back out.  The implementation half already       *)
(* exists in the lease commit's own words — "a SUCCESSFUL renewal is what  *)
(* repairs the expiry", suspension is a converge MODE — this action is     *)
(* that sentence, with the guards that make it safe:                       *)
(*   - only the CURRENT composer (a deposed node's renewal is refused at   *)
(*     the witness — post-CAS this action is disabled, the rv refusal      *)
(*     again), and                                                         *)
(*   - only at an epoch it actually ASSEMBLED (an elected-but-never-       *)
(*     assembled composer must come up through Assemble's horizon and      *)
(*     eviction guards, never through a lease repair).                     *)
(* Promotion cannot race it: a pure apiCut is not a verdict (the peer's    *)
(* data-path probe still succeeds), so while this action is enabled no     *)
(* CAS is justified; once a CAS lands, the seat has moved and the guard    *)
(* fails.  The serialization is the witness's, one more time.              *)
(***************************************************************************)
ResumeServing ==
  LET t == record.composer IN
  /\ tgt[t] = "ok"
  /\ t \notin apiCut
  /\ lease[t] = "lapsed"
  /\ serving[t] = 0
  /\ lastServed[t] = record.epoch
  /\ lease' = [lease EXCEPT ![t] = "live"]
  /\ serving' = [serving EXCEPT ![t] = record.epoch]
  /\ UNCHANGED <<tgt, record, lastServed, legAdmit, session, excl, fenced,
                 pendingEpoch, delivered, gen, clientHeld, owner, crashes,
                 divergent, doomed, staleWrite, staleServe, zombieWrote,
                 legSync, soloAcked, members, diverged, splitRead, rejoined>>
  /\ UNCHANGED cutVars

Next ==
  \/ \E t \in Tgts : Partition(t) \/ Die(t) \/ Recover(t)
                       \/ LeaseLapse(t) \/ DeadmanFire(t)
                       \/ CutWitness(t) \/ HealWitness(t)
  \/ PromoteCAS \/ EvictAtLeg \/ Assemble \/ MarkStale \/ ResumeServing
  \/ \E c \in Clients : FenceClient(c) \/ PreemptExecute(c)
                          \/ DeliveredMark(c) \/ ClientWrite(c)
                          \/ ClientRead(c) \/ ReAttach(c)
  \/ RegrantRange
  \/ DieMidWrite \/ RebuildStart \/ RebuildComplete \/ DeltaRejoin \/ SelfRejoin
  \/ CutPeer \/ HealPeer

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

\* Tranche 2's theorem: no read is ever served through a composition whose
\* member legs diverge.  raid1 cannot state this about itself — it has no
\* scrub, no dirty flag, and its balancer makes every such read a coin
\* flip — so the belts that carry it are all flint's: UncleanResync (come
\* back solo after a torn death), AncestryGuard (no delta door for a leg
\* with bytes of its own), and the full rebuild as the only divergence
\* eraser.
Inv_NoSplitRead == ~splitRead

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

\* Tranche-2 witnesses:
\* a stale leg really re-enters the composition (either rejoin door) —
\* without this, every NoSplitRead green is compatible with "nothing ever
\* rejoins", the quarantine vacuity lesson again;
ProbeRejoinCompletes == ~rejoined
\* a torn composer death really occurs (UncleanResync's subject exists);
ProbeTornReachable == ~diverged
\* and a full FAIL-BACK really completes: promote away, rebuild the old
\* leg, lose the survivor, promote back — the record machine's round trip.
ProbeFailBackCompletes == ~(record.epoch = 3 /\ ActiveNew)

\* Tranche-4 witnesses:
\* a COMPLETE failover with ZERO process failures — a pure peer-cut
\* condemns, the witness serializes, and the survivor serves.  This is
\* the non-vacuity of the whole new axis: without it, every green in the
\* cut world is compatible with "cuts never do anything";
ProbeCutFailoverCompletes == ~(record.epoch >= 2 /\ ActiveNew /\ crashes = 0)
\* and THE BILL, collected: a perfectly healthy composer, suspended,
\* because only its path to the witness failed.  TLC must produce this
\* state — the etcd decision's honest cost as a reachability witness
\* (the WaitsPrice pattern in probe form; in the run's cfg MaxCrashes = 0
\* so the suspension is attributable to the cut alone).
ProbeHealthySuspend == ~(/\ tgt[record.composer] = "ok"
                         /\ record.composer \in apiCut
                         /\ serving[record.composer] = 0)
\* The OTHER order of the symmetric-partition race: the composer's stale
\* mark lands FIRST, the seat still un-CAS'd at epoch 1 — from here the
\* peer's election is refused by ElectInSync reading the fresh mark.
\* ProbeCutFailover witnesses the CAS-first order; the pair together is
\* what licenses the claim that the Witness strict green explored BOTH
\* sides of the race the witness serializes (a probe must name the
\* action's effect, the void-A2 lesson).
ProbeMarkFirst == ~(peerCut /\ record.epoch = 1 /\ legSync[Deposed] = "stale")

(***************************************************************************)
(* TRANCHE 3 — LIVENESS.  Progress is claimed in the POST-STORM QUIET      *)
(* (crashes = MaxCrashes conditions every antecedent — under a crash       *)
(* budget "transiently unavailable forever" is unrepresentable, the        *)
(* WriterLimbo lesson, so the honest claim is: once the failures stop,     *)
(* the machine finishes).  Fairness is placed on exactly the components    *)
(* that are RETRIED LOOPS in the design: the MDS reconcile pipeline        *)
(* (promotion, eviction, assembly, fence preempt + mark, rebuild, the      *)
(* stale-mark report), the lease timers, and — separately, because it      *)
(* DOES NOT EXIST in the code yet — the redirect actor.  SpecNoRedirect    *)
(* is SpecLive minus that one obligation: the shipped world, where the     *)
(* csi-node session record replays the dead traddr with a deliberate       *)
(* "No MDS call".  Its run must FAIL, and that lasso is the review's       *)
(* "redirect has no actor" finding as a machine-checked counterexample —   *)
(* the SpecNoP4 pattern.                                                   *)
(*                                                                         *)
(* Failure events, Recover, FenceClient, client IO, RegrantRange and       *)
(* DeltaRejoin carry NO fairness: failures are not obligated, a dead node  *)
(* may stay dead, fences and grants happen on demand, and the delta door   *)
(* is an optimization — the FULL rebuild carries the rejoin obligation.    *)
(***************************************************************************)

FairnessCtl ==
  /\ WF_vars(PromoteCAS)
  /\ WF_vars(EvictAtLeg)
  /\ WF_vars(Assemble)
  /\ WF_vars(MarkStale)
  \* The un-suspend is the converge loop's work like the rest of the
  \* pipeline.  Vacuous in every legacy graph (the action is never
  \* enabled there — the suspended-while-seated state needs an apiCut),
  \* so legacy liveness runs keep their exact obligations.
  /\ WF_vars(ResumeServing)
  /\ WF_vars(RebuildStart)
  /\ WF_vars(RebuildComplete)
  /\ \A t \in Tgts : WF_vars(LeaseLapse(t))
  /\ \A c \in Clients : WF_vars(PreemptExecute(c)) /\ WF_vars(DeliveredMark(c))

SpecLive       == Spec /\ FairnessCtl /\ \A c \in Clients : WF_vars(ReAttach(c))
SpecNoRedirect == Spec /\ FairnessCtl

\* TRANCHE 4's live spec: SpecLive plus the obligation that CONTROL-PLANE
\* CUTS HEAL — the K8s API comes back.  Progress in the witness world is
\* promised on exactly that premise and no weaker one; SpecLive WITHOUT
\* it is the NoWitness run, whose lasso (survivor healthy, in-sync, and
\* witnessless forever) is the shipped two-sqlite world's permanent form
\* — the 9fcdd05 finding as a machine-checked counterexample, the NoActor
\* pattern: specify a missing thing as a fairness obligation you withhold.
SpecLiveW == SpecLive /\ (\A t \in Tgts : WF_vars(HealWitness(t)))
                      /\ WF_vars(HealPeer)

\* Once the failures stop: an unreachable composer with an in-sync,
\* healthy peer is always failed over — the record moves and the survivor
\* serves (or the composer itself comes back, which also re-activates the
\* composition; both roads end at ActiveNew).  The legSync conjunct is
\* NOT a dodge, it is ElectInSync's honest scope — see
\* PromotionCompletesUnconditional below, which drops it and must FAIL.
PromotionCompletes ==
  [](( /\ tgt[record.composer] # "ok"
       /\ crashes = MaxCrashes
       /\ tgt[Deposed] = "ok"
       /\ legSync[Deposed] = "insync"
       /\ record.epoch < MaxEpoch )
     => <>ActiveNew)

\* THE PRICE OF ElectInSync, stated so it cannot be forgotten: drop the
\* in-sync conjunct and the promise is FALSE — a degraded volume whose
\* composer then partitions is DOWN until that composer recovers, because
\* the election gate refuses the stale survivor and the suspended
\* composer can drive no rebuild.  The WaitsPrice run requires TLC to
\* produce this lasso: availability spent on durability, priced as a
\* counterexample.  The undesigned escape is the operator override
\* (FlintReplication's LastResortServe analog).
PromotionCompletesUnconditional ==
  [](( /\ tgt[record.composer] # "ok"
       /\ crashes = MaxCrashes
       /\ tgt[Deposed] = "ok"
       /\ record.epoch < MaxEpoch )
     => <>ActiveNew)

\* Once the failures stop and a composer serves reachably, every standing
\* fence is eventually CONFIRMED against the current composition — the
\* reconcile retry re-earns delivery at whichever tgt the record names.
\* This is the promise the constructor-traddr world breaks (StaticTraddr
\* run): a preempt pinned to the original node can never confirm after a
\* failover, delivered stays 0, and the quarantine sweep parks forever.
FenceEventuallyConfirmed ==
  \A c \in Clients :
    [](( /\ c \in fenced
         /\ crashes = MaxCrashes
         /\ tgt[record.composer] = "ok" )
       => <>(delivered[c] = record.epoch))

\* Once the failures stop and the new composition serves, every client is
\* eventually re-attached to it.  This obligation belongs to the REDIRECT
\* ACTOR — the component the review found does not exist (the csi-node
\* session record replays the recorded traddr, AttachBlockNode answers
\* static per-shard config, and the only notify sender is expand).
\* SpecLive grants the actor WF; SpecNoRedirect withholds it and the
\* NoActor run must produce the parked-client lasso.  When the actor is
\* built, this property is its acceptance test.
ClientEventuallyRedirected ==
  \A c \in Clients :
    [](( /\ ActiveNew
         /\ session[c] # record.composer
         /\ tgt[record.composer] = "ok"
         /\ crashes = MaxCrashes )
       => <>(session[c] = record.composer))

\* Once the failures stop, a stale leg on a healthy node behind an active
\* composer is eventually rebuilt and rejoined — the record-driven
\* rebuild is a retried reconcile task, not a hope.  The full-rebuild
\* door carries the obligation (DeltaRejoin is unfaired: an optimization
\* may be refused forever without breaking the promise).
StaleLegEventuallyRejoins ==
  [](( /\ legSync[Deposed] = "stale"
       /\ tgt[record.composer] = "ok"
       /\ tgt[Deposed] = "ok"
       /\ ActiveNew
       /\ crashes = MaxCrashes )
     => <>(legSync[Deposed] = "insync"))

================================================================================
