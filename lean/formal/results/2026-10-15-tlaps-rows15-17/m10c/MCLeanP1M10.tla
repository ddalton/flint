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
\* M11 2026-10-16, for the proof's inductive step (TLC-check first).
\* (M') MidBar's converse: in a carrying barrier a declared upload still holds
\* its scanned bytes and a declared delete is still absent (an edit or a
\* delete under a record moves the path; an upload's copy moves snap and
\* local together).  Finish's DTree needs it (baseline := snap).
MidSnap == \A s \in Writers, p \in Paths :
             w[s].pc \in {"scanned", "claimed", "cased"} /\ Carries(s) /\ Decl(s, p) =>
               /\ p \in w[s].uploads => w[s].snap[p] = w[s].local[p]
               /\ p \in w[s].deletes => w[s].local[p] = Nil
\* (Q1) a conflict's handle is live or was never a lost upload: then
\* SurfacedDecl never reads live, and Reap/Collect/Sweep cannot touch it.
LostAll == UNION {w[t].lost : t \in Writers}
ConfSafe == \A c \in conflicts : c[2] \in live \/ c[2] \notin LostAll
\* NM10C1 2026-10-16 13:53Z: ConfSafe FAILS at depth 18, by design -- a withheld
\* upload swept before the install becomes a dead, lost conflict
\* (SurfacedDecl's second arm is false for it; AckWithheldHolds).  What the
\* proof needs instead: Reap (the only unguarded live-shrinker under RetireAge)
\* never takes a lost handle -- a lost handle was never cited.
LostUncited == \A h \in LostAll : ~Cited(h) /\ h \notin retiring /\ h \notin aged
\* Simpler, if it holds: Verify's gone are the uploads already swept, and live
\* grows only by fresh handles (a re-upload is a copy), so a lost handle
\* stays dead -- SurfacedDecl's live arm then implies its other arm.
GoneDead == \A s \in Writers, p \in Paths : p \in w[s].gone => w[s].snap[p] \notin live
LostDead == \A h \in LostAll : h \notin live
\* Why live never regains a lost handle: it was uploaded (a later Upload of it
\* is a copy), and minted (a GPut or a copy mints a fresh one).
UpDoneUpped == \A s \in Writers, p \in Paths : p \in w[s].upDone => w[s].snap[p] \in upped
LostUpped == LostAll \subseteq upped /\ LostAll \subseteq minted
\* Install's barrier facts: from the claim every upload is done or withheld,
\* a barrier's delete has a baseline (it was dirty at the scan), and before
\* the install no conflict names a withheld snap (marking it lost cannot
\* break another writer's SurfacedDecl).
UpsDone == \A s \in Writers : w[s].pc \in {"claimed", "cased"} => w[s].uploads \subseteq w[s].upDone \cup w[s].gone
DelBase == \A s \in Writers, p \in Paths :
             w[s].pc \in {"scanned", "claimed", "cased"} /\ p \in w[s].deletes => w[s].baseline[p] # Nil
GoneUnconf == \A s \in Writers, p \in Paths :
                w[s].pc = "claimed" /\ p \in w[s].gone => \A c \in conflicts : c[2] # w[s].snap[p]
\* M11 rescope fork 2026-10-16: DTree breaks mid-rescope (a fetched-over
\* declared file a narrow drops: RescopeFirst uncites it, dirty, not the
\* declared bytes) and CPub3 breaks at RescopeSecond (a fetch-arm path
\* unlinked).  Neither is an AK1 break.  Restated: DTree spares the narrow's
\* leftover; CPub's fetch arm is CFetch's "not Broken against doc"; Cased's
\* install arm is "not Broken against inst".
DTree4 == \A s \in Writers, p \in Paths :
            Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ ~RescopeLeftover(s, p) =>
              Content(w[s].local[p]) = Content(w[s].decl[p])
CPub4 == \A s \in Writers, p \in Paths :
           Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p] =>
             \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
             \/ ~Broken(s, p, doc)
\* If a declared FILE never needs the fetch arm (Touch declares only bytes the
\* document does not hold, so a converge onto them needs a peer to publish the
\* same bytes), the Q2 machinery is unnecessary: CPub is the carrier arm alone.
CPubCar == \A s \in Writers, p \in Paths :
             Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p] =>
               w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
\* CPubCar's step at a converge: the document holds a declared file's bytes only
\* once the carrier published them (no peer holds this tree's unpublished
\* bytes: handles are minted per tree, a copy keeps its original's content).
DocCar == \A s \in Writers, p \in Paths :
            Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[p] # Nil /\ Content(doc[p]) = Content(w[s].decl[p]) =>
              w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
\* With CPubCar and DocCar, Cased needs no install arm: at the install a declared
\* file is clean (carrier form, kept by InstallCar) or an upload (InstallCar
\* publishes its snap, or nothing changed and the document holds it: DocCar).
Cased5 == \A s \in Writers, p \in Paths :
            w[s].pc = "cased" /\ Carries(s) /\ Decl(s, p) /\ w[s].decl[p] # Nil =>
              \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
              \/ p \in w[s].gone
\* M12's helpers (gen-m12.py), which make the side facts inductive.
SGoneUpped == \A s \in Writers, p \in Paths : p \in w[s].gone => w[s].snap[p] \in upped
SGoneBar == \A s \in Writers : w[s].pc \notin {"claimed", "cased"} => w[s].gone = {}
SGoneSub == \A s \in Writers : w[s].pc = "claimed" => w[s].gone \subseteq w[s].upDone
SConfUpped == \A c \in conflicts : c[2] \in upped \cup {Nil}
STombUpped == \A q \in Paths : tomb[q] \in upped \cup {Nil}
SBaseUpped == \A t \in Writers, q \in Paths : w[t].baseline[q] \in upped \cup {Nil}
SUp(s, p) == w[s].pc \in {"scanned", "claimed"} /\ p \in w[s].upDone
SUpUnconf == \A s \in Writers, p \in Paths : SUp(s, p) => \A c \in conflicts : c[2] # w[s].snap[p]
SUpNotTomb == \A s \in Writers, p \in Paths : SUp(s, p) => \A q \in Paths : tomb[q] # w[s].snap[p]
SUpNotBase == \A s \in Writers, p \in Paths : SUp(s, p) =>
                \A t \in Writers, q \in Paths : w[t].baseline[q] # w[s].snap[p]
Cased4 == \A s \in Writers, p \in Paths :
            w[s].pc = "cased" /\ Carries(s) /\ Decl(s, p) /\ w[s].decl[p] # Nil =>
              \/ w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
              \/ ~Broken(s, p, w[s].inst)
              \/ p \in w[s].gone
\* (Q2) a clean declared file the carrier does not publish is not broken
\* against the current document (Skip and PullOnly bind doc).
CFetch == \A s \in Writers, p \in Paths :
            Decl(s, p) /\ w[s].decl[p] # Nil /\ w[s].local[p] = w[s].baseline[p]
            /\ ~(w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)) =>
              ~Broken(s, p, doc)
\* Gateway DocCar (2026-10-16).  GCas lands gw[p]: a save in flight is the
\* content of no declared handle (a UI mint; Flight keeps it out of every tree).
GwDecl == \A p \in Paths : gw[p] # Nil =>
            \A s \in Writers : Content(w[s].decl[p]) # Content(gw[p])
\* GRename moves doc[p] to q: DocCar at EVERY document path is what a rename
\* keeps (its new document handles were document handles).
DocCarAny == \A s \in Writers, p, k \in Paths :
               Decl(s, p) /\ w[s].decl[p] # Nil /\ doc[k] # Nil
               /\ Content(doc[k]) = Content(w[s].decl[p]) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
\* RescopeSecond's unlink (2026-10-16): a narrow's leftover is a dirty path in
\* "mid", but its carrier form (from CPub3 when the uncite found it clean) must
\* survive to the unlink, which leaves it clean at Nil.  CPub3 with the leftover arm:
CPubLeft == \A s \in Writers, p \in Paths :
              Decl(s, p) /\ w[s].decl[p] # Nil
              /\ (w[s].local[p] = w[s].baseline[p] \/ (w[s].sStage = "mid" /\ p \in w[s].sDrop \ w[s].sKeep)) =>
                w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
\* CPubLeft is FALSE (RUL d6): an edit mid-rescope makes a dropped path the agent's
\* new file (bytes not sHeld), Touch declares it, no carrier yet.  The unlink takes
\* only a leftover whose bytes are still the held ones (RescopeLeftover = the unlink's
\* guard), so that is the arm:
CPubLeft2 == \A s \in Writers, p \in Paths :
               Decl(s, p) /\ w[s].decl[p] # Nil
               /\ (w[s].local[p] = w[s].baseline[p] \/ RescopeLeftover(s, p)) =>
                 w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
\* The one fact behind GwDecl, DocCarAny and Install's other trees (2026-10-16):
\* a declared FILE not yet in carrier form keeps its bytes private to its tree --
\* no document path, no save in flight and no other tree's upload holds them.
DeclPriv == \A u \in Writers, q \in Paths :
              Decl(u, q) /\ w[u].decl[q] # Nil /\ ~(w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)) =>
                /\ \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q])
                /\ \A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q])
                /\ \A s \in Writers \ {u}, k \in Paths : k \in w[s].uploads => Content(w[s].snap[k]) # Content(w[u].decl[q])
