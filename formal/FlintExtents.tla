----------------------------- MODULE FlintExtents -----------------------------
(***************************************************************************)
(* The pNFS BLOCK-LAYOUT extent allocator — grant/fence/reuse core.        *)
(*                                                                         *)
(* docs/plans/pnfs-block-layout-design.md §9 is this module's spec; this   *)
(* file is TRANCHE 1 of it, deliberately: the grant lifecycle, the recall/ *)
(* fence machinery, physical block reuse, and the target-side reservation  *)
(* state.  It models code that DOES NOT EXIST YET — that is the point (the *)
(* module and its drills come before the allocator, the way FlintTruncate  *)
(* would have wanted to come before F65) — so every green here is a        *)
(* statement about the DESIGN, and the design must be implemented against  *)
(* these runs, not vice versa.                                             *)
(*                                                                         *)
(* THE HEADLINE HAZARD, and why the WRITE is the theorem: a block-layout   *)
(* client holds raw LBAs and writes them over NVMe/TCP with the MDS        *)
(* nowhere on the path.  When the MDS frees a client's extent and hands    *)
(* the physical range to a new owner, a stale holder that never heard the  *)
(* recall corrupts the NEW owner's committed bytes.  FlintTruncate's       *)
(* read-only shape was right for its layer; copied here it would be a      *)
(* blind spot, because the entire point of RFC 8154/9561 fencing is        *)
(* stopping WRITES.  Inv_NoStaleExtentWrite is co-equal with the read      *)
(* theorem and listed first.                                               *)
(*                                                                         *)
(* TRANCHE 1 FINDING — THE DOC'S OWN ARM SET WAS INSUFFICIENT.  §9 as      *)
(* first written closed the grant-vs-reclaim races with two grant-side     *)
(* belts: RecallBlocksGrant (refuse at the gate check while a reclaim is   *)
(* in flight) and PublishRecheck (re-read the reclaim after publishing,    *)
(* self-revoke — C6's shape, inherited from FlintTruncate).  Working the   *)
(* two-step honesty rule through THIS module's actions shows a window      *)
(* neither can close: the reclaim snapshots its holders BEFORE a racing    *)
(* grant publishes, and can complete-and-free BETWEEN that grant's insert  *)
(* and its recheck — the recheck then reads reclaim.active = FALSE and     *)
(* revokes nothing, and the blocks are already free under a live grant.    *)
(* FlintTruncate survives the analogous interleave only because its        *)
(* "free" (the DS set_len fanout) destroys the CONTENT, so a layout that   *)
(* slips through reads truncated-to-zero bytes, not a new owner's; an      *)
(* extent free destroys NOTHING — the harm arrives with the next owner.    *)
(* The repair is on the FREE side: the free transaction re-validates the   *)
(* grants table (sqlite-native — the free and the grant insert execute     *)
(* over the same tables) and refuses while a live unfenced grant covers    *)
(* any block.  That is the FreeRevalidates arm, and                        *)
(* FlintExtentsStaleSnapshotFree.cfg pins the refuted bookkeeping-only     *)
(* design permanently, MarkOverwrite-style.  With FreeRevalidates          *)
(* carrying safety, PublishRecheck and RecallBlocksGrant become PROGRESS   *)
(* arms (they keep the reclaim from wedging behind grants it must then     *)
(* fence) — they are DEFERRED to the liveness tranche, where they get      *)
(* teeth an invariant cannot give them.                                    *)
(*                                                                         *)
(* WHAT THIS TRANCHE MODELS:                                               *)
(*   - Physical blocks over a small domain; extents are BLOCK-SETS, not    *)
(*     atomic identities (the scalar-raidHost lesson: fixed extent         *)
(*     identities make range aliasing unrepresentable).                    *)
(*   - gen[b], bumped on every free->provisional edge: the reuse detector. *)
(*     Zeros from a fresh extent are not stale content; bytes from a       *)
(*     PREVIOUS OWNER are — which is why gen, and not content-emptiness,   *)
(*     carries both theorems.                                              *)
(*   - Two-step LAYOUTGET (GrantCheck / GrantInsert) because the           *)
(*     implementation will be two steps: an in-memory gate read, then a    *)
(*     sqlite transaction.  Collapsing them is what made FlintTruncate     *)
(*     green on a property the code did not hold.                          *)
(*   - The reclaim (recall-then-free) with its holder SNAPSHOT taken at    *)
(*     start — check-then-act modelled as the code will actually be —      *)
(*     plus ReclaimResnapshot, the retry loop's separate re-read step      *)
(*     (the F62 idiom: the re-read must be its own action so TLC can       *)
(*     interleave against it).                                             *)
(*   - resv, the TARGET-side reservation registry, as REAL STATE: the set  *)
(*     of clients whose registrations have been preempted (RTYPE=4h EARO:  *)
(*     a preempted host's reads AND writes are refused).  All clients      *)
(*     start registered; TgtRestart without PTPL clears the reservation    *)
(*     and every exclusion with it — spdk-tgt is the most-restarted        *)
(*     component in the system (FlintReplication proved it), and without   *)
(*     this action §5's "PTPL is mandatory" cannot be STATED, let alone    *)
(*     checked.                                                            *)
(*                                                                         *)
(* FIX ARMS (TRUE = belt exists), each with a run that fails without it:   *)
(*   GrantsExclusive      the grant transaction validates physical         *)
(*                        disjointness against live grant rows.  The       *)
(*                        mutation is §8's recorded landmine: the PK does  *)
(*                        not police overlap.                              *)
(*   RecallBeforeReuse    frees happen only through the reclaim (recall    *)
(*                        every holder, wait for return-or-fence).  The    *)
(*                        mutation frees directly under live grants — the  *)
(*                        F65-of-extents.                                  *)
(*   FreeRevalidates      the free transaction re-validates holders        *)
(*                        (tranche-1 finding, above).                      *)
(*   FenceReaches         a fence lands as a real exclusion at the target. *)
(*                        KEEP FALSE IN THE SHIPPED CFG until proven       *)
(*                        against real spdk-tgt reservation behaviour on   *)
(*                        real hardware, and re-justify it every time the  *)
(*                        code moves — a constant encoding an assumption   *)
(*                        silently becomes a lie.  This is RecallReaches'  *)
(*                        analog and carries the same residual: both       *)
(*                        theorems are EXPECTED FALSE while it is FALSE,   *)
(*                        and the shipped cfg does not list them, exactly  *)
(*                        as FlintTruncate.cfg does not list               *)
(*                        Inv_NoStaleServe.                                *)
(*   PersistReservations  PTPL: the exclusion registry survives            *)
(*                        TgtRestart.  §5 calls PTPL mandatory;            *)
(*                        FlintExtentsTgtAmnesia.cfg is that sentence      *)
(*                        with teeth.                                      *)
(*                                                                         *)
(* ABSTRACTIONS, STATED (the module's THE-ABSTRACTION-WAS-THE-BUG          *)
(* surface):                                                               *)
(*   1. One volume, one namespace, one target.  Multi-target/trunking is   *)
(*      scenery until the code has it; resv would become a per-target      *)
(*      function then ("if two tgt incarnations can ever expose one        *)
(*      extent range, the serving-target state is a SET from day one").    *)
(*   2. One grant per client, whole-grant recalls.  Range granularity of   *)
(*      recalls changes WHICH grant a reclaim waits on, not whether the    *)
(*      snapshot/free machinery is sound; per-range recall records are     *)
(*      the allocator tranche's business.                                  *)
(*   3. Harm is gen-mismatch, NOT fenced-writing.  The doc's one-line      *)
(*      sketch had staleWrite fire on `client \in fenced` too; that is     *)
(*      deliberately NOT modelled, because a fenced holder writing its     *)
(*      own not-yet-freed blocks is EXPECTED under §8's                    *)
(*      quarantine-not-free GC and absorbed by it — firing the ghost       *)
(*      there would make LostFence's counterexample a technicality        *)
(*      instead of a corruption.  The ghost fires exactly when bytes       *)
(*      land (or are read) across an ownership generation.                 *)
(*   4. A read/write is ATOMIC w.r.t. fencing — nothing here says what     *)
(*      happens to I/O already inside the target when the preempt lands;   *)
(*      that is NVMe-level ordering, checked on hardware, never here.      *)
(*   5. Client crash is not distinguished from client silence: a client    *)
(*      that never returns IS the unresponsive holder the fence path       *)
(*      exists for.  ClientCrash as a separate action arrives with lease   *)
(*      machinery in a later tranche.                                      *)
(*   6. Byte durability at an extent is an axiom WITH NO LICENSE — block-  *)
(*      class volumes are single-replica lvols and no formal durability    *)
(*      story exists until server-side replication lands (§12).  Citing    *)
(*      a green here as a durability claim would be the axiom laundering   *)
(*      itself.                                                            *)
(*                                                                         *)
(* OWED (the allocator tranche, in lockstep with the sqlite schema):       *)
(* LAYOUTCOMMIT and the "committed" alloc state (+ CommitGatesSize,        *)
(* CommitChecksGen, ProvisionalInvisible, fsize, the zeroRead ghost /      *)
(* Inv_SizeCommitCoupled — the F67 shape); MdsCrash/MdsRestart + durable   *)
(* (PersistGrants, RecoverConservative); the MDS fallback lane             *)
(* (FallbackChecksGrants — the MDS as data-path actor); Split/Merge +      *)
(* SplitKeepsDisjoint + Inv_NoPhysicalAliasing; PublishRecheck /           *)
(* RecallBlocksGrant with LIVENESS teeth; quarantine as distinct state.    *)
(* The check-tla.sh matrix grows with each; the header count moves with    *)
(* it, as a named deliverable.                                             *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Clients,             \* block-layout clients (SPDK or kernel initiators)
  NBlocks,             \* physical domain: blocks are 1..NBlocks
  MaxGrants,           \* LAYOUTGET budget (GrantCheck starts)
  MaxReclaims,         \* reclaim / direct-free budget
  MaxTgtRestarts,      \* spdk-tgt restart budget
  GrantsExclusive,     \* TRUE = the grant transaction polices disjointness
  RecallBeforeReuse,   \* TRUE = frees only through the reclaim machinery
  FreeRevalidates,     \* TRUE = the free transaction re-validates holders
  FenceReaches,        \* TRUE = every fence lands as a target exclusion
  PersistReservations  \* TRUE = PTPL: exclusions survive TgtRestart

VARIABLES
  alloc,          \* [Blocks -> {"free","provisional"}] physical allocation
  gen,            \* [Blocks -> Nat] bumped on every free->provisional edge
  grants,         \* [Clients -> [live, held, g]] published layout grants;
                  \*   live is the CLIENT's view (it holds the layout until
                  \*   it returns it) — MDS-side death is `fenced`
  granting,       \* [Clients -> [open, blks]] the checked-but-unpublished
                  \*   two-step window
  reclaim,        \* [active, blks, waiting] the recall-then-free job;
                  \*   waiting is the holder SNAPSHOT taken at start
  fenced,         \* SUBSET Clients — MDS-side revocation bookkeeping
  resv,           \* SUBSET Clients — clients PREEMPTED at the target
                  \*   (EARO: their reads and writes are refused there)
  nGrants, nReclaims, nRestarts,   \* budgets
  \* Ghosts, single-writer each (the A2Probe standing rule):
  staleRead,      \* ClientRead crossed an ownership generation
  staleWrite,     \* ClientWrite crossed an ownership generation — THE theorem
  reuseFired,     \* GrantInsert re-cycled a previously-owned block
  fenceFired,     \* Fence executed
  tgtRestarted,   \* TgtRestart executed
  resnapshotGrew  \* ReclaimResnapshot found holders the snapshot missed

vars == <<alloc, gen, grants, granting, reclaim, fenced, resv,
          nGrants, nReclaims, nRestarts,
          staleRead, staleWrite, reuseFired, fenceFired, tgtRestarted,
          resnapshotGrew>>

Blocks == 1..NBlocks
GenBound == MaxReclaims + 1        \* one initial bump + one per free
Ranges == (SUBSET Blocks) \ {{}}

ZeroG == [b \in Blocks |-> 0]
NoGrant == [live |-> FALSE, held |-> {}, g |-> ZeroG]
NoWindow == [open |-> FALSE, blks |-> {}]
NoReclaim == [active |-> FALSE, blks |-> {}, waiting |-> {}]

\* Live holders of a block / of any block in a range — fenced INCLUDED:
\* a fenced grant's row is dead to the MDS but its blocks are not
\* re-grantable until FREED (§8's quarantine discipline, simplified: the
\* blocks pass through the reclaim before any new owner sees them).
HeldBy(b) == {c \in Clients : grants[c].live /\ b \in grants[c].held}
LiveHolders(R) == {c \in Clients : grants[c].live /\ grants[c].held \cap R # {}}

TypeOK ==
  /\ alloc \in [Blocks -> {"free", "provisional"}]
  /\ gen \in [Blocks -> 0..GenBound]
  /\ grants \in [Clients ->
       [live : BOOLEAN, held : SUBSET Blocks, g : [Blocks -> 0..GenBound]]]
  /\ granting \in [Clients -> [open : BOOLEAN, blks : SUBSET Blocks]]
  /\ reclaim \in
       [active : BOOLEAN, blks : SUBSET Blocks, waiting : SUBSET Clients]
  /\ fenced \subseteq Clients
  /\ resv \subseteq Clients
  /\ nGrants \in 0..MaxGrants
  /\ nReclaims \in 0..MaxReclaims
  /\ nRestarts \in 0..MaxTgtRestarts
  /\ staleRead \in BOOLEAN /\ staleWrite \in BOOLEAN
  /\ reuseFired \in BOOLEAN /\ fenceFired \in BOOLEAN
  /\ tgtRestarted \in BOOLEAN /\ resnapshotGrew \in BOOLEAN
  \* Structure: g is normalised to 0 outside held (state-space hygiene and
  \* a modelling-bug tripwire, not a claim about the code).
  /\ \A c \in Clients : \A b \in Blocks :
       b \notin grants[c].held => grants[c].g[b] = 0

Init ==
  /\ alloc = [b \in Blocks |-> "free"]
  /\ gen = ZeroG
  /\ grants = [c \in Clients |-> NoGrant]
  /\ granting = [c \in Clients |-> NoWindow]
  /\ reclaim = NoReclaim
  /\ fenced = {} /\ resv = {}
  /\ nGrants = 0 /\ nReclaims = 0 /\ nRestarts = 0
  /\ staleRead = FALSE /\ staleWrite = FALSE
  /\ reuseFired = FALSE /\ fenceFired = FALSE
  /\ tgtRestarted = FALSE /\ resnapshotGrew = FALSE

(***************************************************************************)
(* The MDS: grants                                                         *)
(***************************************************************************)

\* LAYOUTGET step 1: the in-memory gate read.  A range is requestable if
\* every block is free or an orphan (provisional with no live holder — a
\* returned grant's uncommitted extents, re-grantable until reclaimed).
\* No reclaim-awareness here in tranche 1: RecallBlocksGrant is deferred
\* to the liveness tranche, and safety must not depend on it (the finding).
GrantCheck(c, R) ==
  /\ nGrants < MaxGrants
  /\ ~grants[c].live
  /\ ~granting[c].open
  /\ c \notin fenced        \* no re-admission before lease recovery (owed)
  /\ \A b \in R : alloc[b] = "free" \/ HeldBy(b) = {}
  /\ granting' = [granting EXCEPT ![c] = [open |-> TRUE, blks |-> R]]
  /\ nGrants' = nGrants + 1
  /\ UNCHANGED <<alloc, gen, grants, reclaim, fenced, resv,
                 nReclaims, nRestarts, staleRead, staleWrite, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew>>

\* LAYOUTGET step 2: the sqlite transaction.  With GrantsExclusive it
\* re-validates disjointness INSIDE the transaction and refuses a range
\* the check approved on a stale read — two windows racing to the same
\* free block lose exactly one of them here.  The mutation is the
\* PK-does-not-police-overlap landmine: the insert trusts the check, and
\* two live grants share a block.
\*
\* WHAT THE TRANSACTION CAN SEE — and the limit that makes FreeRevalidates
\* load-bearing: disjointness is policed over the extent/grant ROWS, and a
\* freed block has LEFT the tables.  So `occupied` is "an allocated block
\* with a live holder", never "any block some client still believes it
\* holds" — no grant-time check can protect against a free that should not
\* have happened, because the free is precisely the step that destroys the
\* evidence.  The protection for that belongs to the free side, which is
\* the tranche-1 finding.
GrantInsert(c) ==
  /\ granting[c].open
  /\ LET R == granting[c].blks
         occupied == \E b \in R : alloc[b] = "provisional" /\ HeldBy(b) # {}
     IN IF GrantsExclusive /\ occupied
        THEN \* transaction refuses; the window closes, nothing published
          /\ granting' = [granting EXCEPT ![c] = NoWindow]
          /\ UNCHANGED <<alloc, gen, grants, reclaim, fenced, resv,
                         nGrants, nReclaims, nRestarts, staleRead,
                         staleWrite, reuseFired, fenceFired, tgtRestarted,
                         resnapshotGrew>>
        ELSE
          LET fresh == {b \in R : alloc[b] = "free"}
              gen2 == [b \in Blocks |->
                        IF b \in fresh THEN gen[b] + 1 ELSE gen[b]]
          IN
          /\ alloc' = [b \in Blocks |->
                        IF b \in fresh THEN "provisional" ELSE alloc[b]]
          /\ gen' = gen2
          /\ grants' = [grants EXCEPT ![c] =
               [live |-> TRUE, held |-> R,
                g |-> [b \in Blocks |-> IF b \in R THEN gen2[b] ELSE 0]]]
          /\ granting' = [granting EXCEPT ![c] = NoWindow]
          /\ reuseFired' = (reuseFired \/ \E b \in fresh : gen[b] > 0)
          /\ UNCHANGED <<reclaim, fenced, resv, nGrants, nReclaims,
                         nRestarts, staleRead, staleWrite, fenceFired,
                         tgtRestarted, resnapshotGrew>>

(***************************************************************************)
(* The MDS: reclaim (recall-then-free)                                     *)
(***************************************************************************)

\* The belted free path opens: pick allocated blocks, SNAPSHOT the live
\* unfenced holders, recall them (recall delivery is not modelled as an
\* arm — a lost recall is indistinguishable from a slow client in a
\* safety model, and the fence is the designed backstop for both).
ReclaimStart(R) ==
  /\ RecallBeforeReuse
  /\ nReclaims < MaxReclaims
  /\ ~reclaim.active
  /\ R \subseteq {b \in Blocks : alloc[b] = "provisional"}
  /\ R # {}
  /\ reclaim' = [active |-> TRUE, blks |-> R,
                 waiting |-> LiveHolders(R) \ fenced]
  /\ nReclaims' = nReclaims + 1
  /\ UNCHANGED <<alloc, gen, grants, granting, fenced, resv, nGrants,
                 nRestarts, staleRead, staleWrite, reuseFired, fenceFired,
                 tgtRestarted, resnapshotGrew>>

\* The retry loop's separate re-read step (the F62 idiom): recompute the
\* holder set from the live tables.  This is what eventually surfaces a
\* grant the start-time snapshot missed — the ghost records exactly that
\* event, and its probe is the non-vacuity licence for FreeRevalidates'
\* green (the belt refuses frees precisely when this set is non-empty).
ReclaimResnapshot ==
  /\ reclaim.active
  /\ LET w2 == LiveHolders(reclaim.blks) \ fenced
     IN /\ reclaim' = [reclaim EXCEPT !.waiting = w2]
        /\ resnapshotGrew' =
             (resnapshotGrew \/ (reclaim.waiting = {} /\ w2 # {}))
  /\ UNCHANGED <<alloc, gen, grants, granting, fenced, resv, nGrants,
                 nReclaims, nRestarts, staleRead, staleWrite, reuseFired,
                 fenceFired, tgtRestarted>>

\* An unresponsive holder is fenced: revoked server-side (bookkeeping in
\* `fenced` — its grant row is dead to the MDS) and preempted at the
\* target.  The ∃ is the honest arm: the MDS PROCEEDS BELIEVING THE FENCE
\* LANDED EITHER WAY, and FenceReaches = FALSE lets the exclusion silently
\* not exist — RecallReaches' analog, kept FALSE in the shipped cfg until
\* spdk-tgt reservation enforcement is proven on real hardware.
Fence(c) ==
  /\ reclaim.active
  /\ c \in reclaim.waiting
  /\ fenced' = fenced \cup {c}
  /\ \E landed \in BOOLEAN :
       /\ FenceReaches => landed
       /\ resv' = IF landed THEN resv \cup {c} ELSE resv
  /\ reclaim' = [reclaim EXCEPT !.waiting = @ \ {c}]
  /\ fenceFired' = TRUE
  /\ UNCHANGED <<alloc, gen, grants, granting, nGrants, nReclaims,
                 nRestarts, staleRead, staleWrite, reuseFired,
                 tgtRestarted, resnapshotGrew>>

\* The free.  Every snapshotted holder has returned or been fenced; with
\* FreeRevalidates the transaction ALSO re-validates the live tables and
\* refuses while any live unfenced grant covers a block — the tranche-1
\* finding: without this, a grant the snapshot missed is freed under
\* (FlintExtentsStaleSnapshotFree.cfg), and no grant-side belt closes it.
ReclaimComplete ==
  /\ reclaim.active
  /\ reclaim.waiting = {}
  /\ FreeRevalidates =>
       \A b \in reclaim.blks : HeldBy(b) \subseteq fenced
  /\ alloc' = [b \in Blocks |->
                IF b \in reclaim.blks THEN "free" ELSE alloc[b]]
  /\ reclaim' = NoReclaim
  /\ UNCHANGED <<gen, grants, granting, fenced, resv, nGrants, nReclaims,
                 nRestarts, staleRead, staleWrite, reuseFired, fenceFired,
                 tgtRestarted, resnapshotGrew>>

\* The unbelted world: RecallBeforeReuse = FALSE frees allocated blocks
\* outright, holders or no holders — the F65-of-extents.
FreeDirect(R) ==
  /\ ~RecallBeforeReuse
  /\ nReclaims < MaxReclaims
  /\ R \subseteq {b \in Blocks : alloc[b] = "provisional"}
  /\ R # {}
  /\ alloc' = [b \in Blocks |-> IF b \in R THEN "free" ELSE alloc[b]]
  /\ nReclaims' = nReclaims + 1
  /\ UNCHANGED <<gen, grants, granting, reclaim, fenced, resv, nGrants,
                 nRestarts, staleRead, staleWrite, reuseFired, fenceFired,
                 tgtRestarted, resnapshotGrew>>

(***************************************************************************)
(* The target                                                              *)
(***************************************************************************)

\* spdk-tgt restarts.  Without PTPL the reservation and every registration
\* die with the process: all exclusions are forgotten and every client —
\* including every fenced one — can reach the namespace again.
TgtRestart ==
  /\ nRestarts < MaxTgtRestarts
  /\ resv' = IF PersistReservations THEN resv ELSE {}
  /\ tgtRestarted' = TRUE
  /\ nRestarts' = nRestarts + 1
  /\ UNCHANGED <<alloc, gen, grants, granting, reclaim, fenced, nGrants,
                 nReclaims, staleRead, staleWrite, reuseFired, fenceFired,
                 resnapshotGrew>>

(***************************************************************************)
(* The clients — raw NVMe I/O under a held layout; the MDS is not on this  *)
(* path at all, which is the entire reason this module exists.             *)
(***************************************************************************)

ClientRead(c, b) ==
  /\ grants[c].live
  /\ b \in grants[c].held
  /\ c \notin resv                      \* EARO refuses a preempted host
  /\ staleRead' = (staleRead \/ gen[b] # grants[c].g[b])
  /\ UNCHANGED <<alloc, gen, grants, granting, reclaim, fenced, resv,
                 nGrants, nReclaims, nRestarts, staleWrite, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew>>

ClientWrite(c, b) ==
  /\ grants[c].live
  /\ b \in grants[c].held
  /\ c \notin resv
  /\ staleWrite' = (staleWrite \/ gen[b] # grants[c].g[b])
  /\ UNCHANGED <<alloc, gen, grants, granting, reclaim, fenced, resv,
                 nGrants, nReclaims, nRestarts, staleRead, reuseFired,
                 fenceFired, tgtRestarted, resnapshotGrew>>

LayoutReturn(c) ==
  /\ grants[c].live
  /\ grants' = [grants EXCEPT ![c] = NoGrant]
  /\ reclaim' = [reclaim EXCEPT !.waiting = @ \ {c}]
  /\ UNCHANGED <<alloc, gen, granting, fenced, resv, nGrants, nReclaims,
                 nRestarts, staleRead, staleWrite, reuseFired, fenceFired,
                 tgtRestarted, resnapshotGrew>>

Next ==
  \/ \E c \in Clients, R \in Ranges : GrantCheck(c, R)
  \/ \E c \in Clients : GrantInsert(c)
  \/ \E R \in Ranges : ReclaimStart(R)
  \/ ReclaimResnapshot
  \/ \E c \in Clients : Fence(c)
  \/ ReclaimComplete
  \/ \E R \in Ranges : FreeDirect(R)
  \/ TgtRestart
  \/ \E c \in Clients, b \in Blocks : ClientRead(c, b)
  \/ \E c \in Clients, b \in Blocks : ClientWrite(c, b)
  \/ \E c \in Clients : LayoutReturn(c)

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* Invariants                                                              *)
(***************************************************************************)

\* No two live unfenced grants share a physical block.  Fenced-vs-live
\* overlap is EXPECTED (fence-then-reuse is the design working); the
\* invariant scopes to grants the MDS still considers live.
Inv_NoConflictingGrants ==
  \A c1, c2 \in Clients :
    (/\ c1 # c2
     /\ grants[c1].live /\ c1 \notin fenced
     /\ grants[c2].live /\ c2 \notin fenced)
      => grants[c1].held \cap grants[c2].held = {}

\* No block moves through the allocator's lifecycle while a live unfenced
\* grant covers it: its generation never moves under the grant, and it is
\* never in the free state under the grant (freed-under-grant is already
\* the bug even before a new owner appears).
Inv_RecallCompletesBeforeReuse ==
  \A c \in Clients :
    (grants[c].live /\ c \notin fenced) =>
      \A b \in grants[c].held :
        /\ gen[b] = grants[c].g[b]
        /\ alloc[b] # "free"

(***************************************************************************)
(* THE THEOREMS — descendants of Inv_NoStaleServe, and the WRITE is        *)
(* first-class: a stale write corrupts the NEW owner's bytes, which is     *)
(* strictly worse than a stale read.  Both are EXPECTED FALSE while        *)
(* FenceReaches = FALSE (the shipped world) and are deliberately NOT       *)
(* listed in FlintExtents.cfg — listing them would be the model asserting  *)
(* a delivery the design has not demonstrated on hardware.                 *)
(* FlintExtentsTarget.cfg is the conditional green: what shipping the      *)
(* block layout REQUIRES, not what any code yet does.                      *)
(***************************************************************************)
Inv_NoStaleExtentWrite == ~staleWrite
Inv_NoStaleExtentRead == ~staleRead

Inv == TypeOK /\ Inv_NoConflictingGrants /\ Inv_RecallCompletesBeforeReuse
InvTarget == Inv /\ Inv_NoStaleExtentWrite /\ Inv_NoStaleExtentRead

================================================================================
