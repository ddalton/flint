----------------------------- MODULE FlintSnapshots -----------------------------
(***************************************************************************)
(* The epoch-chain / delta-copy protocol — flint's snapshot machinery at   *)
(* BLOCK-CONTENT level.  FlintReplication models content as a write-set,   *)
(* which is deliberately too coarse for snapshots: every hazard this       *)
(* module checks is about OVERWRITES — which version of a block survives   *)
(* a chain walk.  Here content is [Blocks -> version] and the questions    *)
(* are the ones catchup.rs answers in prose:                               *)
(*                                                                         *)
(*   - An epoch cut snapshots the source head; the retained chain is the   *)
(*     shared history (apply_epoch_cut / the blobstore snapshot chain).    *)
(*   - A copy session walks the chain OLDEST-FIRST applying each epoch's   *)
(*     shallow delta — the blocks allocated IN that epoch (lineage_chain   *)
(*     collects, chain.reverse() orders, one shallow copy per epoch).      *)
(*   - A full build walks from the chain root; a based session starts     *)
(*     from the target's shared base epoch and walks the suffix.  A base   *)
(*     that aged out of retention is DEMOTED to a full rebuild             *)
(*     (LINEAGE_NOT_COVERED — the walk cannot even index it).              *)
(*   - Retention drops the OLDEST epoch; SPDK blobstore deletion of a      *)
(*     snapshot ABSORBS its clusters into the surviving child             *)
(*     (delete_snapshot re-parents, lib/blob/blobstore.c) — so the new     *)
(*     oldest epoch's delta is everything since the original baseline.     *)
(*                                                                         *)
(* THE THEOREM (Inv_SessionFaithful): every COMPLETED copy session leaves  *)
(* the target holding exactly the cut's content.  Sessions are atomic     *)
(* here — a session that errors mid-walk (source churn, vanished epoch)   *)
(* simply does not complete, and crash-inside-a-session is the crash-     *)
(* sweep sim harness's job, not this module's.                             *)
(*                                                                         *)
(* Three mutations, each a documented hazard, each of which TLC must      *)
(* catch (scripts/check-tla.sh):                                           *)
(*                                                                         *)
(*   WalkFull = FALSE — the DELTA-SPLIT bug (catchup.rs: "a snapshot       *)
(*     interleaved between two epochs splits the newer epoch's delta —     *)
(*     shallow copy moves only the top layer"): applying only the cut      *)
(*     epoch's delta silently loses every block whose last write was in a  *)
(*     middle epoch.                                                       *)
(*   OrderedWalk = FALSE — walk order violation (what chain.reverse()      *)
(*     exists for): applying epochs newest-first lets an older epoch's     *)
(*     version overwrite the newer one.                                    *)
(*   RelinkOnDelete = FALSE — the FINDING #1 class, content edition: a     *)
(*     bare delete of the oldest epoch (what the sim harness's fake used   *)
(*     to do; real SPDK re-parents/absorbs, refuses >1 clones -EBUSY)      *)
(*     loses the absorbed clusters — a later full build misses every       *)
(*     block whose last write lived only in the dropped epoch.             *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
  Blocks,          \* logical blocks, e.g. {b1, b2}
  MaxVer,          \* bound on the global version counter
  MaxEpochs,       \* max retained chain length
  MaxCuts,         \* total epoch-cut budget
  WalkFull,        \* TRUE = apply EVERY epoch's delta on the walk
  OrderedWalk,     \* TRUE = oldest-to-newest application order
  RelinkOnDelete   \* TRUE = retention drop absorbs clusters (blobstore)

VARIABLES
  ver,        \* global version counter — every write is a fresh version
  src,        \* [Blocks -> 0..MaxVer] live source head content
  chain,      \* retained epochs, oldest first: [id, content] records
  prevBase,   \* content baseline PRECEDING chain[1] (drop bookkeeping)
  nextId,     \* next epoch id
  cuts,       \* cut budget spent
  tgtBase,    \* 0 = no shared base; else the epoch id the target sits on
  tgtContent, \* [Blocks -> 0..MaxVer]
  synced,     \* a copy session completed (and nothing invalidated it)
  expected    \* ghost: the cut content that session promised

vars == <<ver, src, chain, prevBase, nextId, cuts, tgtBase, tgtContent,
          synced, expected>>

Zero == [b \in Blocks |-> 0]

TypeOK ==
  /\ ver \in 0..MaxVer
  /\ src \in [Blocks -> 0..MaxVer]
  /\ chain \in Seq([id : 1..(MaxCuts + 1), content : [Blocks -> 0..MaxVer]])
  /\ Len(chain) <= MaxEpochs
  /\ prevBase \in [Blocks -> 0..MaxVer]
  /\ nextId \in 1..(MaxCuts + 1)
  /\ cuts \in 0..MaxCuts
  /\ tgtBase \in 0..(MaxCuts + 1)
  /\ tgtContent \in [Blocks -> 0..MaxVer]
  /\ synced \in BOOLEAN
  /\ expected \in [Blocks -> 0..MaxVer]

Init ==
  /\ ver = 0
  /\ src = Zero
  /\ chain = <<>>
  /\ prevBase = Zero
  /\ nextId = 1
  /\ cuts = 0
  /\ tgtBase = 0
  /\ tgtContent = Zero
  /\ synced = FALSE
  /\ expected = Zero

MaxS(S) == CHOOSE x \in S : \A y \in S : x >= y
MinS(S) == CHOOSE x \in S : \A y \in S : x <= y

\* The shallow delta of retained epoch i: blocks allocated IN it — i.e.
\* changed relative to the preceding retained content.  For the oldest
\* retained epoch the predecessor is prevBase: with blobstore relink that
\* baseline never moves on a drop (the surviving epoch absorbed the
\* dropped clusters), so its delta is everything since the original root.
Alloc(i) ==
  LET prior == IF i = 1 THEN prevBase ELSE chain[i - 1].content
  IN {b \in Blocks : chain[i].content[b] # prior[b]}

\* One completed walk from `from`..`to` applied over `start` content.
\* Faithful mode: for each block, the NEWEST epoch that allocated it wins
\* (oldest-first application).  The mutations pick the split or the
\* reversed order instead.
ApplyWalk(start, from, to) ==
  [b \in Blocks |->
     IF ~WalkFull
       THEN IF b \in Alloc(to) THEN chain[to].content[b] ELSE start[b]
       ELSE LET hits == {i \in from..to : b \in Alloc(i)}
            IN IF hits = {} THEN start[b]
               ELSE IF OrderedWalk THEN chain[MaxS(hits)].content[b]
                                   ELSE chain[MinS(hits)].content[b]]

(***************************************************************************)
(* Source side                                                             *)
(***************************************************************************)

Write(b) ==
  /\ ver < MaxVer
  /\ ver' = ver + 1
  /\ src' = [src EXCEPT ![b] = ver + 1]
  /\ UNCHANGED <<chain, prevBase, nextId, cuts, tgtBase, tgtContent,
                 synced, expected>>

\* Epoch cut: snapshot the head onto the chain (apply_epoch_cut).
Cut ==
  /\ cuts < MaxCuts
  /\ Len(chain) < MaxEpochs
  /\ chain' = Append(chain, [id |-> nextId, content |-> src])
  /\ nextId' = nextId + 1
  /\ cuts' = cuts + 1
  /\ UNCHANGED <<ver, src, prevBase, tgtBase, tgtContent, synced, expected>>

\* Retention drop of the OLDEST epoch.  Blobstore semantics
\* (RelinkOnDelete): the surviving child absorbs the dropped clusters —
\* the baseline does not move, so Alloc(new oldest) still covers them.
\* The mutation (bare delete, the old sim-harness fake): the baseline
\* jumps to the dropped content and those clusters fall out of every
\* future walk.
Drop ==
  /\ Len(chain) >= 2
  /\ prevBase' = IF RelinkOnDelete THEN prevBase ELSE chain[1].content
  /\ chain' = Tail(chain)
  /\ UNCHANGED <<ver, src, nextId, cuts, tgtBase, tgtContent, synced,
                 expected>>

(***************************************************************************)
(* Target side — sessions are atomic (see header)                          *)
(***************************************************************************)

\* Full build: walk the whole retained chain from the root
\* (lineage_chain with no base — "everything from the root is collected").
CopyFull ==
  /\ chain # <<>>
  /\ LET to == Len(chain) IN
       /\ tgtContent' = ApplyWalk(Zero, 1, to)
       /\ tgtBase' = chain[to].id
       /\ expected' = chain[to].content
  /\ synced' = TRUE
  /\ UNCHANGED <<ver, src, chain, prevBase, nextId, cuts>>

\* Based session: the target sits on a RETAINED epoch (its content IS that
\* epoch's content — a based copy clones from the shared base snapshot,
\* not from a possibly-diverged head; divergent-head admission is
\* FlintReplication's RejoinGuard) and walks the suffix.  A base that is
\* no longer retained cannot even be indexed — the guard IS the
\* LINEAGE_NOT_COVERED demotion; Scrub is the demotion arm.
CopyBased ==
  /\ tgtBase # 0
  /\ \E j \in 1..Len(chain) :
       /\ chain[j].id = tgtBase
       /\ LET to == Len(chain) IN
            /\ tgtContent' = ApplyWalk(chain[j].content, j + 1, to)
            /\ tgtBase' = chain[to].id
            /\ expected' = chain[to].content
  /\ synced' = TRUE
  /\ UNCHANGED <<ver, src, chain, prevBase, nextId, cuts>>

\* The full-rebuild demotion: drop the (aged-out or absent) base and
\* start over (HotRejoinScrubbed / "delta demoted to a full rebuild").
ScrubTarget ==
  /\ tgtBase # 0 \/ synced
  /\ tgtBase' = 0
  /\ tgtContent' = Zero
  /\ synced' = FALSE
  /\ expected' = Zero
  /\ UNCHANGED <<ver, src, chain, prevBase, nextId, cuts>>

Next ==
  \/ \E b \in Blocks : Write(b)
  \/ Cut
  \/ Drop
  \/ CopyFull
  \/ CopyBased
  \/ ScrubTarget

Spec == Init /\ [][Next]_vars

(***************************************************************************)
(* THE invariant: a completed session delivered exactly the cut.  A       *)
(* single stale or missed block version here is, one level up, the        *)
(* split-read surface Inv_NoDivergentServing guards in                    *)
(* FlintReplication — this module is where the copy machinery earns the   *)
(* right to be trusted by that model's atomic CatchUp/Admit steps.        *)
(***************************************************************************)
Inv_SessionFaithful == synced => tgtContent = expected

\* Structural sanity: versions never regress along the retained chain.
Inv_ChainMonotone ==
  \A i \in 1..Len(chain), b \in Blocks :
    i = 1 \/ chain[i].content[b] >= chain[i - 1].content[b]

Inv == TypeOK /\ Inv_SessionFaithful /\ Inv_ChainMonotone

================================================================================