\* A restart sends a rescope at "mid" back to "saved" (sKeep := {}): DTree4's leftover
\* exemption (RescopeLeftover, "mid" only) is lost.  The exemption over the whole
\* rescope: a dropped path the first half uncited, while the rescope is in flight.
DTree5 == \A s \in Writers, p \in Paths :
            Decl(s, p) /\ w[s].local[p] # w[s].baseline[p]
            /\ ~(w[s].sStage \in {"saved", "mid"} /\ p \in w[s].sDrop /\ w[s].baseline[p] = Nil) =>
              Content(w[s].local[p]) = Content(w[s].decl[p])
\* M11's leftover set with the restart arm (2026-10-16, the Touch restart fix):
\* after a restart mid-narrow the stage is "saved" and sKeep is gone; a
\* leftover is then a dropped path uncited with a held entry.
LeftL(s) == IF w[s].sStage = "mid" THEN w[s].sDrop \ w[s].sKeep
            ELSE IF w[s].sStage = "saved" THEN {p \in w[s].sDrop : w[s].baseline[p] = Nil /\ w[s].sHeld[p] # Nil}
            ELSE {}
DTreeL == \A s \in Writers, p \in Paths :
            Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin LeftL(s) =>
              Content(w[s].local[p]) = Content(w[s].decl[p])
\* The same by handle (constant-level: the proof's A1LeftSet reads the tree
\* record only): the file still holds the very handle the uncite recorded.
LeftH(s) == {p \in w[s].sDrop : /\ w[s].local[p] # Nil /\ w[s].sHeld[p] # Nil /\ w[s].local[p] = w[s].sHeld[p]
                                /\ \/ w[s].sStage = "mid" /\ p \notin w[s].sKeep
                                   \/ w[s].sStage = "saved" /\ w[s].baseline[p] = Nil}
DTreeH == \A s \in Writers, p \in Paths :
            Decl(s, p) /\ w[s].local[p] # w[s].baseline[p] /\ p \notin LeftH(s) =>
              Content(w[s].local[p]) = Content(w[s].decl[p])
\* M11's restated leftover facts (gen-m11.py A1CPubL, A1KeepCited, A1HeldSame).
CPubLeftH == \A s \in Writers, p \in Paths :
               Decl(s, p) /\ w[s].decl[p] # Nil /\ p \in LeftH(s) => w[s].car.id = w[s].pend.id /\ CarForm(s, p, w[s].car)
KeepCited == \A s \in Writers, p \in Paths : w[s].sStage = "mid" /\ p \in w[s].sKeep => w[s].baseline[p] # Nil
LeftCand(s, p) == /\ p \in w[s].sDrop /\ w[s].local[p] # Nil /\ w[s].sHeld[p] # Nil
                  /\ \/ w[s].sStage = "mid" /\ p \notin w[s].sKeep
                     \/ w[s].sStage = "saved" /\ w[s].baseline[p] = Nil
HeldSame == \A s \in Writers, p \in Paths :
              LeftCand(s, p) /\ Content(w[s].local[p]) = Content(w[s].sHeld[p]) => w[s].local[p] = w[s].sHeld[p]
\* DeclPriv's helpers (M13 plan, 2026-10-16).  A save in flight is a UI mint
\* held nowhere else (no tree, no document path, no other save).
GwAlone == \A k \in Paths : gw[k] # Nil =>
             /\ \A t \in Writers, j \in Paths :
                  /\ Content(w[t].local[j]) # Content(gw[k]) /\ Content(w[t].snap[j]) # Content(gw[k])
                  /\ Content(w[t].decl[j]) # Content(gw[k]) /\ Content(w[t].baseline[j]) # Content(gw[k])
             /\ \A j \in Paths : Content(doc[j]) # Content(gw[k])
             /\ \A j \in Paths \ {k} : Content(gw[j]) # Content(gw[k])
\* Another tree's dirty file (not the narrow's leftover) or upload never holds a
\* private declaration's bytes.
PrivDirty == \A u \in Writers, q \in Paths :
               Decl(u, q) /\ w[u].decl[q] # Nil /\ ~(w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)) =>
                 \A s \in Writers \ {u}, k \in Paths :
                   /\ w[s].local[k] # w[s].baseline[k] /\ k \notin LeftH(s) => Content(w[s].local[k]) # Content(w[u].decl[q])
                   /\ k \in w[s].uploads => Content(w[s].snap[k]) # Content(w[u].decl[q])
