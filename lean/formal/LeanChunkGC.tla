------------------------------ MODULE LeanChunkGC ------------------------------
(***************************************************************************)
(* Chunk garbage collection against a concurrent publisher and a reader.   *)
(* Spec of record: docs/plans/flint-lean-chunked-manifest-design.md §8.1.  *)
(* Model BEFORE the reaper exists — §9 of that doc says exactly that, and  *)
(* this is the moment it means.                                            *)
(*                                                                         *)
(* Why a separate module from LeanSubtree: the manifest there is ONE       *)
(* object per generation, so every object had exactly one referent and     *)
(* "delete what the live pointer does not name" was sound. Chunks are      *)
(* SHARED between generations. That single change is what makes the old    *)
(* reasoning not carry over, and it is the whole subject here.             *)
(*                                                                         *)
(* The load-bearing abstraction, and it is the point rather than a         *)
(* simplification: the GC's LIST is a SNAPSHOT, not ground truth. It is    *)
(* taken at one instant and acted on at another, and a publisher runs in   *)
(* between. Chunk age is the second sensor, and it can lie in a specific   *)
(* way — an ADOPTED chunk is old but live. Modelling either as fact would  *)
(* reproduce the mistake that once let a data-loss bug through this        *)
(* corpus: model the OBSERVATION, not only the state.                      *)
(*                                                                         *)
(* Deliberately out of scope: the boundary function (a pure function of    *)
(* its input — a model would restate the code and agree with it; it is     *)
(* property-tested instead), multi-writer merge, and the pointer CAS       *)
(* itself, which LeanSubtree already covers.                               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
  Addrs,            \* chunk addresses (model values)
  MaxPubs,          \* publish budget
  Retain,           \* how many superseded pointers the reaper keeps
  \* ---- protocol arms: TRUE = §8.1 as designed --------------------------
  ListBeforeRefs,   \* TRUE: list candidates, THEN union the retained refs
  AgeGrace,         \* TRUE: only chunks past the grace are collectable
  AdoptRewrites,    \* TRUE: a publisher finding a chunk present REWRITES it
  RefsAtDelete,     \* TRUE: the reference set is re-read AT the delete, not
                    \* carried from an earlier snapshot. The strict run
                    \* refuted the list-before-refs rule §8.1 named: what
                    \* matters is not which snapshot comes first, but that
                    \* refs must not predate a CAS the delete follows.
  GraceCoversPublish \* TRUE: the grace outlives the longest publish, so a
                     \* chunk cannot age out from under the publish that is
                     \* about to reference it. A TIMING assumption, stated as
                     \* an arm precisely because the strict run refuted the
                     \* structural argument that was supposed to replace it.

VARIABLES
  store,      \* [Addrs -> {"absent","fresh","aged"}]
  live,       \* [seq |-> Nat, chunks |-> SUBSET Addrs]
  retained,   \* set of superseded pointers, |retained| <= Retain
  pub,        \* [phase, target, done]
  rdr,        \* [phase, target, got]
  gc,         \* [phase, cand, refs]
  torn,       \* ghost: a reader found a chunk its pointer named, absent
  collected,  \* ghost: how many chunks the reaper deleted
  adopted,    \* ghost: a publisher referenced a chunk it did not upload
  npubs

vars == <<store, live, retained, pub, rdr, gc, torn, collected, adopted, npubs>>

NonEmpty == {S \in SUBSET Addrs : S # {}}

AllRefs == live.chunks \union UNION {p.chunks : p \in retained}

Trim(S) ==
  IF Cardinality(S) <= Retain
    THEN S
    ELSE S \ {CHOOSE r \in S : \A q \in S : r.seq <= q.seq}

Init ==
  /\ store = [a \in Addrs |-> "absent"]
  /\ live = [seq |-> 0, chunks |-> {}]
  /\ retained = {}
  /\ pub = [phase |-> "idle", target |-> {}, done |-> {}]
  /\ rdr = [phase |-> "idle", target |-> {}, got |-> {}]
  /\ gc  = [phase |-> "idle", cand |-> {}, refs |-> {}]
  /\ torn = FALSE
  /\ collected = 0
  /\ adopted = 0
  /\ npubs = 0

(* ---- publisher ------------------------------------------------------- *)

PubStart ==
  /\ pub.phase = "idle"
  /\ npubs < MaxPubs
  /\ \E T \in NonEmpty :
       pub' = [phase |-> "writing", target |-> T, done |-> {}]
  /\ UNCHANGED <<store, live, retained, rdr, gc, torn, collected, adopted, npubs>>

(* One chunk of the publish. The interesting arm is the ELSE: the object  *)
(* is already there (our own crashed attempt, or a concurrent writer that *)
(* produced identical content), so the publisher references it without    *)
(* uploading. That skip is where O(changed) comes from — and it is also   *)
(* how an object that is OLD becomes LIVE without looking any younger.    *)
PubWrite ==
  /\ pub.phase = "writing"
  /\ \E a \in pub.target \ pub.done :
       /\ IF store[a] = "absent"
            THEN /\ store' = [store EXCEPT ![a] = "fresh"]
                 /\ adopted' = adopted
            ELSE /\ IF AdoptRewrites
                      THEN store' = [store EXCEPT ![a] = "fresh"]
                      ELSE store' = store
                 /\ adopted' = adopted + 1
       /\ pub' = [pub EXCEPT !.done = pub.done \union {a}]
  /\ UNCHANGED <<live, retained, rdr, gc, torn, collected, npubs>>

PubCas ==
  /\ pub.phase = "writing"
  /\ pub.done = pub.target
  /\ live' = [seq |-> live.seq + 1, chunks |-> pub.target]
  /\ retained' = Trim(retained \union {live})
  /\ pub' = [phase |-> "idle", target |-> {}, done |-> {}]
  /\ npubs' = npubs + 1
  /\ UNCHANGED <<store, rdr, gc, torn, collected, adopted>>

(* The crash §8.1 exists for: chunks are durable, the pointer CAS never   *)
(* happens, and what is left is an aged object no pointer references.     *)
(* WITHOUT this action the model cannot produce an orphan at all, and the *)
(* adoption arm it is meant to exercise is unreachable — the first run of *)
(* this module reported AdoptSkips as HOLDING for exactly that reason.    *)
PubCrash ==
  /\ pub.phase = "writing"
  /\ pub' = [phase |-> "idle", target |-> {}, done |-> {}]
  /\ npubs' = npubs + 1
  /\ UNCHANGED <<store, live, retained, rdr, gc, torn, collected, adopted>>

(* ---- reader ---------------------------------------------------------- *)

RdrStart ==
  /\ rdr.phase = "idle"
  /\ live.chunks # {}
  /\ rdr' = [phase |-> "reading", target |-> live.chunks, got |-> {}]
  /\ UNCHANGED <<store, live, retained, pub, gc, torn, collected, adopted, npubs>>

RdrFetch ==
  /\ rdr.phase = "reading"
  /\ \E a \in rdr.target \ rdr.got :
       IF store[a] = "absent"
         THEN /\ torn' = TRUE
              /\ UNCHANGED rdr
         ELSE /\ rdr' = [rdr EXCEPT !.got = rdr.got \union {a}]
              /\ UNCHANGED torn
  /\ UNCHANGED <<store, live, retained, pub, gc, collected, adopted, npubs>>

RdrDone ==
  /\ rdr.phase = "reading"
  /\ rdr.got = rdr.target
  /\ rdr' = [phase |-> "idle", target |-> {}, got |-> {}]
  /\ UNCHANGED <<store, live, retained, pub, gc, torn, collected, adopted, npubs>>

(* ---- the reaper ------------------------------------------------------ *)
(* Two snapshots taken at two instants, and §8.1 claims the ORDER decides *)
(* whether it is safe. `ListBeforeRefs = FALSE` is that claim's mutation. *)

GcSnap1 ==
  /\ gc.phase = "idle"
  /\ IF ListBeforeRefs
       THEN gc' = [phase |-> "one", cand |-> {a \in Addrs : store[a] # "absent"},
                   refs |-> {}]
       ELSE gc' = [phase |-> "one", cand |-> {}, refs |-> AllRefs]
  /\ UNCHANGED <<store, live, retained, pub, rdr, torn, collected, adopted, npubs>>

GcSnap2 ==
  /\ gc.phase = "one"
  /\ IF ListBeforeRefs
       THEN gc' = [gc EXCEPT !.phase = "two", !.refs = AllRefs]
       ELSE gc' = [gc EXCEPT !.phase = "two",
                             !.cand = {a \in Addrs : store[a] # "absent"}]
  /\ UNCHANGED <<store, live, retained, pub, rdr, torn, collected, adopted, npubs>>

EffectiveRefs == IF RefsAtDelete THEN AllRefs ELSE gc.refs

Doomed ==
  {a \in gc.cand \ EffectiveRefs : (~AgeGrace) \/ (store[a] = "aged")}

GcDelete ==
  /\ gc.phase = "two"
  /\ store' = [a \in Addrs |-> IF a \in Doomed THEN "absent" ELSE store[a]]
  /\ collected' = collected + Cardinality(Doomed)
  /\ gc' = [phase |-> "idle", cand |-> {}, refs |-> {}]
  /\ UNCHANGED <<live, retained, pub, rdr, torn, adopted, npubs>>

(* Time passing: a chunk written a while ago stops being protected by the *)
(* grace. This is the age SENSOR, and the adoption arm is what makes it   *)
(* capable of lying.                                                      *)
Age ==
  /\ \E a \in Addrs :
       /\ store[a] = "fresh"
       /\ ~(GraceCoversPublish /\ pub.phase = "writing" /\ a \in pub.done)
       /\ store' = [store EXCEPT ![a] = "aged"]
  /\ UNCHANGED <<live, retained, pub, rdr, gc, torn, collected, adopted, npubs>>

Next ==
  \/ PubStart \/ PubWrite \/ PubCas \/ PubCrash
  \/ RdrStart \/ RdrFetch \/ RdrDone
  \/ GcSnap1 \/ GcSnap2 \/ GcDelete
  \/ Age

Spec == Init /\ [][Next]_vars

(* ---- invariants ------------------------------------------------------ *)

(* The manifest a reader would resolve RIGHT NOW is complete. This is the *)
(* one the design actually claims, and the one a hole in the chunk list   *)
(* breaks — silently, as entries that vanished from a manifest nobody     *)
(* edited.                                                                *)
Inv_LiveComplete == \A a \in live.chunks : store[a] # "absent"

(* Every retained generation is still readable — that is what a retention *)
(* window MEANS. A window whose contents can be collected is not one.     *)
Inv_RetainedComplete ==
  \A p \in retained : \A a \in p.chunks : store[a] # "absent"

(* A reader never finds a chunk its pointer named, absent. NOT asserted by *)
(* the strict run: a reader whose pointer has fallen out of the retention  *)
(* window is a known gap (§8.1 does not bound reader lifetime), and        *)
(* LeanChunkGCSlowReader.cfg exists to say exactly when it bites rather    *)
(* than leaving it as prose.                                              *)
Inv_NoTornRead == torn = FALSE

(* ---- probes: each must be VIOLATED, or the run proved nothing -------- *)
Probe_Collected == collected = 0
Probe_Adopted == adopted = 0
================================================================================
