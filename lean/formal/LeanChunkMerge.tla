----------------------------- MODULE LeanChunkMerge -----------------------------
(***************************************************************************)
(* The chunked three-way merge (chunked-manifest design §6).               *)
(*                                                                         *)
(* The entry-level merge is already machine-checked in LeanSubtree and is  *)
(* NOT re-derived here. The question chunking adds is one level up: a      *)
(* writer that 412s must merge, and the tempting optimisation is to reuse  *)
(* the OTHER writer's chunk list and substitute only the chunks its own    *)
(* change touched — which would make a merge cost O(changed) too.          *)
(*                                                                         *)
(* The property checked is a REFINEMENT: whatever the chunked path         *)
(* publishes must contain exactly the key set the whole-document merge     *)
(* would have produced. Anything else loses or duplicates entries, and a   *)
(* lost entry reads to every consumer as a file the agent deleted.         *)
(*                                                                         *)
(* The load-bearing detail, and the reason this model exists at all: with  *)
(* boundaries determined by the key ALONE, a key's chunk is a function of  *)
(* the key, splicing is trivially safe, and there is nothing to check.     *)
(* MinRun breaks that — a cut is suppressed until enough keys have         *)
(* accumulated, so chunk membership depends on the SET, and two writers    *)
(* with different key sets can disagree about where a chunk ends. §3 calls *)
(* min/max "where the pure-function property leaks"; this module is what   *)
(* that leak costs.                                                        *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS
  MaxKey,        \* keys are 1..MaxKey, in sorted order by construction
  BoundaryKeys,  \* keys the content hash marks as a cut point
  MinRun,        \* the floor: a cut is suppressed below this run length
  SpliceLists    \* TRUE = the mutation: reuse theirs, substitute mine

Keys == 1..MaxKey

VARIABLES base, addA, delA, addB, delB

vars == <<base, addA, delA, addB, delB>>

(* Chunk membership, as a fold over the sorted key stream. Returns a      *)
(* function from key to chunk index; keys sharing an index are one chunk. *)
RECURSIVE Fold(_, _, _, _)
Fold(S, k, idx, run) ==
  IF k > MaxKey
    THEN [x \in {} |-> 0]
    ELSE IF k \notin S
           THEN Fold(S, k + 1, idx, run)
           ELSE LET run2 == run + 1
                    cut  == (k \in BoundaryKeys) /\ (run2 >= MinRun)
                IN  [x \in {k} |-> idx]
                    @@ Fold(S, k + 1, IF cut THEN idx + 1 ELSE idx,
                                      IF cut THEN 0 ELSE run2)

ChunkIdx(S) == Fold(S, 1, 0, 0)

Chunks(S) ==
  LET f == ChunkIdx(S)
  IN  { { k \in S : f[k] = i } : i \in { f[k] : k \in S } }

(* The two writers' key sets. *)
SetA == (base \ delA) \union addA
SetB == (base \ delB) \union addB

(* The entry-level merge, as LeanSubtree already checks it: start from    *)
(* THEIRS so foreign entries survive by construction, apply MY upserts,   *)
(* and apply my deletes only where theirs is unchanged since my base.     *)
DelAEffective == delA \ (addB \union delB)
Merged == (SetB \ DelAEffective) \union addA

(* What the chunked path publishes. *)
ChangedA == addA \union delA

Spliced ==
  { c \in Chunks(SetB) : c \intersect ChangedA = {} }
  \union
  { c \in Chunks(SetA) : c \intersect ChangedA # {} }

Published ==
  IF SpliceLists THEN UNION Spliced ELSE Merged

Init ==
  /\ base \in SUBSET Keys
  /\ addA \in SUBSET (Keys \ base)
  /\ delA \in SUBSET base
  /\ addB \in SUBSET (Keys \ base)
  /\ delB \in SUBSET base

Next == UNCHANGED vars

Spec == Init /\ [][Next]_vars

(* ---- the refinement ---------------------------------------------------*)

(* Whatever the chunked path publishes must be exactly what the whole-    *)
(* document merge would have. Losing a foreign entry is the failure this  *)
(* exists to catch; gaining a duplicate is the other side of the same     *)
(* seam bug.                                                              *)
Inv_ChunkedMergeMatches == Published = Merged

(* ---- probes: must be VIOLATED ---------------------------------------- *)

(* The two writers' chunk BOUNDARIES actually diverge somewhere. Without  *)
(* this the strict run is green over inputs where splicing could not have *)
(* gone wrong, and proves nothing about the strategy.                     *)
Probe_BoundariesDiverged == Chunks(SetA) = Chunks(SetB)

(* Both writers actually changed something. *)
Probe_BothWrote == ChangedA = {} \/ (addB \union delB) = {}
================================================================================