\* The same within the tree: no other path of u holds them dirty or as an upload.
PrivSelf == \A u \in Writers, q \in Paths :
              Decl(u, q) /\ w[u].decl[q] # Nil /\ ~(w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)) =>
                \A k \in Paths \ {q} :
                  /\ k \in w[u].uploads => Content(w[u].snap[k]) # Content(w[u].decl[q])
                  /\ w[u].local[k] # w[u].baseline[k] => Content(w[u].local[k]) # Content(w[u].decl[q])
\* DeclPriv's peer arm from one cross-tree fact: two trees never both hold, as an
\* unpublished dirty file or an upload, bytes the document holds nowhere.
Dirt(t, j) == w[t].local[j] # w[t].baseline[j] /\ w[t].local[j] # Nil /\ j \notin LeftH(t)
OffDoc(x) == \A j \in Paths : doc[j] = Nil \/ Content(doc[j]) # x
DirtyApart == \A u, s \in Writers, q, k \in Paths :
                u # s /\ Dirt(u, q) /\ OffDoc(Content(w[u].local[q])) =>
                  /\ Dirt(s, k) => Content(w[s].local[k]) # Content(w[u].local[q])
                  /\ k \in w[s].uploads => Content(w[s].snap[k]) # Content(w[u].local[q])
\* A save in flight is held by no tree's file (Touch's gw arm).
GwLocal == \A k \in Paths : gw[k] # Nil => \A t \in Writers, j \in Paths : Content(w[t].local[j]) # Content(gw[k])
\* DirtyApart without the document guard (the inductive form, M14): a dirty
\* file's bytes are its tree's own write; no other tree holds them dirty or as an upload.
DirtyUnique == \A u, s \in Writers, q, k \in Paths :
                 u # s /\ Dirt(u, q) =>
                   /\ Dirt(s, k) => Content(w[s].local[k]) # Content(w[u].local[q])
                   /\ k \in w[s].uploads => Content(w[s].snap[k]) # Content(w[u].local[q])
==============================================================================
