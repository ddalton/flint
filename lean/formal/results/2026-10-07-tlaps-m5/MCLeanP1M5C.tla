----------------------------- MODULE MCLeanP1M5C ------------------------------
(***************************************************************************)
(* M5 part C: the strengthening NR5's proof is planned on, as STATE        *)
(* invariants for TLC (E36), over LeanP1 (the proof's `anc` ghost is       *)
(* AncR here, the recursion over `base`).  NR5 follows: an owed take is    *)
(* at a clean baseline (PC1: the document derives from it, or a record     *)
(* names it; rank: a version deriving from the baseline is never strictly  *)
(* older); a convergence take holds the document's bytes (LDB); a finish   *)
(* takes its own snapshot (SDB); a widen fetches only where it holds       *)
(* nothing (DT).                                                           *)
(***************************************************************************)
EXTENDS MCLeanP1M5R
Bar == {"scanned", "claimed", "cased"}
RECURSIVE AncR(_)
AncR(h) == IF h = Nil \/ base[h] = Nil THEN {} ELSE {base[h]} \cup AncR(base[h])
CR(h) == Gen(Content(h))
\* The history's order: an ancestor's bytes are strictly older (minted earlier).
Rk  == \A m \in minted : \A a \in AncR(m) : CR(a) < CR(m)
OG  == \A m \in minted : orig[m] = Nil => Gen(m) < nextGen
\* A copy shares its original's history.
SCA == \A m, n \in minted : Content(m) = Content(n) => AncR(m) = AncR(n)
\* Uploads exist only in a barrier.
UB  == \A s \in Writers : w[s].pc \notin Bar => w[s].uploads = {}
\* The tree's bytes derive from its baseline; in a barrier, from the snapshot;
\* the snapshot from the baseline.
LDB == \A s \in Writers, p \in Paths :
         (w[s].local[p] # Nil /\ w[s].baseline[p] # Nil) => Derives(w[s].local[p], w[s].baseline[p])
LDS == \A s \in Writers : w[s].pc \in Bar => \A p \in w[s].uploads :
         w[s].local[p] # Nil => Derives(w[s].local[p], w[s].snap[p])
SDB == \A s \in Writers : w[s].pc \in Bar => \A p \in w[s].uploads :
         w[s].baseline[p] # Nil => Derives(w[s].snap[p], w[s].baseline[p])
\* The document at p derives from x, or a record names x at p.
DorN(p, x) == (doc[p] # Nil /\ Derives(doc[p], x)) \/ NamedAt(p, x)
PC1 == \A s \in Writers, p \in Paths :
         (w[s].baseline[p] # Nil /\ w[s].local[p] = w[s].baseline[p]) => DorN(p, w[s].baseline[p])
PC2 == \A s \in Writers : w[s].pc = "cased" => \A p \in w[s].uploads \cap w[s].upDone : DorN(p, w[s].snap[p])
DT  == \A s \in Writers : w[s].sDrop \cap w[s].sTgt = {}
J5C == Rk /\ OG /\ SCA /\ UB /\ LDB /\ LDS /\ SDB /\ PC1 /\ PC2 /\ DT
==============================================================================
