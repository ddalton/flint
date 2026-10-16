----------------------------- MODULE MCLeanP1M10 -----------------------------
(* AK1 part 2's candidate conjuncts (M10), checked before the proof is
   written.  With MCLeanP1M7's BoundOK, DTree, CPub (copied here).         *)
EXTENDS LeanP1
Honorable(s) == w[s].pend.id # 0 /\ w[s].bound.id = w[s].pend.id /\ w[s].bound.fold = w[s].pend.fold
HDropped(s) == w[s].bound.wd \cup (IF w[s].car.id = w[s].pend.id THEN w[s].car.dropped \cup w[s].car.uncited ELSE {})
BoundOK == \A s \in Writers : Honorable(s) =>
             \A p \in Paths : Broken(s, p, w[s].bound.doc) => p \in HDropped(s)
Decl(s, p) == w[s].pend.id # 0 /\ p \in w[s].dset /\ p \notin w[s].moved
CarForm(s, p, c) ==
  /\ p \in c.paths
  /\ \/ p \in c.dropped
     \/ w[s].decl[p] = Nil /\ p \in c.deletes
     \/ /\ w[s].decl[p] # Nil /\ p \notin c.deletes
        /\ c.pub[p] # Nil /\ Content(c.pub[p]) = Content(w[s].decl[p])
DTree == \A s \in Writers, p \in Paths :
           Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] => Content(w[s].local[p]) = Content(w[s].decl[p])
CPub == \A s \in Writers, p \in Paths :
          Decl(s, p) /\ w[s].local[p] = w[s].baseline[p] => w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
\* M10A/B 23:10Z: CPub fails at depth 5 -- a declared delete, the UI deletes
\* the path too, the consume fetches: clean, nothing published, nothing broken.
\* A clean declared path is in carrier form, or its baseline carries the
\* declaration (a fetch made the tree agree).
CPub2 == \A s \in Writers, p \in Paths :
          Decl(s, p) /\ w[s].local[p] = w[s].baseline[p] =>
            \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            \/ Content(w[s].baseline[p]) = Content(w[s].decl[p])
\* M10B 23:14Z: CPub2 fails at depth 8 -- a declared delete made clean by a
\* fetch, then the UI renames another file onto it, fetched.  Not broken: a
\* declared delete never is (an absent path carries it, a present one is a
\* LaterVersion).  So only declared FILES need the carrier.
CPub3 == \A s \in Writers, p \in Paths :
          Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p] =>
            \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
            \/ Content(w[s].baseline[p]) = Content(w[s].decl[p])
\* (F) folds: a touch makes the honour wait for the next barrier end.
Folds == \A s \in Writers : w[s].pend.fold <= w[s].nt /\ w[s].bound.fold <= w[s].nt /\ w[s].cFold <= w[s].nt
\* (Z) in a barrier the record is the one it consumed (touch, retire: idle only).
Frozen == \A s \in Writers : w[s].pc \in {"consumed", "scanned", "claimed", "cased"} =>
            w[s].cId = w[s].pend.id /\ w[s].cFold = w[s].pend.fold
\* (M) after a carrying scan, a dirty declared path is published by it.
MidBar == \A s \in Writers, p \in Paths :
            w[s].pc \in {"scanned", "claimed", "cased"} /\ Carries(s) /\ Decl(s, p)
            /\ w[s].local[p] # w[s].baseline[p] =>
              \/ p \in w[s].uploads /\ w[s].snap[p] = w[s].local[p]
              \/ p \in w[s].deletes /\ w[s].local[p] = Nil
\* (I) after a carrying install, every declared path is in carrier form, or
\* the install changed nothing and the result carries the declaration.
Cased == \A s \in Writers, p \in Paths :
           \* (declared files only: M10B 23:20Z, a declared delete is never broken)
           w[s].pc = "cased" /\ Carries(s) /\ Decl(s, p) /\ w[s].decl[p] # Nil =>
             \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
             \/ Content(w[s].inst[p]) = Content(w[s].decl[p])
             \* withheld: the barrier end names it (wd = gone; M10C1/C2, two writers)
             \/ p \in w[s].gone
\* (B') the bound's withheld paths are this barrier's gone (Finish), the bound's
\* document a function on the paths.
BoundDoc == \A s \in Writers : w[s].bound.doc \in [Paths -> Opt(Handles)]
==============================================================================
