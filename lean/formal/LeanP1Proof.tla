---------------------------- MODULE LeanP1Proof ------------------------------
(* The TLAPS proof module for the lean protocol (docs/plans/
   lean-tlaps-inductive-proof-plan.md).  It EXTENDS LeanP1Anc.tla, the copy
   of LeanP1.tla that tlapm can read (every writer step's new tree an
   explicit record `<Step>W(..)`; tied to the original by exact distinct
   count: results/2026-10-06-tlaps-typeok/RESULTS.txt), and proves its
   theorems under the shipped shape: every rule constant TRUE (`Shipped`).

   M0 (this file's first theorem): `Spec => []TypeOK`.  TypeOK is not
   inductive on its own; `IndTypeOK` is, and it adds:
     - the three upload ghosts TypeOK leaves untyped (`upped`, `copies`,
       `orig`; `copies` is the generation of a copy handle),
     - `mv = Nil` (the plan's I0: with RenameAtomic the second CAS never
       runs, and ITS `CHOOSE q \in Paths : ...` is what would break
       `acked \subseteq Paths \X Handles`) -- the one conjunct of `Shipped`
       this theorem uses,
     - `nextGen > Seed`, so a minted handle is in `Handles`,
     - `Minted`: the document, a save in flight and every tree name only
       minted handles, and an upload's snapshot is a minted handle -- what
       `live \subseteq minted` needs at `Upload`.
   Every step lemma has the same shape: TypeOK, Ghosts and Minted taken
   from IndTypeOK as three hidden facts; the step's primed variables from
   its definition; the step's record in `Writer` from the types of what it
   reads (Z3, ~10 s); the frame conjuncts from `FrameTypeOK`; `Minted` from
   the record's `local`, `uploads` and `snap`.  A step cites only the facts
   it needs: an obligation that carries the whole invariant expanded puts
   Z3 and tlapm's encoder past their limits on goals they close in seconds
   otherwise (results/2026-10-06-tlaps-typeok/NOTES.txt).  What TLC reads
   from the cfg and a proof must be told is stated here as named ASSUMEs
   (tlapm does not use an unnamed one: micro/Asm.tla), including the six
   LeanP1Anc.tla states without names.
   M1 (after the TypeOK theorem): `Inv_OneHolder`, `Prop_DeleteSettles` and
   `Prop_NarrowNeverDeletes` over `IndM1` -- IndTypeOK and the plan's I1, I6,
   I7 with six facts read off the actions (TLC-checked first:
   results/2026-10-07-tlaps-m1/).  One generic lemma per conjunct takes the
   step's new tree as a parameter; a step lemma reads the tree's fields
   once (`<1>3`) and discharges each lemma's hypothesis from them.        *)
EXTENDS LeanP1Anc, TLAPS, FiniteSetTheorems

------------------------------------------------------------------------------
(* What a proof must be told.                                               *)

\* The shipped shape: every rule constant TRUE (plan section 2).
Shipped ==
  /\ CommitSurfacesForeign /\ CommitVerifiesUploads /\ SweepUnderLease
  /\ CollectorSparesCited /\ CommitRecordsDeleteOverride /\ DeleteWinsPreserved
  /\ ContentConverges /\ RecheckSkipped /\ CommitAdvanceGuarded /\ RetireAge
  /\ GatewayIgnoresLease /\ GatewayJudgesRead /\ GatewaySweepGrace /\ RenameAtomic
  /\ ConsumeHonorsScope /\ RescopeUnciteFirst /\ RescopeKeepsDirty /\ WidenKeepsLocal
  /\ UnlinkChecksBytes /\ ConsumeKeepsLeft /\ SyncKeepsLeft /\ ReaderRechecksOwed
ASSUME ShippedShape == Shipped

\* LeanP1Anc.tla's own assumptions, named so a proof can cite them.
ASSUME FreePaths     == Free \subseteq Paths
ASSUME MaxMintNat    == MaxMint \in Nat /\ MaxMint >= 1
ASSUME ScopesPaths   == Scopes \subseteq SUBSET Paths /\ Scopes # {}
ASSUME ReadersWriters == Readers \subseteq Writers
ASSUME NilHandle     == Nil \notin Handles
ASSUME NoneWriter    == "none" \notin Writers
\* ...and what the cfg's finite constants give TLC for free.
ASSUME MaxCopiesNat  == MaxCopies \in Nat
ASSUME PathsFinite   == IsFiniteSet(Paths)

------------------------------------------------------------------------------
(* The inductive invariant for TypeOK.                                      *)

\* The upload ghosts TypeOK leaves untyped.
Ghosts == upped \subseteq Handles /\ copies \in Nat /\ orig \in [Handles -> Opt(Handles)]
\* Every handle the document, a save in flight or a tree names is minted;
\* an upload's snapshot is a minted handle (the upload re-cites it in `live`).
Minted ==
  /\ \A p \in Paths : doc[p] \in Opt(minted)
  /\ \A p \in Paths : gw[p] \in Opt(minted)
  /\ \A s \in Writers, p \in Paths : w[s].local[p] \in Opt(minted)
  /\ \A s \in Writers : \A p \in w[s].uploads : w[s].snap[p] \in minted
IndTypeOK == TypeOK /\ Ghosts /\ mv = Nil /\ nextGen > Seed /\ Minted

\* What every gateway or writer step conjoins (`Next`'s first disjunct).
Frame == TookUpdate /\ RetUpdate /\ AncUpdate /\ UNCHANGED <<aged, ages, rdoc, rlag>>
\* What the frame conjuncts type.
FrameOK ==
  /\ took' \in [Writers -> [Paths -> SUBSET Gens]]
  /\ retiring' \subseteq Handles
  /\ anc' \in [Handles -> SUBSET Handles]
  /\ aged' \subseteq Handles /\ ages' \in Nat
  /\ rdoc' \in [Paths -> Opt(Handles)] /\ rlag' \in BOOLEAN

------------------------------------------------------------------------------
(* Helpers.                                                                 *)

LEMMA WriterFields ==
  ASSUME NEW W \in Writer
  PROVE  /\ W.st \in {"off", "on"}
         /\ W.pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
         /\ W.local \in [Paths -> Opt(Handles)]
         /\ W.baseline \in [Paths -> Opt(Handles)]
         /\ W.integrated \subseteq Gens
         /\ W.uploads \subseteq Paths /\ W.deletes \subseteq Paths
         /\ W.snap \in [Paths -> Opt(Handles)]
         /\ W.upDone \subseteq Paths /\ W.gone \subseteq Paths /\ W.verified \in BOOLEAN
         /\ W.inst \in [Paths -> Opt(Handles)]
         /\ W.retire \subseteq Handles
         /\ W.collected \in BOOLEAN /\ W.adv \in BOOLEAN
         /\ W.synced \in Nat /\ W.derived \in Nat
         /\ W.skipped \subseteq Paths /\ W.scope \subseteq Paths
         /\ W.sStage \in {"none", "saved", "mid"}
         /\ W.sTgt \subseteq Paths /\ W.sDrop \subseteq Paths /\ W.sKeep \subseteq Paths
         /\ W.sHeld \in [Paths -> Opt(Handles)]
         /\ W.unlinked \subseteq Paths
         /\ W.memo \in Nat /\ W.rnow \in Nat
<1>1. /\ W.st \in {"off", "on"}
      /\ W.pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
      /\ W.local \in [Paths -> Opt(Handles)]
      /\ W.baseline \in [Paths -> Opt(Handles)]
      /\ W.integrated \subseteq Gens
      /\ W.uploads \subseteq Paths /\ W.deletes \subseteq Paths
      /\ W.snap \in [Paths -> Opt(Handles)]
  BY DEF Writer
<1>2. /\ W.upDone \subseteq Paths /\ W.gone \subseteq Paths /\ W.verified \in BOOLEAN
      /\ W.inst \in [Paths -> Opt(Handles)]
      /\ W.retire \subseteq Handles
      /\ W.collected \in BOOLEAN /\ W.adv \in BOOLEAN
      /\ W.synced \in Nat /\ W.derived \in Nat
      /\ W.skipped \subseteq Paths /\ W.scope \subseteq Paths
  BY DEF Writer
<1>3. /\ W.sStage \in {"none", "saved", "mid"}
      /\ W.sTgt \subseteq Paths /\ W.sDrop \subseteq Paths /\ W.sKeep \subseteq Paths
      /\ W.sHeld \in [Paths -> Opt(Handles)]
      /\ W.unlinked \subseteq Paths
      /\ W.memo \in Nat /\ W.rnow \in Nat
  BY DEF Writer
<1>. QED BY <1>1, <1>2, <1>3

\* On its own: with a lemma's other facts in scope the same goal runs past Z3's limit.
LEMMA WriterInitType == WriterInit \in Writer
BY DEF WriterInit, Writer, Opt

LEMMA HandleGen == \A h \in Handles : Gen(h) \in Gens /\ h[1] \in Paths
BY DEF Handles, Gen

LEMMA CardPaths == ASSUME NEW S \in SUBSET Paths PROVE Cardinality(S) \in Nat
BY PathsFinite, FS_Subset, FS_CardinalityType

\* The handle a GPut or an Edit mints.
LEMMA MintHandle ==
  ASSUME TypeOK, nextGen > Seed, nextGen <= MaxMint, NEW p \in Paths
  PROVE  nextGen \in Gens /\ <<p, nextGen>> \in Handles
BY MaxMintNat, MaxCopiesNat DEF TypeOK, Gens, Seed, Handles

\* The handle an Upload's copy mints.
LEMMA CopyHandle ==
  ASSUME Ghosts, copies < MaxCopies, NEW p \in Paths
  PROVE  MaxMint + copies + 1 \in Gens /\ <<p, MaxMint + copies + 1>> \in Handles
BY MaxMintNat, MaxCopiesNat DEF Ghosts, Gens, Seed, Handles

\* A document's cited generations.
LEMMA DocGens ==
  ASSUME TypeOK, NEW S \in SUBSET Paths
  PROVE  {Gen(doc[p]) : p \in {q \in S : doc[q] # Nil}} \subseteq Gens
BY HandleGen DEF TypeOK, Opt

LEMMA FrameTypeOK ==
  ASSUME TypeOK, Frame,
         w' \in [Writers -> Writer],
         doc' \in [Paths -> Opt(Handles)],
         base' \in [Handles -> Opt(Handles)]
  PROVE  FrameOK
<1>1. took' \in [Writers -> [Paths -> SUBSET Gens]]
  <2>1. \A s \in Writers, p \in Paths :
          /\ w'[s].baseline[p] # Nil => Gen(w'[s].baseline[p]) \in Gens
          /\ w'[s].local[p] # Nil => Gen(w'[s].local[p]) \in Gens
    BY WriterFields, HandleGen DEF Opt
  <2>. QED BY <2>1 DEF TypeOK, Frame, TookUpdate
<1>2. retiring' \subseteq Handles BY DEF TypeOK, Frame, RetUpdate, Opt
<1>3. anc' \in [Handles -> SUBSET Handles] BY DEF TypeOK, Frame, AncUpdate, AncOf, Opt
<1>. QED BY <1>1, <1>2, <1>3 DEF TypeOK, Frame, FrameOK

\* The step writes one tree: what the others and the typing of `w'` become.
LEMMA WriteOne ==
  ASSUME TypeOK, NEW s \in Writers, NEW R \in Writer, w' = [w EXCEPT ![s] = R]
  PROVE  /\ w' \in [Writers -> Writer]
         /\ \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t]
BY DEF TypeOK

\* A tree's map after one path is written.
LEMMA ExceptFun ==
  ASSUME NEW f \in [Paths -> Opt(Handles)], NEW p \in Paths, NEW v \in Opt(Handles)
  PROVE  [f EXCEPT ![p] = v] \in [Paths -> Opt(Handles)]
OBVIOUS

\* Minted after a step that writes no tree.
LEMMA MintedKeep ==
  ASSUME Minted, minted \subseteq minted', w' = w,
         \A q \in Paths : doc'[q] \in Opt(minted'),
         \A q \in Paths : gw'[q] \in Opt(minted')
  PROVE  Minted'
BY DEF Minted, Opt

\* Minted after a step that writes one tree R.
LEMMA MintedWrite ==
  ASSUME Minted, NEW s \in Writers, NEW R,
         minted \subseteq minted',
         \A q \in Paths : doc'[q] \in Opt(minted'),
         \A q \in Paths : gw'[q] \in Opt(minted'),
         \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         \A q \in Paths : R.local[q] \in Opt(minted'),
         \A q \in R.uploads : R.snap[q] \in minted'
  PROVE  Minted'
BY DEF Minted, Opt

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_TypeOK == Init => IndTypeOK
<1>. SUFFICES ASSUME Init PROVE IndTypeOK OBVIOUS
<1>1. Seed \in Gens BY MaxMintNat, MaxCopiesNat DEF Seed, Gens
<1>2. \A p \in Paths : <<p, Seed>> \in Handles BY <1>1 DEF Handles
<1>3. live \subseteq Handles /\ minted = live /\ upped = live BY <1>2, FreePaths DEF Init
<1>4. doc \in [Paths -> Opt(Handles)] BY <1>2 DEF Init, Opt
<1>5. WriterInit \in Writer BY WriterInitType
<1>6. w \in [Writers -> Writer] BY <1>5 DEF Init
<1>7. /\ live \subseteq Handles /\ minted \subseteq Handles /\ live \subseteq minted
      /\ doc \in [Paths -> Opt(Handles)] /\ seq \in Nat
      /\ tomb \in [Paths -> Opt(Handles)] /\ base \in [Handles -> Opt(Handles)]
  BY <1>3, <1>4 DEF Init, Opt
<1>8. /\ acked \subseteq Paths \X Handles /\ conflicts \subseteq Paths \X Opt(Handles)
      /\ holder \in Writers \cup {"none"}
      /\ nextGen \in Nat /\ ui \in Nat /\ reqs \in Nat /\ barriers \in Nat
      /\ w \in [Writers -> Writer]
      /\ took \in [Writers -> [Paths -> SUBSET Gens]]
  BY <1>6 DEF Init, Seed
<1>9. /\ gw \in [Paths -> Opt(Handles)] /\ mv \in Opt(Paths \X Handles)
      /\ udel \subseteq Paths \X Handles
      /\ restarts \in Nat /\ syncs \in Nat /\ regressed \in BOOLEAN /\ rescopes \in Nat /\ fails \in Nat
      /\ retiring \subseteq Handles /\ aged \subseteq Handles /\ ages \in Nat
      /\ rdoc \in [Paths -> Opt(Handles)] /\ rlag \in BOOLEAN
      /\ anc \in [Handles -> SUBSET Handles]
  BY DEF Init, Opt
<1>10. TypeOK BY <1>7, <1>8, <1>9 DEF TypeOK
<1>11. Ghosts BY <1>3 DEF Init, Ghosts, Opt
<1>12. Minted BY <1>2, <1>3, <1>5, <1>6 DEF Init, Minted, WriterInit, Opt
<1>. QED BY <1>10, <1>11, <1>12 DEF Init, IndTypeOK, Seed

------------------------------------------------------------------------------
(* The gateway's steps (no tree changes).                                  *)

LEMMA GPut_TypeOK ==
  ASSUME IndTypeOK, NEW p \in Paths, GPut(p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>1. /\ live' = live \cup {<<p, nextGen>>} /\ minted' = minted \cup {<<p, nextGen>>}
      /\ base' = [base EXCEPT ![<<p, nextGen>>] = doc[p]]
      /\ gw' = [gw EXCEPT ![p] = <<p, nextGen>>]
      /\ nextGen' = nextGen + 1 /\ ui' = ui + 1
      /\ upped' = upped \cup {<<p, nextGen>>}
      /\ nextGen <= MaxMint
      /\ UNCHANGED <<doc, seq, tomb, acked, conflicts, holder, reqs, barriers, w, mv, udel,
                     restarts, syncs, regressed, rescopes, fails, copies, orig>>
  BY DEF GPut
<1>2. nextGen \in Gens /\ <<p, nextGen>> \in Handles BY <1>a, <1>b, <1>1, MintHandle
<1>3. base' \in [Handles -> Opt(Handles)] BY <1>a, <1>1, <1>2 DEF TypeOK, Opt
<1>4. w' \in [Writers -> Writer] /\ doc' \in [Paths -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>5. FrameOK BY <1>a, <1>1, <1>3, <1>4, FrameTypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5 DEF TypeOK, Opt, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>a, <1>b, <1>1, <1>2 DEF Ghosts, TypeOK, Seed
<1>g. gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] BY <1>a DEF TypeOK
<1>8. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>g, <1>1, <1>2 DEF Minted, Opt
<1>9. Minted' BY <1>c, <1>8, MintedKeep
<1>. QED BY <1>6, <1>7, <1>9 DEF IndTypeOK

LEMMA GCas_TypeOK ==
  ASSUME IndTypeOK, NEW p \in Paths, GCas(p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>1. /\ gw[p] # Nil
      /\ doc' \in {[doc EXCEPT ![p] = gw[p]], doc}
      /\ seq' \in {seq + 1, seq}
      /\ tomb' \in {[tomb EXCEPT ![p] = Nil], tomb}
      /\ acked' \in {acked \cup {<<p, gw[p]>>}, acked}
      /\ gw' = [gw EXCEPT ![p] = Nil]
      /\ UNCHANGED <<live, minted, base, conflicts, holder, nextGen, ui, reqs, barriers, w, mv, udel, aux>>
  BY DEF GCas
<1>2. gw[p] \in minted /\ gw[p] \in Handles BY <1>a, <1>c, <1>1 DEF TypeOK, Minted, Opt
<1>3. doc' \in [Paths -> Opt(Handles)] /\ \A q \in Paths : doc'[q] \in Opt(minted)
  BY <1>a, <1>c, <1>1, <1>2 DEF TypeOK, Minted, Opt
<1>4. w' \in [Writers -> Writer] /\ base' \in [Handles -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>5. FrameOK BY <1>a, <1>1, <1>3, <1>4, FrameTypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5 DEF TypeOK, Opt, aux, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts, aux
<1>g. gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] BY <1>a DEF TypeOK
<1>8. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>g, <1>1, <1>3 DEF Minted, Opt, aux
<1>9. Minted' BY <1>c, <1>8, MintedKeep
<1>. QED BY <1>6, <1>7, <1>9 DEF IndTypeOK

LEMMA GRename_TypeOK ==
  ASSUME IndTypeOK, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. RenameAtomic BY ShippedShape DEF Shipped
<1>1. /\ doc[p] # Nil
      /\ doc' = [doc EXCEPT ![q] = doc[p], ![p] = Nil]
      /\ tomb' = [tomb EXCEPT ![q] = Nil, ![p] = doc[p]]
      /\ acked' = acked \cup {<<q, doc[p]>>}
      /\ mv' = mv
      /\ seq' = seq + 1 /\ reqs' = reqs + 1
      /\ UNCHANGED <<live, minted, base, conflicts, holder, gw, udel, nextGen, ui, barriers, w, aux>>
  BY <1>0 DEF GRename
<1>2. doc[p] \in minted /\ doc[p] \in Handles BY <1>a, <1>c, <1>1 DEF TypeOK, Minted, Opt
<1>3. doc' \in [Paths -> Opt(Handles)] /\ \A r \in Paths : doc'[r] \in Opt(minted)
  BY <1>a, <1>c, <1>1, <1>2 DEF TypeOK, Minted, Opt
<1>4. w' \in [Writers -> Writer] /\ base' \in [Handles -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>5. FrameOK BY <1>a, <1>1, <1>3, <1>4, FrameTypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5 DEF TypeOK, Opt, aux, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts, aux
<1>g. gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] BY <1>a DEF TypeOK
<1>8. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>g, <1>1, <1>3 DEF Minted, Opt, aux
<1>9. Minted' BY <1>c, <1>8, MintedKeep
<1>. QED BY <1>6, <1>7, <1>9 DEF IndTypeOK

\* The two-CAS rename's second CAS: never enabled (mv = Nil).
LEMMA GRenameFinish_TypeOK ==
  ASSUME IndTypeOK, GRenameFinish, Frame
  PROVE  IndTypeOK'
BY DEF IndTypeOK, GRenameFinish

LEMMA GDelete_TypeOK ==
  ASSUME IndTypeOK, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>1. /\ doc[p] # Nil
      /\ doc' = [doc EXCEPT ![p] = Nil]
      /\ tomb' = [tomb EXCEPT ![p] = doc[p]]
      /\ udel' = udel \cup {<<p, doc[p]>>}
      /\ seq' = seq + 1 /\ reqs' = reqs + 1
      /\ UNCHANGED <<live, minted, base, acked, conflicts, holder, gw, mv, nextGen, ui, barriers, w, aux>>
  BY DEF GDelete
<1>2. doc[p] \in minted /\ doc[p] \in Handles BY <1>a, <1>c, <1>1 DEF TypeOK, Minted, Opt
<1>3. doc' \in [Paths -> Opt(Handles)] /\ \A r \in Paths : doc'[r] \in Opt(minted)
  BY <1>a, <1>c, <1>1, <1>2 DEF TypeOK, Minted, Opt
<1>4. w' \in [Writers -> Writer] /\ base' \in [Handles -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>5. FrameOK BY <1>a, <1>1, <1>3, <1>4, FrameTypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5 DEF TypeOK, Opt, aux, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts, aux
<1>g. gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] BY <1>a DEF TypeOK
<1>8. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>g, <1>1, <1>3 DEF Minted, Opt, aux
<1>9. Minted' BY <1>c, <1>8, MintedKeep
<1>. QED BY <1>6, <1>7, <1>9 DEF IndTypeOK

------------------------------------------------------------------------------
(* The agent.                                                               *)

LEMMA Edit_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>1. /\ nextGen <= MaxMint
      /\ minted' = minted \cup {<<p, nextGen>>}
      /\ base' = [base EXCEPT ![<<p, nextGen>>] = w[s].baseline[p]]
      /\ w' = [w EXCEPT ![s] = EditW(s, p)]
      /\ nextGen' = nextGen + 1
      /\ UNCHANGED <<live, doc, seq, tomb, acked, conflicts, holder, ui, reqs, barriers, gw, mv, udel,
                     restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Edit, aux
<1>2. nextGen \in Gens /\ <<p, nextGen>> \in Handles BY <1>a, <1>b, <1>1, MintHandle
<1>2a. [w[s].local EXCEPT ![p] = <<p, nextGen>>] \in [Paths -> Opt(Handles)]
  BY <1>0, <1>2, WriterFields, ExceptFun DEF Opt
<1>2b. w[s].integrated \cup {nextGen} \subseteq Gens /\ w[s].unlinked \ {p} \subseteq Paths
  BY <1>0, <1>2, WriterFields
<1>3. EditW(s, p) \in Writer BY <1>0, <1>2, <1>2a, <1>2b, WriterFields DEF EditW, Writer, Opt
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN EditW(s, p) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. base' \in [Handles -> Opt(Handles)] BY <1>a, <1>0, <1>1, <1>2, WriterFields DEF TypeOK, Opt
<1>6. doc' \in [Paths -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
<1>7. FrameOK BY <1>a, <1>1, <1>4, <1>5, <1>6, FrameTypeOK
<1>8. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>5, <1>7 DEF TypeOK, FrameOK
<1>9. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>a, <1>b, <1>1, <1>2 DEF Ghosts, TypeOK, Seed
<1>10. /\ \A q \in Paths : EditW(s, p).local[q] \in Opt(minted')
       /\ \A q \in EditW(s, p).uploads : EditW(s, p).snap[q] \in minted'
  <2>1. \A q \in Paths : [w[s].local EXCEPT ![p] = <<p, nextGen>>][q] \in Opt(minted')
    BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF Minted, Opt
  <2>2. EditW(s, p).local = [w[s].local EXCEPT ![p] = <<p, nextGen>>]
        /\ EditW(s, p).uploads = w[s].uploads /\ EditW(s, p).snap = w[s].snap
    BY DEF EditW
  <2>3. \A q \in w[s].uploads : w[s].snap[q] \in minted' BY <1>c, <1>1 DEF Minted
  <2>. QED BY <2>1, <2>2, <2>3
<1>11. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>12. Minted' BY <1>c, <1>4, <1>10, <1>11, MintedWrite
<1>. QED BY <1>8, <1>9, <1>12 DEF IndTypeOK

LEMMA Delete_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>1. /\ w' = [w EXCEPT ![s] = DeleteW(s, p)]
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Delete, bucket, aux
<1>2. [w[s].local EXCEPT ![p] = Nil] \in [Paths -> Opt(Handles)] BY <1>0, WriterFields, ExceptFun DEF Opt
<1>3. DeleteW(s, p) \in Writer BY <1>0, <1>2, WriterFields DEF DeleteW, Writer, Opt
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN DeleteW(s, p) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. FrameOK BY <1>a, <1>1, <1>4, FrameTypeOK DEF TypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>4, <1>5 DEF TypeOK, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>8. /\ \A q \in Paths : DeleteW(s, p).local[q] \in Opt(minted')
      /\ \A q \in DeleteW(s, p).uploads : DeleteW(s, p).snap[q] \in minted'
  <2>1. \A q \in Paths : [w[s].local EXCEPT ![p] = Nil][q] \in Opt(minted')
    BY <1>0, <1>c, <1>1, WriterFields DEF Minted, Opt
  <2>2. DeleteW(s, p).local = [w[s].local EXCEPT ![p] = Nil]
        /\ DeleteW(s, p).uploads = w[s].uploads /\ DeleteW(s, p).snap = w[s].snap
    BY DEF DeleteW
  <2>3. \A q \in w[s].uploads : w[s].snap[q] \in minted' BY <1>c, <1>1 DEF Minted
  <2>. QED BY <2>1, <2>2, <2>3
<1>9. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>10. Minted' BY <1>c, <1>4, <1>8, <1>9, MintedWrite
<1>. QED BY <1>6, <1>7, <1>10 DEF IndTypeOK

LEMMA Checkout_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. PICK T \in Scopes :
        /\ w' = [w EXCEPT ![s] = CheckoutW(s, T)]
  BY DEF Checkout
<1>2. UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Checkout, bucket, aux
<1>3. T \subseteq Paths
  BY ScopesPaths
<1>4. {Gen(doc[p]) : p \in {q \in T : doc[q] # Nil}} \subseteq Gens
  BY <1>a, <1>3, DocGens
<1>5. /\ CheckoutHeld(s, T) \in [Paths -> Opt(Handles)]
      /\ \A q \in Paths : CheckoutHeld(s, T)[q] \in Opt(minted)
  BY <1>a, <1>c DEF CheckoutHeld, TypeOK, Minted, Opt
<1>6. CheckoutW(s, T) \in Writer BY <1>0, <1>d, <1>3, <1>4, <1>5, WriterFields DEF CheckoutW, Writer, Opt
<1>7. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN CheckoutW(s, T) ELSE w[t]
  BY <1>a, <1>1, <1>6, WriteOne
<1>8. FrameOK BY <1>a, <1>1, <1>2, <1>7, FrameTypeOK DEF TypeOK
<1>9. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5, <1>7, <1>8 DEF TypeOK, FrameOK
<1>10. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>3, <1>4, <1>5, <1>a DEF Ghosts, TypeOK, Seed
<1>11. \A q \in Paths : CheckoutW(s, T).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, <1>5, WriterFields DEF CheckoutW, Minted, Opt
<1>12. \A q \in CheckoutW(s, T).uploads : CheckoutW(s, T).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, <1>3, <1>4, <1>5, WriterFields DEF CheckoutW, CheckoutHeld, Minted, Opt
<1>13. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>14. Minted' BY <1>c, <1>7, <1>11, <1>12, <1>13, MintedWrite
<1>. QED BY <1>9, <1>10, <1>14 DEF IndTypeOK

------------------------------------------------------------------------------
(* The barrier: consume, scan, upload.                                      *)

LEMMA Consume_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Consume(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                  nextGen, ui, reqs, barriers, restarts, syncs, rescopes, upped, copies, orig>>
  BY DEF Consume
<1>2. /\ w' \in [Writers -> Writer]
      /\ \A t \in Writers : w'[t] = IF t = s THEN w'[s] ELSE w[t]
      /\ \A q \in Paths : w'[s].local[q] \in Opt(minted')
      /\ \A q \in w'[s].uploads : w'[s].snap[q] \in minted'
      /\ regressed' \in BOOLEAN /\ fails' \in Nat
  <2>1. CASE CheapPath(s)
    <3>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s)] /\ UNCHANGED <<regressed, fails>> BY <2>1 DEF Consume
    <3>2. ConsumeCheapW(s) \in Writer BY <1>0, WriterFields DEF ConsumeCheapW, Writer
    <3>3. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN ConsumeCheapW(s) ELSE w[t]
      BY <1>a, <3>1, <3>2, WriteOne
    <3>4. /\ \A q \in Paths : ConsumeCheapW(s).local[q] \in Opt(minted')
          /\ \A q \in ConsumeCheapW(s).uploads : ConsumeCheapW(s).snap[q] \in minted'
      BY <1>c, <1>1 DEF ConsumeCheapW, Minted
    <3>. QED BY <1>a, <3>1, <3>3, <3>4 DEF TypeOK
  <2>2. CASE ~CheapPath(s)
    <3>1. PICK fail \in SUBSET ConsumeOwed(s) :
            /\ fails + Cardinality(fail) <= MaxFetchFails
            /\ w' = [w EXCEPT ![s] = ConsumeW(s, fail)]
            /\ fails' = fails + Cardinality(fail)
            /\ regressed' = (regressed \/ \E p \in ConsumeOwed(s) \ fail : Back(s, p))
      BY <2>2 DEF Consume
    <3>2. fail \subseteq Paths /\ ConsumeTaken(s, fail) \subseteq Paths BY DEF ConsumeOwed, ConsumeConv, ConsumeTaken
    <3>3. Cardinality(fail) \in Nat BY <3>2, CardPaths
    <3>4. {Gen(doc[p]) : p \in {q \in ConsumeTaken(s, fail) : doc[q] # Nil}} \subseteq Gens
      BY <1>a, <3>2, DocGens
    <3>5. ConsumeW(s, fail) \in Writer BY <1>0, <1>d, <3>2, <3>4, WriterFields DEF ConsumeW, Writer, Opt
    <3>6. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN ConsumeW(s, fail) ELSE w[t]
      BY <1>a, <3>1, <3>5, WriteOne
    <3>7. /\ \A q \in Paths : ConsumeW(s, fail).local[q] \in Opt(minted')
          /\ \A q \in ConsumeW(s, fail).uploads : ConsumeW(s, fail).snap[q] \in minted'
      <4>1. \A q \in Paths : ConsumeW(s, fail).local[q] \in Opt(minted')
        BY <1>0, <1>c, <1>1, WriterFields DEF ConsumeW, Minted, Opt
      <4>2. \A q \in ConsumeW(s, fail).uploads : ConsumeW(s, fail).snap[q] \in minted'
        BY <1>c, <1>1 DEF ConsumeW, Minted
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <1>a, <3>1, <3>3, <3>6, <3>7 DEF TypeOK
  <2>. QED BY <2>1, <2>2
<1>3. FrameOK BY <1>a, <1>1, <1>2, FrameTypeOK DEF TypeOK
<1>4. TypeOK' BY <1>a, <1>1, <1>2, <1>3 DEF TypeOK, FrameOK
<1>5. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>6. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>7. Minted' BY <1>c, <1>2, <1>6, MintedWrite
<1>. QED BY <1>4, <1>5, <1>7 DEF IndTypeOK

LEMMA Scan_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Scan(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. PICK dels \in SUBSET ScanAbsent(s) :
        /\ w' = [w EXCEPT ![s] = ScanW(s, dels)]
  BY DEF Scan
<1>2. /\ barriers' = barriers + 1
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Scan, bucket, aux
<1>3. dels \subseteq Paths /\ ScanUps(s) \subseteq Paths
  BY DEF ScanAbsent, ScanUps, ScanDirty
<1>4. ScanW(s, dels) \in Writer BY <1>0, <1>d, <1>3, WriterFields DEF ScanW, Writer, Opt
<1>5. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN ScanW(s, dels) ELSE w[t]
  BY <1>a, <1>1, <1>4, WriteOne
<1>6. FrameOK BY <1>a, <1>1, <1>2, <1>5, FrameTypeOK DEF TypeOK
<1>7. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>5, <1>6 DEF TypeOK, FrameOK
<1>8. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>3, <1>a DEF Ghosts, TypeOK, Seed
<1>9. \A q \in Paths : ScanW(s, dels).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF ScanW, Minted, Opt
<1>10. \A q \in ScanW(s, dels).uploads : ScanW(s, dels).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, <1>3, WriterFields DEF ScanW, ScanUps, ScanDirty, Minted, Opt
<1>11. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>12. Minted' BY <1>c, <1>5, <1>9, <1>10, <1>11, MintedWrite
<1>. QED BY <1>7, <1>8, <1>12 DEF IndTypeOK

LEMMA Skip_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Skip(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ w' = [w EXCEPT ![s] = SkipW(s)]
      /\ barriers' = barriers + 1
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Skip, bucket, aux
<1>2. TRUE OBVIOUS
<1>3. SkipW(s) \in Writer BY <1>0, <1>d, WriterFields DEF SkipW, Writer
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN SkipW(s) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. FrameOK BY <1>a, <1>1, <1>2, <1>4, FrameTypeOK DEF TypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>5 DEF TypeOK, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>a DEF Ghosts, TypeOK, Seed
<1>8. \A q \in Paths : SkipW(s).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF SkipW, Minted, Opt
<1>9. \A q \in SkipW(s).uploads : SkipW(s).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF SkipW, Minted, Opt
<1>10. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>11. Minted' BY <1>c, <1>4, <1>8, <1>9, <1>10, MintedWrite
<1>. QED BY <1>6, <1>7, <1>11 DEF IndTypeOK

LEMMA Upload_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>. DEFINE h == w[s].snap[p]
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>1. /\ p \in w[s].uploads
      /\ UNCHANGED <<doc, seq, tomb, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails>>
  BY DEF Upload
<1>2. h \in minted /\ h \in Handles BY <1>a, <1>c, <1>1 DEF TypeOK, Minted
<1>3. CASE h \notin upped
  <2>1. /\ live' = live \cup {h} /\ upped' = upped \cup {h}
        /\ w' = [w EXCEPT ![s] = UploadW(s, p)]
        /\ UNCHANGED <<minted, base, copies, orig>>
    BY <1>3 DEF Upload
  <2>2. UploadW(s, p) \in Writer BY <1>0, WriterFields DEF UploadW, Writer
  <2>3. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN UploadW(s, p) ELSE w[t]
    BY <1>a, <2>1, <2>2, WriteOne
  <2>4. FrameOK BY <1>a, <1>1, <2>1, <2>3, FrameTypeOK DEF TypeOK
  <2>5. TypeOK' BY <1>a, <1>1, <1>2, <2>1, <2>3, <2>4 DEF TypeOK, FrameOK
  <2>6. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <2>1 DEF Ghosts
  <2>7. /\ \A q \in Paths : UploadW(s, p).local[q] \in Opt(minted')
        /\ \A q \in UploadW(s, p).uploads : UploadW(s, p).snap[q] \in minted'
    BY <1>c, <2>1 DEF UploadW, Minted
  <2>7a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
    BY <1>c, <1>1, <2>1 DEF Minted, Opt
  <2>8. Minted' BY <1>c, <2>3, <2>7, <2>7a, MintedWrite
  <2>. QED BY <2>5, <2>6, <2>8 DEF IndTypeOK
<1>4. CASE h \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. /\ copies < MaxCopies
        /\ live' = live \cup {c} /\ upped' = upped \cup {c} /\ minted' = minted \cup {c}
        /\ base' = [base EXCEPT ![c] = base[h]]
        /\ orig' = [orig EXCEPT ![c] = Content(h)]
        /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]
        /\ copies' = copies + 1
    BY <1>4 DEF Upload
  <2>2. c \in Handles BY <1>b, <2>1, CopyHandle
  <2>3. Content(h) \in Opt(Handles) BY <1>b, <1>2 DEF Ghosts, Content, Opt
  <2>4. /\ [w[s].snap EXCEPT ![p] = c] \in [Paths -> Opt(Handles)]
        /\ [w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]] \in [Paths -> Opt(Handles)]
    <3>1. c \in Opt(Handles) /\ (IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]) \in Opt(Handles)
      BY <1>0, <2>2, WriterFields DEF Opt
    <3>. QED BY <1>0, <3>1, WriterFields, ExceptFun
  <2>5. UploadCopyW(s, p, c) \in Writer BY <1>0, <2>4, WriterFields DEF UploadCopyW, Writer, Opt
  <2>6. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN UploadCopyW(s, p, c) ELSE w[t]
    BY <1>a, <2>1, <2>5, WriteOne
  <2>7. base' \in [Handles -> Opt(Handles)] /\ orig' \in [Handles -> Opt(Handles)]
    BY <1>a, <1>b, <1>2, <2>1, <2>2, <2>3 DEF TypeOK, Ghosts
  <2>8. doc' \in [Paths -> Opt(Handles)] BY <1>a, <1>1 DEF TypeOK
  <2>9. FrameOK BY <1>a, <2>6, <2>7, <2>8, FrameTypeOK
  <2>10. TypeOK' BY <1>a, <1>1, <1>2, <2>1, <2>2, <2>6, <2>7, <2>9 DEF TypeOK, FrameOK
  <2>11. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>a, <1>b, <1>1, <2>1, <2>2, <2>7 DEF Ghosts, TypeOK
  <2>12. /\ \A q \in Paths : UploadCopyW(s, p, c).local[q] \in Opt(minted')
         /\ \A q \in UploadCopyW(s, p, c).uploads : UploadCopyW(s, p, c).snap[q] \in minted'
    <3>1. \A q \in Paths : [w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]][q] \in Opt(minted')
      BY <1>0, <1>c, <2>1, <2>2, WriterFields DEF Minted, Opt
    <3>2. \A q \in w[s].uploads : [w[s].snap EXCEPT ![p] = c][q] \in minted'
      BY <1>0, <1>c, <2>1, <2>2, WriterFields DEF Minted
    <3>3. /\ UploadCopyW(s, p, c).local = [w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]]
          /\ UploadCopyW(s, p, c).uploads = w[s].uploads
          /\ UploadCopyW(s, p, c).snap = [w[s].snap EXCEPT ![p] = c]
      BY DEF UploadCopyW
    <3>. QED BY <3>1, <3>2, <3>3
  <2>12a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
    BY <1>c, <1>1, <2>1 DEF Minted, Opt
  <2>13. Minted' BY <1>c, <2>6, <2>12, <2>12a, MintedWrite
  <2>. QED BY <2>10, <2>11, <2>13 DEF IndTypeOK
<1>. QED BY <1>3, <1>4

------------------------------------------------------------------------------
(* The commit section.                                                      *)

LEMMA PullOnly_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ w' = [w EXCEPT ![s] = PullOnlyW(s)]
      /\ w[s].uploads = {}
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF PullOnly, bucket, aux
<1>2. TRUE OBVIOUS
<1>3. PullOnlyW(s) \in Writer BY <1>0, <1>d, WriterFields DEF PullOnlyW, Writer, Opt
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN PullOnlyW(s) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. FrameOK BY <1>a, <1>1, <1>2, <1>4, FrameTypeOK DEF TypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>5 DEF TypeOK, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>a DEF Ghosts, TypeOK, Seed
<1>8. \A q \in Paths : PullOnlyW(s).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF PullOnlyW, Minted, Opt
<1>9. \A q \in PullOnlyW(s).uploads : PullOnlyW(s).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF PullOnlyW, Minted, Opt
<1>10. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>11. Minted' BY <1>c, <1>4, <1>8, <1>9, <1>10, MintedWrite
<1>. QED BY <1>6, <1>7, <1>11 DEF IndTypeOK

LEMMA Claim_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Claim(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ w' = [w EXCEPT ![s] = ClaimW(s)]
      /\ holder' = s
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Claim, aux
<1>2. TRUE OBVIOUS
<1>3. ClaimW(s) \in Writer BY <1>0, <1>d, WriterFields DEF ClaimW, Writer
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN ClaimW(s) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. FrameOK BY <1>a, <1>1, <1>2, <1>4, FrameTypeOK DEF TypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>5 DEF TypeOK, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>a DEF Ghosts, TypeOK, Seed
<1>8. \A q \in Paths : ClaimW(s).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF ClaimW, Minted, Opt
<1>9. \A q \in ClaimW(s).uploads : ClaimW(s).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF ClaimW, Minted, Opt
<1>10. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>11. Minted' BY <1>c, <1>4, <1>8, <1>9, <1>10, MintedWrite
<1>. QED BY <1>6, <1>7, <1>11 DEF IndTypeOK

LEMMA Verify_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Verify(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ w' = [w EXCEPT ![s] = VerifyW(s)]
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Verify, bucket, aux
<1>2. TRUE OBVIOUS
<1>3. VerifyW(s) \in Writer BY <1>0, <1>d, WriterFields DEF VerifyW, Writer
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN VerifyW(s) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. FrameOK BY <1>a, <1>1, <1>2, <1>4, FrameTypeOK DEF TypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>5 DEF TypeOK, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>a DEF Ghosts, TypeOK, Seed
<1>8. \A q \in Paths : VerifyW(s).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF VerifyW, Minted, Opt
<1>9. \A q \in VerifyW(s).uploads : VerifyW(s).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF VerifyW, Minted, Opt
<1>10. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>11. Minted' BY <1>c, <1>4, <1>8, <1>9, <1>10, MintedWrite
<1>. QED BY <1>6, <1>7, <1>11 DEF IndTypeOK

LEMMA Install_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Install(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ doc' = InstallInst(s)
      /\ seq' = IF InstallInst(s) = doc THEN seq ELSE seq + 1
      /\ tomb' = [p \in Paths |-> IF InstallInst(s)[p] # Nil THEN Nil
                                  ELSE IF doc[p] # Nil THEN doc[p]
                                  ELSE tomb[p]]
      /\ conflicts' = conflicts \cup {<<p, w[s].snap[p]>> : p \in w[s].gone}
                                \cup {<<p, doc[p]>> : p \in InstallContested(s)}
                                \cup {<<p, DeletedAt(s, p)>> : p \in InstallDelOverridden(s)}
                                \cup {<<p, doc[p]>> : p \in InstallDelOver(s)}
      /\ w' = [w EXCEPT ![s] = InstallW(s)]
      /\ UNCHANGED <<live, minted, base, acked, holder, gw, mv, udel, nextGen, ui, reqs, barriers,
                     restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Install, aux
<1>2. \A p \in InstallMine(s) : w[s].snap[p] \in minted /\ w[s].snap[p] \in Handles
  BY <1>a, <1>0, <1>c, WriterFields DEF InstallMine, Minted, TypeOK
<1>3. InstallInst(s) \in [Paths -> Opt(Handles)] /\ \A p \in Paths : InstallInst(s)[p] \in Opt(minted)
  BY <1>a, <1>0, <1>c, <1>2, WriterFields DEF InstallInst, TypeOK, Minted, Opt
<1>4. InstallRetired(s) \subseteq Handles BY <1>a DEF InstallRetired, TypeOK, Opt
<1>5. conflicts' \subseteq Paths \X Opt(Handles)
  BY <1>a, <1>0, <1>1, WriterFields DEF TypeOK, InstallMine, InstallContested, InstallDelOverridden, InstallDelOver, DeletedAt, Opt
<1>6. InstallW(s) \in Writer BY <1>0, <1>d, <1>3, <1>4, WriterFields DEF InstallW, Writer, Opt
<1>7. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN InstallW(s) ELSE w[t]
  BY <1>a, <1>1, <1>6, WriteOne
<1>8. tomb' \in [Paths -> Opt(Handles)] BY <1>a, <1>1, <1>3 DEF TypeOK, Opt
<1>9. FrameOK BY <1>a, <1>1, <1>3, <1>7, FrameTypeOK DEF TypeOK
<1>10. TypeOK' BY <1>a, <1>1, <1>3, <1>5, <1>7, <1>8, <1>9 DEF TypeOK, FrameOK
<1>11. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>12. /\ \A q \in Paths : InstallW(s).local[q] \in Opt(minted')
       /\ \A q \in InstallW(s).uploads : InstallW(s).snap[q] \in minted'
  BY <1>c, <1>1 DEF InstallW, Minted
<1>12a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>3 DEF Minted, Opt
<1>13. Minted' BY <1>c, <1>7, <1>12, <1>12a, MintedWrite
<1>. QED BY <1>10, <1>11, <1>13 DEF IndTypeOK

LEMMA Collect_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Collect(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>1. /\ live' \subseteq live
      /\ w' = [w EXCEPT ![s] = CollectW(s)]
      /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Collect, aux
<1>2. CollectW(s) \in Writer BY <1>0, WriterFields DEF CollectW, Writer
<1>3. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN CollectW(s) ELSE w[t]
  BY <1>a, <1>1, <1>2, WriteOne
<1>4. FrameOK BY <1>a, <1>1, <1>3, FrameTypeOK DEF TypeOK
<1>5. TypeOK' BY <1>a, <1>1, <1>3, <1>4 DEF TypeOK, FrameOK
<1>6. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>7. /\ \A q \in Paths : CollectW(s).local[q] \in Opt(minted')
      /\ \A q \in CollectW(s).uploads : CollectW(s).snap[q] \in minted'
  BY <1>c, <1>1 DEF CollectW, Minted
<1>7a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>8. Minted' BY <1>c, <1>3, <1>7, <1>7a, MintedWrite
<1>. QED BY <1>5, <1>6, <1>8 DEF IndTypeOK

LEMMA Sweep_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>1. /\ live' = live \ {h}
      /\ UNCHANGED <<minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, w, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Sweep, aux
<1>2. FrameOK BY <1>a, <1>1, FrameTypeOK DEF TypeOK
<1>3. TypeOK' BY <1>a, <1>1, <1>2 DEF TypeOK, FrameOK
<1>4. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1 DEF Ghosts
<1>5. /\ minted \subseteq minted' /\ w' = w
      /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1 DEF Minted, Opt
<1>6. Minted' BY <1>c, <1>5, MintedKeep
<1>. QED BY <1>3, <1>4, <1>6 DEF IndTypeOK

LEMMA Finish_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Finish(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ w' = [w EXCEPT ![s] = FinishW(s)]
      /\ holder' = "none"
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF Finish, aux
<1>2. TRUE OBVIOUS
<1>3. FinishW(s) \in Writer BY <1>0, <1>d, WriterFields DEF FinishW, Writer, Opt
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN FinishW(s) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. FrameOK BY <1>a, <1>1, <1>2, <1>4, FrameTypeOK DEF TypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>5 DEF TypeOK, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>a DEF Ghosts, TypeOK, Seed
<1>8. \A q \in Paths : FinishW(s).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF FinishW, Minted, Opt
<1>9. \A q \in FinishW(s).uploads : FinishW(s).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF FinishW, Minted, Opt
<1>10. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>11. Minted' BY <1>c, <1>4, <1>8, <1>9, <1>10, MintedWrite
<1>. QED BY <1>6, <1>7, <1>11 DEF IndTypeOK

------------------------------------------------------------------------------
(* The restart and the sync.                                                *)

LEMMA Restart_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Restart(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ w' = [w EXCEPT ![s] = RestartW(s)]
      /\ holder' = IF holder = s THEN "none" ELSE holder
      /\ restarts' = restarts + 1
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, gw, mv, udel,
                     nextGen, ui, reqs, barriers, regressed, syncs, rescopes, fails, upped, copies, orig>>
  BY DEF Restart
<1>2. TRUE OBVIOUS
<1>3. RestartW(s) \in Writer BY <1>0, <1>d, WriterFields DEF RestartW, Writer, Opt
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN RestartW(s) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. FrameOK BY <1>a, <1>1, <1>2, <1>4, FrameTypeOK DEF TypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>5 DEF TypeOK, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>a DEF Ghosts, TypeOK, Seed
<1>8. \A q \in Paths : RestartW(s).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF RestartW, Minted, Opt
<1>9. \A q \in RestartW(s).uploads : RestartW(s).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF RestartW, Minted, Opt
<1>10. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>11. Minted' BY <1>c, <1>4, <1>8, <1>9, <1>10, MintedWrite
<1>. QED BY <1>6, <1>7, <1>11 DEF IndTypeOK

LEMMA Sync_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, Sync(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. PICK fail \in SUBSET SyncAll(s) :
        /\ fails + Cardinality(fail) <= MaxFetchFails
        /\ fails' = fails + Cardinality(fail)
        /\ regressed' = (regressed \/ \E p \in SyncOwed(s, fail) : Back(s, p))
        /\ w' = [w EXCEPT ![s] = SyncW(s, fail)]
  BY DEF Sync
<1>2. /\ syncs' = syncs + 1
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, rescopes, upped, copies, orig>>
  BY DEF Sync, bucket
<1>3. fail \subseteq Paths /\ SyncOwed(s, fail) \subseteq Paths
  BY DEF SyncAll, SyncOwed
<1>4. Cardinality(fail) \in Nat
  BY <1>3, CardPaths
<1>5. {Gen(doc[p]) : p \in {q \in SyncOwed(s, fail) : doc[q] # Nil}} \subseteq Gens
  BY <1>a, <1>3, DocGens
<1>6. SyncBl(s, fail) \in [Paths -> Opt(Handles)]
  BY <1>a, <1>0, WriterFields DEF SyncBl, TypeOK, Opt
<1>7. SyncW(s, fail) \in Writer BY <1>0, <1>d, <1>3, <1>4, <1>5, <1>6, WriterFields DEF SyncW, Writer, Opt
<1>8. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN SyncW(s, fail) ELSE w[t]
  BY <1>a, <1>1, <1>7, WriteOne
<1>9. FrameOK BY <1>a, <1>1, <1>2, <1>8, FrameTypeOK DEF TypeOK
<1>10. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>8, <1>9 DEF TypeOK, FrameOK
<1>11. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>a DEF Ghosts, TypeOK, Seed
<1>12. \A q \in Paths : SyncW(s, fail).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF SyncW, Minted, Opt
<1>13. \A q \in SyncW(s, fail).uploads : SyncW(s, fail).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, WriterFields DEF SyncW, SyncBl, Minted, Opt
<1>14. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>15. Minted' BY <1>c, <1>8, <1>12, <1>13, <1>14, MintedWrite
<1>. QED BY <1>10, <1>11, <1>15 DEF IndTypeOK

------------------------------------------------------------------------------
(* The narrow / widen verb.                                                 *)

LEMMA RescopeBegin_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. PICK T \in Scopes \ {w[s].scope} :
        /\ \A p \in Leaving(s, T) : w[s].local[p] = w[s].baseline[p]
        /\ w' = [w EXCEPT ![s] = RescopeBeginW(s, T)]
  BY DEF RescopeBegin
<1>2. /\ rescopes' = rescopes + 1
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, fails, upped, copies, orig>>
  BY DEF RescopeBegin, bucket
<1>3. T \subseteq Paths /\ Leaving(s, T) \subseteq Paths
  BY ScopesPaths DEF Leaving
<1>4. RescopeBeginW(s, T) \in Writer BY <1>0, <1>d, <1>3, WriterFields DEF RescopeBeginW, Writer, Opt
<1>5. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN RescopeBeginW(s, T) ELSE w[t]
  BY <1>a, <1>1, <1>4, WriteOne
<1>6. FrameOK BY <1>a, <1>1, <1>2, <1>5, FrameTypeOK DEF TypeOK
<1>7. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>5, <1>6 DEF TypeOK, FrameOK
<1>8. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>3, <1>a DEF Ghosts, TypeOK, Seed
<1>9. \A q \in Paths : RescopeBeginW(s, T).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF RescopeBeginW, Minted, Opt
<1>10. \A q \in RescopeBeginW(s, T).uploads : RescopeBeginW(s, T).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, <1>3, WriterFields DEF RescopeBeginW, Minted, Opt
<1>11. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>12. Minted' BY <1>c, <1>5, <1>9, <1>10, <1>11, MintedWrite
<1>. QED BY <1>7, <1>8, <1>12 DEF IndTypeOK

LEMMA RescopeFirst_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ w' = [w EXCEPT ![s] = RescopeFirstW(s)]
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF RescopeFirst, bucket, aux
<1>2. TRUE OBVIOUS
<1>3. KeepSet(s) \subseteq Paths /\ RescopeFirstDd(s) \subseteq Paths
  BY <1>0, WriterFields DEF KeepSet, RescopeFirstDd
<1>4. RescopeFirstW(s) \in Writer BY <1>0, <1>d, <1>3, WriterFields DEF RescopeFirstW, Writer, Opt
<1>5. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN RescopeFirstW(s) ELSE w[t]
  BY <1>a, <1>1, <1>4, WriteOne
<1>6. FrameOK BY <1>a, <1>1, <1>2, <1>5, FrameTypeOK DEF TypeOK
<1>7. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>5, <1>6 DEF TypeOK, FrameOK
<1>8. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>3, <1>a DEF Ghosts, TypeOK, Seed
<1>9. \A q \in Paths : RescopeFirstW(s).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF RescopeFirstW, Minted, Opt
<1>10. \A q \in RescopeFirstW(s).uploads : RescopeFirstW(s).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, <1>3, WriterFields DEF RescopeFirstW, Minted, Opt
<1>11. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>12. Minted' BY <1>c, <1>5, <1>9, <1>10, <1>11, MintedWrite
<1>. QED BY <1>7, <1>8, <1>12 DEF IndTypeOK

LEMMA RescopeSecond_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. PICK wfail \in SUBSET {p \in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \/ ~WidenKeepsLocal} :
        /\ fails + Cardinality(wfail) <= MaxFetchFails
        /\ fails' = fails + Cardinality(wfail)
        /\ w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>2. UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                  nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, upped, copies, orig>>
  BY DEF RescopeSecond, bucket
<1>3. /\ w[s].sTgt \subseteq Paths /\ RescopeAdd(s) \subseteq Paths
      /\ RescopeFetch0(s) \subseteq Paths /\ RescopeFetch(s, wfail) \subseteq Paths
  BY <1>0, WriterFields DEF RescopeAdd, RescopeKept, RescopeFetch0, RescopeFetch
<1>4. Cardinality(wfail) \in Nat BY <1>3, CardPaths
<1>5. /\ RescopeLocal1(s) \in [Paths -> Opt(Handles)] /\ RescopeBase1(s) \in [Paths -> Opt(Handles)]
      /\ \A p \in Paths : RescopeLocal1(s)[p] \in Opt(minted)
  BY <1>0, <1>c, WriterFields DEF RescopeLocal1, RescopeBase1, Minted, Opt
<1>6. \A p \in RescopeFetch(s, wfail) : doc[p] \in Handles /\ Gen(doc[p]) \in Gens
  BY <1>a, <1>0, <1>3, HandleGen, WriterFields DEF RescopeFetch, RescopeFetch0, RescopeAdd, TypeOK, Opt
<1>7. RescopeSecondW(s, wfail) \in Writer
  BY <1>0, <1>d, <1>3, <1>5, <1>6, WriterFields DEF RescopeSecondW, Writer, Opt
<1>8. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN RescopeSecondW(s, wfail) ELSE w[t]
  BY <1>a, <1>1, <1>7, WriteOne
<1>9. FrameOK BY <1>a, <1>2, <1>8, FrameTypeOK DEF TypeOK
<1>10. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>8, <1>9 DEF TypeOK, FrameOK
<1>11. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>2 DEF Ghosts
<1>12. /\ \A p \in Paths : RescopeSecondW(s, wfail).local[p] \in Opt(minted')
       /\ \A q \in RescopeSecondW(s, wfail).uploads : RescopeSecondW(s, wfail).snap[q] \in minted'
  BY <1>0, <1>c, <1>2, <1>5 DEF RescopeSecondW, Minted, Opt
<1>12a. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>2 DEF Minted, Opt
<1>13. Minted' BY <1>c, <1>8, <1>12, <1>12a, MintedWrite
<1>. QED BY <1>10, <1>11, <1>13 DEF IndTypeOK

------------------------------------------------------------------------------
(* A reader's tick.                                                         *)

LEMMA RPullRead_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. /\ w' = [w EXCEPT ![s] = RPullReadW(s)]
      /\ UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, regressed, rescopes, fails, upped, copies, orig>>
  BY DEF RPullRead, bucket, aux
<1>2. TRUE OBVIOUS
<1>3. RPullReadW(s) \in Writer BY <1>0, <1>d, WriterFields DEF RPullReadW, Writer
<1>4. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN RPullReadW(s) ELSE w[t]
  BY <1>a, <1>1, <1>3, WriteOne
<1>5. FrameOK BY <1>a, <1>1, <1>2, <1>4, FrameTypeOK DEF TypeOK
<1>6. TypeOK' BY <1>a, <1>1, <1>2, <1>4, <1>5 DEF TypeOK, FrameOK
<1>7. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>a DEF Ghosts, TypeOK, Seed
<1>8. \A q \in Paths : RPullReadW(s).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF RPullReadW, Minted, Opt
<1>9. \A q \in RPullReadW(s).uploads : RPullReadW(s).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF RPullReadW, Minted, Opt
<1>10. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>11. Minted' BY <1>c, <1>4, <1>8, <1>9, <1>10, MintedWrite
<1>. QED BY <1>6, <1>7, <1>11 DEF IndTypeOK

LEMMA RPullSync_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndTypeOK'
<1>a. TypeOK BY DEF IndTypeOK
<1>b. Ghosts /\ mv = Nil /\ nextGen > Seed BY DEF IndTypeOK
<1>c. Minted BY DEF IndTypeOK
<1>0. w[s] \in Writer BY <1>a DEF TypeOK
<1>d. doc \in [Paths -> Opt(Handles)] /\ seq \in Nat /\ live \subseteq Handles BY <1>a DEF TypeOK
<1>1. PICK fail \in SUBSET SyncAll(s) :
        /\ fails + Cardinality(fail) <= MaxFetchFails
        /\ fails' = fails + Cardinality(fail)
        /\ regressed' = (regressed \/ \E p \in SyncOwed(s, fail) : Back(s, p))
        /\ w' = [w EXCEPT ![s] = RPullSyncW(s, fail)]
  BY DEF RPullSync
<1>2. UNCHANGED <<live, minted, doc, seq, tomb, base, acked, conflicts, holder, gw, mv, udel,
                     nextGen, ui, reqs, barriers, restarts, syncs, rescopes, upped, copies, orig>>
  BY DEF RPullSync, bucket
<1>3. fail \subseteq Paths /\ SyncOwed(s, fail) \subseteq Paths
  BY DEF SyncAll, SyncOwed
<1>4. Cardinality(fail) \in Nat
  BY <1>3, CardPaths
<1>5. {Gen(doc[p]) : p \in {q \in SyncOwed(s, fail) : doc[q] # Nil}} \subseteq Gens
  BY <1>a, <1>3, DocGens
<1>6. SyncBl(s, fail) \in [Paths -> Opt(Handles)]
  BY <1>a, <1>0, WriterFields DEF SyncBl, TypeOK, Opt
<1>7. RPullSyncW(s, fail) \in Writer BY <1>0, <1>d, <1>3, <1>4, <1>5, <1>6, WriterFields DEF RPullSyncW, Writer, Opt
<1>8. w' \in [Writers -> Writer] /\ \A t \in Writers : w'[t] = IF t = s THEN RPullSyncW(s, fail) ELSE w[t]
  BY <1>a, <1>1, <1>7, WriteOne
<1>9. FrameOK BY <1>a, <1>1, <1>2, <1>8, FrameTypeOK DEF TypeOK
<1>10. TypeOK' BY <1>a, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>8, <1>9 DEF TypeOK, FrameOK
<1>11. Ghosts' /\ mv' = Nil /\ nextGen' > Seed BY <1>b, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>a DEF Ghosts, TypeOK, Seed
<1>12. \A q \in Paths : RPullSyncW(s, fail).local[q] \in Opt(minted')
  BY <1>0, <1>c, <1>1, <1>2, WriterFields DEF RPullSyncW, Minted, Opt
<1>13. \A q \in RPullSyncW(s, fail).uploads : RPullSyncW(s, fail).snap[q] \in minted'
  BY <1>0, <1>c, <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, WriterFields DEF RPullSyncW, SyncBl, Minted, Opt
<1>14. /\ minted \subseteq minted' /\ \A q \in Paths : doc'[q] \in Opt(minted') /\ \A q \in Paths : gw'[q] \in Opt(minted')
  BY <1>c, <1>1, <1>2 DEF Minted, Opt
<1>15. Minted' BY <1>c, <1>8, <1>12, <1>13, <1>14, MintedWrite
<1>. QED BY <1>10, <1>11, <1>15 DEF IndTypeOK

------------------------------------------------------------------------------
(* The retire age and the document's reader: no frame, `anc` unchanged.   *)

LEMMA Age_TypeOK ==
  ASSUME IndTypeOK, Age, UNCHANGED anc
  PROVE  IndTypeOK'
BY DEF IndTypeOK, TypeOK, Ghosts, Minted, Age, aux

LEMMA Reap_TypeOK ==
  ASSUME IndTypeOK, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndTypeOK'
BY DEF IndTypeOK, TypeOK, Ghosts, Minted, Reap, aux

LEMMA RLoad_TypeOK ==
  ASSUME IndTypeOK, RLoad, UNCHANGED anc
  PROVE  IndTypeOK'
BY DEF IndTypeOK, TypeOK, Ghosts, Minted, RLoad, aux

------------------------------------------------------------------------------
(* The step, and the theorem.                                               *)

LEMMA Next_TypeOK == IndTypeOK /\ Next => IndTypeOK'
<1>. SUFFICES ASSUME IndTypeOK, Next PROVE IndTypeOK' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndTypeOK' BY <3>1, GPut_TypeOK
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndTypeOK' BY <3>2, GCas_TypeOK
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndTypeOK' BY <3>3, GDelete_TypeOK
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndTypeOK' BY <3>4, GRename_TypeOK
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_TypeOK
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndTypeOK' BY <3>1, Checkout_TypeOK
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndTypeOK' BY <3>2, Consume_TypeOK
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndTypeOK' BY <3>3, Scan_TypeOK
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndTypeOK' BY <3>4, Skip_TypeOK
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndTypeOK' BY <3>5, PullOnly_TypeOK
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndTypeOK' BY <3>6, Claim_TypeOK
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndTypeOK' BY <3>7, Verify_TypeOK
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndTypeOK' BY <3>8, Install_TypeOK
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndTypeOK' BY <3>9, Collect_TypeOK
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndTypeOK' BY <3>10, Finish_TypeOK
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndTypeOK' BY <3>11, Restart_TypeOK
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndTypeOK' BY <3>12, Sync_TypeOK
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndTypeOK' BY <3>13, RescopeBegin_TypeOK
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndTypeOK' BY <3>14, RescopeFirst_TypeOK
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndTypeOK' BY <3>15, RescopeSecond_TypeOK
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndTypeOK' BY <3>16, RPullRead_TypeOK
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndTypeOK' BY <3>17, RPullSync_TypeOK
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndTypeOK' BY <3>18, Edit_TypeOK
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndTypeOK' BY <3>19, Delete_TypeOK
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndTypeOK' BY <3>20, Upload_TypeOK
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndTypeOK' BY <3>21, Sweep_TypeOK
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_TypeOK
  <2>2. CASE RLoad BY <2>2, RLoad_TypeOK
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndTypeOK' BY <2>3, Reap_TypeOK
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

THEOREM TypeInvariant == Spec => []TypeOK
<1>1. Init => IndTypeOK BY Init_TypeOK
<1>2. IndTypeOK /\ [Next]_vars => IndTypeOK'
  <2>1. IndTypeOK /\ Next => IndTypeOK' BY Next_TypeOK
  <2>2. IndTypeOK /\ UNCHANGED vars => IndTypeOK'
    BY DEF IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
  <2>. QED BY <2>1, <2>2
<1>3. IndTypeOK => TypeOK BY DEF IndTypeOK
<1>. QED BY <1>1, <1>2, <1>3, PTL DEF Spec

------------------------------------------------------------------------------
(* M1: Inv_OneHolder, Prop_DeleteSettles, Prop_NarrowNeverDeletes.          *)

CC == {"claimed", "cased"}
\* I1: the holder is in its commit section, and on.
Holder == holder # "none" => w[holder].pc \in CC /\ w[holder].st = "on"
\* I6: at the CAS every delete landed (DeleteWinsPreserved: a delete over a
\* foreign change applies).
Cased == \A s \in Writers : w[s].pc = "cased" => \A p \in w[s].deletes : w[s].inst[p] = Nil
\* The scan's two sets are disjoint; what the verify withheld is an upload.
Mine == \A s \in Writers : w[s].deletes \cap w[s].uploads = {} /\ w[s].gone \subseteq w[s].uploads
\* From the scan to the finish, no path the barrier publishes or deletes is
\* one a rescope unlinked (such a path is clean: I7).
Ups == \A s \in Writers : w[s].pc \in {"scanned", "claimed", "cased"} =>
         (w[s].uploads \cup w[s].deletes) \cap w[s].unlinked = {}
\* A writer that is off has never run: every step but Checkout needs On(s).
Off == \A s \in Writers : w[s].st = "off" => w[s] = WriterInit
\* I7a: the barrier runs with no rescope in flight.
R1 == \A s \in Writers : w[s].pc \in {"consumed", "scanned", "claimed", "cased"} => w[s].sStage = "none"
\* I7b: a path a rescope unlinked is clean until the agent touches it.
R2 == \A s \in Writers, p \in Paths :
        (p \in w[s].unlinked /\ w[s].sStage = "none") => w[s].local[p] = w[s].baseline[p]
\* I7b with a rescope in flight: clean, or uncited by the first half --
\* dropped and not kept, its baseline gone, its bytes those the intent
\* recorded (so the second half unlinks it).
R3 == \A s \in Writers, p \in Paths :
        (p \in w[s].unlinked /\ w[s].sStage \in {"saved", "mid"}) =>
          \/ w[s].local[p] = w[s].baseline[p]
          \/ /\ p \in w[s].sDrop \ w[s].sKeep /\ w[s].baseline[p] = Nil
             /\ w[s].local[p] # Nil /\ w[s].sHeld[p] = w[s].local[p]
\* Between the halves, every path the first half dropped is uncited.
R4 == \A s \in Writers : w[s].sStage = "mid" => \A p \in w[s].sDrop \ w[s].sKeep : w[s].baseline[p] = Nil
\* A reader's pull never spans the halves.
R5 == \A s \in Writers : w[s].pc = "pulling" => w[s].sStage \in {"none", "saved"}
Rescope == R1 /\ R2 /\ R3 /\ R4 /\ R5
M1 == Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope
IndM1 == IndTypeOK /\ M1
\* Prop_NarrowNeverDeletes's step formula.
NarrowOK == \A s \in Writers :
              (w[s].pc = "consumed" /\ w'[s].pc = "scanned") => w'[s].deletes \cap w[s].unlinked = {}

------------------------------------------------------------------------------
(* One lemma per conjunct: what a step's new tree R must satisfy.           *)

\* The step writes one tree: what every tree becomes.
LEMMA WriteAny ==
  ASSUME TypeOK, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R]
  PROVE  \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t]
BY DEF TypeOK

\* A step that writes no tree and keeps the holder keeps M1.
LEMMA M1Keep ==
  ASSUME M1, w' = w, holder' = holder
  PROVE  M1' /\ NarrowOK
<1>1. Inv_OneHolder' /\ Holder' BY DEF M1, Inv_OneHolder, Holder
<1>2. Cased' /\ Mine' /\ Ups' /\ Off' BY DEF M1, Cased, Mine, Ups, Off
<1>3. R1' /\ R2' /\ R3' /\ R4' /\ R5' BY DEF M1, Rescope, R1, R2, R3, R4, R5
<1>4. NarrowOK BY DEF NarrowOK
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF M1, Rescope

\* The holder's two conjuncts after a step that keeps the holder.
LEMMA HolderWrite ==
  ASSUME Inv_OneHolder, Holder, holder \in Writers \cup {"none"},
         NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         holder' = holder,
         (R.pc \in CC) <=> (w[s].pc \in CC),
         w[s].st = "on" => R.st = "on"
  PROVE  Inv_OneHolder' /\ Holder'
<1>1. Inv_OneHolder' BY DEF Inv_OneHolder, CC
<1>2. Holder' BY DEF Holder, CC
<1>. QED BY <1>1, <1>2

LEMMA CasedWrite ==
  ASSUME Cased, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.pc = "cased" => \A p \in R.deletes : R.inst[p] = Nil
  PROVE  Cased'
BY DEF Cased

LEMMA MineWrite ==
  ASSUME Mine, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.deletes \cap R.uploads = {} /\ R.gone \subseteq R.uploads
  PROVE  Mine'
BY DEF Mine

LEMMA UpsWrite ==
  ASSUME Ups, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.pc \in {"scanned", "claimed", "cased"} => (R.uploads \cup R.deletes) \cap R.unlinked = {}
  PROVE  Ups'
BY DEF Ups

LEMMA OffWrite ==
  ASSUME Off, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.st = "off" => R = WriterInit
  PROVE  Off'
BY DEF Off

\* Rescope after a step that writes R.
LEMMA RescopeWrite ==
  ASSUME Rescope, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.pc \in {"consumed", "scanned", "claimed", "cased"} => R.sStage = "none",
         \A p \in Paths : (p \in R.unlinked /\ R.sStage = "none") => R.local[p] = R.baseline[p],
         \A p \in Paths : (p \in R.unlinked /\ R.sStage \in {"saved", "mid"}) =>
           \/ R.local[p] = R.baseline[p]
           \/ /\ p \in R.sDrop \ R.sKeep /\ R.baseline[p] = Nil
              /\ R.local[p] # Nil /\ R.sHeld[p] = R.local[p],
         R.sStage = "mid" => \A p \in R.sDrop \ R.sKeep : R.baseline[p] = Nil,
         R.pc = "pulling" => R.sStage \in {"none", "saved"}
  PROVE  Rescope'
<1>1. R1' BY DEF Rescope, R1
<1>2. R2' BY DEF Rescope, R2
<1>3. R3' BY DEF Rescope, R3
<1>4. R4' BY DEF Rescope, R4
<1>5. R5' BY DEF Rescope, R5
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Rescope

\* Rescope after a step that keeps the rescope fields, except that the
\* unlinked set may shrink and the tree may change off it.
LEMMA RescopeShrink ==
  ASSUME Rescope, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         R.sStage = w[s].sStage, R.unlinked \subseteq w[s].unlinked,
         \A q \in R.unlinked : R.local[q] = w[s].local[q],
         R.baseline = w[s].baseline, R.sDrop = w[s].sDrop, R.sKeep = w[s].sKeep, R.sHeld = w[s].sHeld,
         R.pc \in {"consumed", "scanned", "claimed", "cased"} => R.sStage = "none",
         R.pc = "pulling" => R.sStage \in {"none", "saved"}
  PROVE  Rescope'
<1>1. \A p \in Paths : (p \in R.unlinked /\ R.sStage = "none") => R.local[p] = R.baseline[p]
  BY DEF Rescope, R2
<1>2. \A p \in Paths : (p \in R.unlinked /\ R.sStage \in {"saved", "mid"}) =>
        \/ R.local[p] = R.baseline[p]
        \/ /\ p \in R.sDrop \ R.sKeep /\ R.baseline[p] = Nil
           /\ R.local[p] # Nil /\ R.sHeld[p] = R.local[p]
  BY DEF Rescope, R3
<1>3. R.sStage = "mid" => \A p \in R.sDrop \ R.sKeep : R.baseline[p] = Nil BY DEF Rescope, R4
<1>. QED BY <1>1, <1>2, <1>3, RescopeWrite

\* Prop_NarrowNeverDeletes's step formula after a step that writes R.
LEMMA NarrowWrite ==
  ASSUME NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         (w[s].pc = "consumed" /\ R.pc = "scanned") => R.deletes \cap w[s].unlinked = {}
  PROVE  NarrowOK
BY DEF NarrowOK

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M1 == Init => IndM1
<1>. SUFFICES ASSUME Init PROVE IndM1 OBVIOUS
<1>1. IndTypeOK BY Init_TypeOK
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>2a. holder = "none" BY DEF Init
<1>3. \A s \in Writers : /\ w[s].st = "off" /\ w[s].pc = "idle"
                         /\ w[s].deletes = {} /\ w[s].uploads = {} /\ w[s].gone = {}
                         /\ w[s].unlinked = {} /\ w[s].sStage = "none"
  BY <1>2 DEF WriterInit
<1>4. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off
  BY <1>2, <1>2a, <1>3, NoneWriter DEF Inv_OneHolder, Holder, Cased, Mine, Ups, Off
<1>5. Rescope BY <1>3 DEF Rescope, R1, R2, R3, R4, R5
<1>. QED BY <1>1, <1>4, <1>5 DEF IndM1, M1

------------------------------------------------------------------------------
(* Steps that write no tree.                                                *)

LEMMA GPut_M1 ==
  ASSUME IndM1, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY GPut_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF GPut
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA GCas_M1 ==
  ASSUME IndM1, NEW p \in Paths, GCas(p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY GCas_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF GCas
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA GRename_M1 ==
  ASSUME IndM1, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY GRename_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF GRename
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

\* Never enabled (mv = Nil).
LEMMA GRenameFinish_M1 ==
  ASSUME IndM1, GRenameFinish, Frame
  PROVE  IndM1' /\ NarrowOK
BY DEF IndM1, IndTypeOK, GRenameFinish

LEMMA GDelete_M1 ==
  ASSUME IndM1, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY GDelete_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF GDelete
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA Sweep_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY Sweep_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF Sweep
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA Age_M1 ==
  ASSUME IndM1, Age, UNCHANGED anc
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY Age_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF Age
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA Reap_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY Reap_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF Reap
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

LEMMA RLoad_M1 ==
  ASSUME IndM1, RLoad, UNCHANGED anc
  PROVE  IndM1' /\ NarrowOK
<1>1. IndTypeOK' BY RLoad_TypeOK DEF IndM1
<1>2. w' = w /\ holder' = holder BY DEF RLoad
<1>. QED BY <1>1, <1>2, M1Keep DEF IndM1

------------------------------------------------------------------------------
(* The agent.                                                               *)

LEMMA Edit_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Edit_TypeOK
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit
<1>h. holder' = holder BY DEF Edit
<1>g. On(s) BY DEF Edit
<1>2. \A t \in Writers : w'[t] = IF t = s THEN EditW(s, p) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ EditW(s, p).st = w[s].st
      /\ EditW(s, p).pc = w[s].pc
      /\ EditW(s, p).deletes = w[s].deletes
      /\ EditW(s, p).inst = w[s].inst
      /\ EditW(s, p).uploads = w[s].uploads
      /\ EditW(s, p).gone = w[s].gone
      /\ EditW(s, p).unlinked = w[s].unlinked \ {p}
      /\ EditW(s, p).sStage = w[s].sStage
      /\ EditW(s, p).local = [w[s].local EXCEPT ![p] = <<p, nextGen>>]
      /\ EditW(s, p).baseline = w[s].baseline
      /\ EditW(s, p).sDrop = w[s].sDrop
      /\ EditW(s, p).sKeep = w[s].sKeep
      /\ EditW(s, p).sHeld = w[s].sHeld
  BY DEF EditW
<1>4a. (EditW(s, p).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => EditW(s, p).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. EditW(s, p).pc = "cased" => \A q \in EditW(s, p).deletes : EditW(s, p).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. EditW(s, p).deletes \cap EditW(s, p).uploads = {} /\ EditW(s, p).gone \subseteq EditW(s, p).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. EditW(s, p).pc \in {"scanned", "claimed", "cased"} => (EditW(s, p).uploads \cup EditW(s, p).deletes) \cap EditW(s, p).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. EditW(s, p).st = "off" => EditW(s, p) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ EditW(s, p).sStage = w[s].sStage /\ EditW(s, p).unlinked \subseteq w[s].unlinked
       /\ \A q \in EditW(s, p).unlinked : EditW(s, p).local[q] = w[s].local[q]
       /\ EditW(s, p).baseline = w[s].baseline /\ EditW(s, p).sDrop = w[s].sDrop /\ EditW(s, p).sKeep = w[s].sKeep /\ EditW(s, p).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (EditW(s, p).pc \in {"consumed", "scanned", "claimed", "cased"} => EditW(s, p).sStage = "none")
       /\ (EditW(s, p).pc = "pulling" => EditW(s, p).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ EditW(s, p).pc = "scanned") => EditW(s, p).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Delete_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Delete_TypeOK
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] BY DEF Delete
<1>h. holder' = holder BY DEF Delete, bucket
<1>g. On(s) BY DEF Delete
<1>2. \A t \in Writers : w'[t] = IF t = s THEN DeleteW(s, p) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ DeleteW(s, p).st = w[s].st
      /\ DeleteW(s, p).pc = w[s].pc
      /\ DeleteW(s, p).deletes = w[s].deletes
      /\ DeleteW(s, p).inst = w[s].inst
      /\ DeleteW(s, p).uploads = w[s].uploads
      /\ DeleteW(s, p).gone = w[s].gone
      /\ DeleteW(s, p).unlinked = w[s].unlinked \ {p}
      /\ DeleteW(s, p).sStage = w[s].sStage
      /\ DeleteW(s, p).local = [w[s].local EXCEPT ![p] = Nil]
      /\ DeleteW(s, p).baseline = w[s].baseline
      /\ DeleteW(s, p).sDrop = w[s].sDrop
      /\ DeleteW(s, p).sKeep = w[s].sKeep
      /\ DeleteW(s, p).sHeld = w[s].sHeld
  BY DEF DeleteW
<1>4a. (DeleteW(s, p).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => DeleteW(s, p).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. DeleteW(s, p).pc = "cased" => \A q \in DeleteW(s, p).deletes : DeleteW(s, p).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. DeleteW(s, p).deletes \cap DeleteW(s, p).uploads = {} /\ DeleteW(s, p).gone \subseteq DeleteW(s, p).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. DeleteW(s, p).pc \in {"scanned", "claimed", "cased"} => (DeleteW(s, p).uploads \cup DeleteW(s, p).deletes) \cap DeleteW(s, p).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. DeleteW(s, p).st = "off" => DeleteW(s, p) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ DeleteW(s, p).sStage = w[s].sStage /\ DeleteW(s, p).unlinked \subseteq w[s].unlinked
       /\ \A q \in DeleteW(s, p).unlinked : DeleteW(s, p).local[q] = w[s].local[q]
       /\ DeleteW(s, p).baseline = w[s].baseline /\ DeleteW(s, p).sDrop = w[s].sDrop /\ DeleteW(s, p).sKeep = w[s].sKeep /\ DeleteW(s, p).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (DeleteW(s, p).pc \in {"consumed", "scanned", "claimed", "cased"} => DeleteW(s, p).sStage = "none")
       /\ (DeleteW(s, p).pc = "pulling" => DeleteW(s, p).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ DeleteW(s, p).pc = "scanned") => DeleteW(s, p).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Checkout_M1 ==
  ASSUME IndM1, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Checkout_TypeOK
<1>i. w[s] = WriterInit BY <1>m DEF Off, Checkout
<1>j. /\ w[s].pc = "idle" /\ w[s].unlinked = {} /\ w[s].sStage = "none"
      /\ w[s].deletes = {} /\ w[s].uploads = {} /\ w[s].gone = {} /\ w[s].sDrop = {}
  BY <1>i DEF WriterInit
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>h. holder' = holder BY DEF Checkout, bucket
<1>g. w[s].st = "off" BY DEF Checkout
<1>2. \A t \in Writers : w'[t] = IF t = s THEN CheckoutW(s, T) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ CheckoutW(s, T).st = "on"
      /\ CheckoutW(s, T).pc = w[s].pc
      /\ CheckoutW(s, T).deletes = w[s].deletes
      /\ CheckoutW(s, T).inst = doc
      /\ CheckoutW(s, T).uploads = w[s].uploads
      /\ CheckoutW(s, T).gone = w[s].gone
      /\ CheckoutW(s, T).unlinked = w[s].unlinked
      /\ CheckoutW(s, T).sStage = w[s].sStage
      /\ CheckoutW(s, T).local = CheckoutHeld(s, T)
      /\ CheckoutW(s, T).baseline = CheckoutHeld(s, T)
      /\ CheckoutW(s, T).sDrop = w[s].sDrop
      /\ CheckoutW(s, T).sKeep = w[s].sKeep
      /\ CheckoutW(s, T).sHeld = w[s].sHeld
  BY DEF CheckoutW
<1>4a. (CheckoutW(s, T).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => CheckoutW(s, T).st = "on") BY <1>3
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. CheckoutW(s, T).pc = "cased" => \A q \in CheckoutW(s, T).deletes : CheckoutW(s, T).inst[q] = Nil BY <1>3, <1>j
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. CheckoutW(s, T).deletes \cap CheckoutW(s, T).uploads = {} /\ CheckoutW(s, T).gone \subseteq CheckoutW(s, T).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. CheckoutW(s, T).pc \in {"scanned", "claimed", "cased"} => (CheckoutW(s, T).uploads \cup CheckoutW(s, T).deletes) \cap CheckoutW(s, T).unlinked = {} BY <1>3, <1>j
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. CheckoutW(s, T).st = "off" => CheckoutW(s, T) = WriterInit BY <1>3
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope' BY <1>m, <1>2, <1>3, <1>j, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ CheckoutW(s, T).pc = "scanned") => CheckoutW(s, T).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Consume_M1 ==
  ASSUME IndM1, NEW s \in Writers, Consume(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Consume_TypeOK
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" BY DEF Consume
<1>h. holder' = holder BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s)] BY <1>1 DEF Consume
  <2>h. holder' = holder BY DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" BY DEF Consume
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN ConsumeCheapW(s) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ ConsumeCheapW(s).st = w[s].st
        /\ ConsumeCheapW(s).pc = "consumed"
        /\ ConsumeCheapW(s).deletes = w[s].deletes
        /\ ConsumeCheapW(s).inst = w[s].inst
        /\ ConsumeCheapW(s).uploads = w[s].uploads
        /\ ConsumeCheapW(s).gone = w[s].gone
        /\ ConsumeCheapW(s).unlinked = w[s].unlinked
        /\ ConsumeCheapW(s).sStage = w[s].sStage
        /\ ConsumeCheapW(s).local = w[s].local
        /\ ConsumeCheapW(s).baseline = w[s].baseline
        /\ ConsumeCheapW(s).sDrop = w[s].sDrop
        /\ ConsumeCheapW(s).sKeep = w[s].sKeep
        /\ ConsumeCheapW(s).sHeld = w[s].sHeld
    BY DEF ConsumeCheapW
  <2>4a. (ConsumeCheapW(s).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => ConsumeCheapW(s).st = "on") BY <2>3, <2>g DEF CC
  <2>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <2>2, <2>4a, <2>h, HolderWrite
  <2>5a. ConsumeCheapW(s).pc = "cased" => \A q \in ConsumeCheapW(s).deletes : ConsumeCheapW(s).inst[q] = Nil BY <2>3
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. ConsumeCheapW(s).deletes \cap ConsumeCheapW(s).uploads = {} /\ ConsumeCheapW(s).gone \subseteq ConsumeCheapW(s).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. ConsumeCheapW(s).pc \in {"scanned", "claimed", "cased"} => (ConsumeCheapW(s).uploads \cup ConsumeCheapW(s).deletes) \cap ConsumeCheapW(s).unlinked = {} BY <2>3
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. ConsumeCheapW(s).st = "off" => ConsumeCheapW(s) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. /\ ConsumeCheapW(s).sStage = w[s].sStage /\ ConsumeCheapW(s).unlinked \subseteq w[s].unlinked
         /\ \A q \in ConsumeCheapW(s).unlinked : ConsumeCheapW(s).local[q] = w[s].local[q]
         /\ ConsumeCheapW(s).baseline = w[s].baseline /\ ConsumeCheapW(s).sDrop = w[s].sDrop /\ ConsumeCheapW(s).sKeep = w[s].sKeep /\ ConsumeCheapW(s).sHeld = w[s].sHeld
    BY <2>3, <1>f
  <2>9b. /\ (ConsumeCheapW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => ConsumeCheapW(s).sStage = "none")
         /\ (ConsumeCheapW(s).pc = "pulling" => ConsumeCheapW(s).sStage \in {"none", "saved"})
    BY <2>3, <2>g, <1>r DEF R1, R5
  <2>9. Rescope' BY <1>m, <2>2, <2>9a, <2>9b, RescopeShrink
  <2>10a. (w[s].pc = "consumed" /\ ConsumeCheapW(s).pc = "scanned") => ConsumeCheapW(s).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>2. CASE ~CheapPath(s)
  <2>0. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail)] BY <1>2 DEF Consume
  <2>1. w' = [w EXCEPT ![s] = ConsumeW(s, fail)] BY <2>0
  <2>h. holder' = holder BY DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" BY DEF Consume
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN ConsumeW(s, fail) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ ConsumeW(s, fail).st = w[s].st
        /\ ConsumeW(s, fail).pc = "consumed"
        /\ ConsumeW(s, fail).deletes = w[s].deletes
        /\ ConsumeW(s, fail).inst = w[s].inst
        /\ ConsumeW(s, fail).uploads = w[s].uploads
        /\ ConsumeW(s, fail).gone = w[s].gone
        /\ ConsumeW(s, fail).unlinked = w[s].unlinked
        /\ ConsumeW(s, fail).sStage = w[s].sStage
        /\ ConsumeW(s, fail).local = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].local[q]]
        /\ ConsumeW(s, fail).baseline = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]]
        /\ ConsumeW(s, fail).sDrop = w[s].sDrop
        /\ ConsumeW(s, fail).sKeep = w[s].sKeep
        /\ ConsumeW(s, fail).sHeld = w[s].sHeld
    BY DEF ConsumeW
  <2>4a. (ConsumeW(s, fail).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => ConsumeW(s, fail).st = "on") BY <2>3, <2>g DEF CC
  <2>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <2>2, <2>4a, <2>h, HolderWrite
  <2>5a. ConsumeW(s, fail).pc = "cased" => \A q \in ConsumeW(s, fail).deletes : ConsumeW(s, fail).inst[q] = Nil BY <2>3
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. ConsumeW(s, fail).deletes \cap ConsumeW(s, fail).uploads = {} /\ ConsumeW(s, fail).gone \subseteq ConsumeW(s, fail).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. ConsumeW(s, fail).pc \in {"scanned", "claimed", "cased"} => (ConsumeW(s, fail).uploads \cup ConsumeW(s, fail).deletes) \cap ConsumeW(s, fail).unlinked = {} BY <2>3
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. ConsumeW(s, fail).st = "off" => ConsumeW(s, fail) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. \A q \in Paths : (q \in ConsumeW(s, fail).unlinked /\ ConsumeW(s, fail).sStage = "none")
                           => ConsumeW(s, fail).local[q] = ConsumeW(s, fail).baseline[q]
    BY <2>3, <1>r, <1>f DEF R2
  <2>9. Rescope' BY <1>m, <2>2, <2>3, <1>g, <2>9a, RescopeWrite
  <2>10a. (w[s].pc = "consumed" /\ ConsumeW(s, fail).pc = "scanned") => ConsumeW(s, fail).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>. QED BY <1>1, <1>2

LEMMA Scan_M1 ==
  ASSUME IndM1, NEW s \in Writers, Scan(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Scan_TypeOK
<1>k. w[s].sStage = "none" BY <1>r DEF R1, Scan
<1>l. \A q \in w[s].unlinked : w[s].local[q] = w[s].baseline[q] BY <1>r, <1>k, <1>f DEF R2
<1>1. PICK dels \in SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan
<1>h. holder' = holder BY DEF Scan, bucket
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Scan
<1>2. \A t \in Writers : w'[t] = IF t = s THEN ScanW(s, dels) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ ScanW(s, dels).st = w[s].st
      /\ ScanW(s, dels).pc = "scanned"
      /\ ScanW(s, dels).deletes = dels
      /\ ScanW(s, dels).inst = w[s].inst
      /\ ScanW(s, dels).uploads = ScanUps(s)
      /\ ScanW(s, dels).gone = {}
      /\ ScanW(s, dels).unlinked = w[s].unlinked
      /\ ScanW(s, dels).sStage = w[s].sStage
      /\ ScanW(s, dels).local = w[s].local
      /\ ScanW(s, dels).baseline = w[s].baseline
      /\ ScanW(s, dels).sDrop = w[s].sDrop
      /\ ScanW(s, dels).sKeep = w[s].sKeep
      /\ ScanW(s, dels).sHeld = w[s].sHeld
  BY DEF ScanW
<1>4a. (ScanW(s, dels).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => ScanW(s, dels).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. ScanW(s, dels).pc = "cased" => \A q \in ScanW(s, dels).deletes : ScanW(s, dels).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. ScanW(s, dels).deletes \cap ScanW(s, dels).uploads = {} /\ ScanW(s, dels).gone \subseteq ScanW(s, dels).uploads BY <1>1, <1>3 DEF ScanUps, ScanAbsent, ScanDirty
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. ScanW(s, dels).pc \in {"scanned", "claimed", "cased"} => (ScanW(s, dels).uploads \cup ScanW(s, dels).deletes) \cap ScanW(s, dels).unlinked = {} BY <1>1, <1>3, <1>l DEF ScanUps, ScanAbsent, ScanDirty
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. ScanW(s, dels).st = "off" => ScanW(s, dels) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ ScanW(s, dels).sStage = w[s].sStage /\ ScanW(s, dels).unlinked \subseteq w[s].unlinked
       /\ \A q \in ScanW(s, dels).unlinked : ScanW(s, dels).local[q] = w[s].local[q]
       /\ ScanW(s, dels).baseline = w[s].baseline /\ ScanW(s, dels).sDrop = w[s].sDrop
       /\ ScanW(s, dels).sKeep = w[s].sKeep /\ ScanW(s, dels).sHeld = w[s].sHeld
  BY <1>3
<1>9b. /\ (ScanW(s, dels).pc \in {"consumed", "scanned", "claimed", "cased"} => ScanW(s, dels).sStage = "none")
       /\ (ScanW(s, dels).pc = "pulling" => ScanW(s, dels).sStage \in {"none", "saved"})
  BY <1>3, <1>k
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ ScanW(s, dels).pc = "scanned") => ScanW(s, dels).deletes \cap w[s].unlinked = {} BY <1>1, <1>3, <1>l DEF ScanAbsent, ScanDirty
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Skip_M1 ==
  ASSUME IndM1, NEW s \in Writers, Skip(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Skip_TypeOK
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] BY DEF Skip
<1>h. holder' = holder BY DEF Skip, bucket
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Skip
<1>2. \A t \in Writers : w'[t] = IF t = s THEN SkipW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ SkipW(s).st = w[s].st
      /\ SkipW(s).pc = "idle"
      /\ SkipW(s).deletes = w[s].deletes
      /\ SkipW(s).inst = w[s].inst
      /\ SkipW(s).uploads = w[s].uploads
      /\ SkipW(s).gone = w[s].gone
      /\ SkipW(s).unlinked = w[s].unlinked
      /\ SkipW(s).sStage = w[s].sStage
      /\ SkipW(s).local = w[s].local
      /\ SkipW(s).baseline = w[s].baseline
      /\ SkipW(s).sDrop = w[s].sDrop
      /\ SkipW(s).sKeep = w[s].sKeep
      /\ SkipW(s).sHeld = w[s].sHeld
  BY DEF SkipW
<1>4a. (SkipW(s).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => SkipW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. SkipW(s).pc = "cased" => \A q \in SkipW(s).deletes : SkipW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. SkipW(s).deletes \cap SkipW(s).uploads = {} /\ SkipW(s).gone \subseteq SkipW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. SkipW(s).pc \in {"scanned", "claimed", "cased"} => (SkipW(s).uploads \cup SkipW(s).deletes) \cap SkipW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. SkipW(s).st = "off" => SkipW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ SkipW(s).sStage = w[s].sStage /\ SkipW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in SkipW(s).unlinked : SkipW(s).local[q] = w[s].local[q]
       /\ SkipW(s).baseline = w[s].baseline /\ SkipW(s).sDrop = w[s].sDrop /\ SkipW(s).sKeep = w[s].sKeep /\ SkipW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (SkipW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => SkipW(s).sStage = "none")
       /\ (SkipW(s).pc = "pulling" => SkipW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ SkipW(s).pc = "scanned") => SkipW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Upload_M1 ==
  ASSUME IndM1, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Upload_TypeOK
<1>g. On(s) /\ w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
<1>h. holder' = holder BY DEF Upload
<1>k. p \notin w[s].unlinked BY <1>m, <1>g DEF Ups
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] BY <1>1 DEF Upload
  <2>h. holder' = holder BY DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN UploadW(s, p) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ UploadW(s, p).st = w[s].st
        /\ UploadW(s, p).pc = w[s].pc
        /\ UploadW(s, p).deletes = w[s].deletes
        /\ UploadW(s, p).inst = w[s].inst
        /\ UploadW(s, p).uploads = w[s].uploads
        /\ UploadW(s, p).gone = w[s].gone
        /\ UploadW(s, p).unlinked = w[s].unlinked
        /\ UploadW(s, p).sStage = w[s].sStage
        /\ UploadW(s, p).local = w[s].local
        /\ UploadW(s, p).baseline = w[s].baseline
        /\ UploadW(s, p).sDrop = w[s].sDrop
        /\ UploadW(s, p).sKeep = w[s].sKeep
        /\ UploadW(s, p).sHeld = w[s].sHeld
    BY DEF UploadW
  <2>4a. (UploadW(s, p).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => UploadW(s, p).st = "on") BY <2>3, <2>g DEF CC
  <2>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <2>2, <2>4a, <2>h, HolderWrite
  <2>5a. UploadW(s, p).pc = "cased" => \A q \in UploadW(s, p).deletes : UploadW(s, p).inst[q] = Nil BY <1>m, <2>3 DEF Cased
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. UploadW(s, p).deletes \cap UploadW(s, p).uploads = {} /\ UploadW(s, p).gone \subseteq UploadW(s, p).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. UploadW(s, p).pc \in {"scanned", "claimed", "cased"} => (UploadW(s, p).uploads \cup UploadW(s, p).deletes) \cap UploadW(s, p).unlinked = {} BY <1>m, <2>3 DEF Ups
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. UploadW(s, p).st = "off" => UploadW(s, p) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. /\ UploadW(s, p).sStage = w[s].sStage /\ UploadW(s, p).unlinked \subseteq w[s].unlinked
         /\ \A q \in UploadW(s, p).unlinked : UploadW(s, p).local[q] = w[s].local[q]
         /\ UploadW(s, p).baseline = w[s].baseline /\ UploadW(s, p).sDrop = w[s].sDrop /\ UploadW(s, p).sKeep = w[s].sKeep /\ UploadW(s, p).sHeld = w[s].sHeld
    BY <2>3, <1>f
  <2>9b. /\ (UploadW(s, p).pc \in {"consumed", "scanned", "claimed", "cased"} => UploadW(s, p).sStage = "none")
         /\ (UploadW(s, p).pc = "pulling" => UploadW(s, p).sStage \in {"none", "saved"})
    BY <2>3, <2>g, <1>r DEF R1, R5
  <2>9. Rescope' BY <1>m, <2>2, <2>9a, <2>9b, RescopeShrink
  <2>10a. (w[s].pc = "consumed" /\ UploadW(s, p).pc = "scanned") => UploadW(s, p).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] BY <1>2 DEF Upload
  <2>h. holder' = holder BY DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
  <2>2. \A t \in Writers : w'[t] = IF t = s THEN UploadCopyW(s, p, c) ELSE w[t] BY <1>b, <2>1, WriteAny
  <2>3. /\ UploadCopyW(s, p, c).st = w[s].st
        /\ UploadCopyW(s, p, c).pc = w[s].pc
        /\ UploadCopyW(s, p, c).deletes = w[s].deletes
        /\ UploadCopyW(s, p, c).inst = w[s].inst
        /\ UploadCopyW(s, p, c).uploads = w[s].uploads
        /\ UploadCopyW(s, p, c).gone = w[s].gone
        /\ UploadCopyW(s, p, c).unlinked = w[s].unlinked
        /\ UploadCopyW(s, p, c).sStage = w[s].sStage
        /\ UploadCopyW(s, p, c).local = [w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]]
        /\ UploadCopyW(s, p, c).baseline = w[s].baseline
        /\ UploadCopyW(s, p, c).sDrop = w[s].sDrop
        /\ UploadCopyW(s, p, c).sKeep = w[s].sKeep
        /\ UploadCopyW(s, p, c).sHeld = w[s].sHeld
    BY DEF UploadCopyW
  <2>4a. (UploadCopyW(s, p, c).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => UploadCopyW(s, p, c).st = "on") BY <2>3, <2>g DEF CC
  <2>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <2>2, <2>4a, <2>h, HolderWrite
  <2>5a. UploadCopyW(s, p, c).pc = "cased" => \A q \in UploadCopyW(s, p, c).deletes : UploadCopyW(s, p, c).inst[q] = Nil BY <1>m, <2>3 DEF Cased
  <2>5. Cased' BY <1>m, <2>2, <2>5a, CasedWrite
  <2>6a. UploadCopyW(s, p, c).deletes \cap UploadCopyW(s, p, c).uploads = {} /\ UploadCopyW(s, p, c).gone \subseteq UploadCopyW(s, p, c).uploads BY <1>m, <2>3 DEF Mine
  <2>6. Mine' BY <1>m, <2>2, <2>6a, MineWrite
  <2>7a. UploadCopyW(s, p, c).pc \in {"scanned", "claimed", "cased"} => (UploadCopyW(s, p, c).uploads \cup UploadCopyW(s, p, c).deletes) \cap UploadCopyW(s, p, c).unlinked = {} BY <1>m, <2>3 DEF Ups
  <2>7. Ups' BY <1>m, <2>2, <2>7a, UpsWrite
  <2>8a. UploadCopyW(s, p, c).st = "off" => UploadCopyW(s, p, c) = WriterInit BY <2>3, <2>g DEF On
  <2>8. Off' BY <1>m, <2>2, <2>8a, OffWrite
  <2>9a. /\ UploadCopyW(s, p, c).sStage = w[s].sStage /\ UploadCopyW(s, p, c).unlinked \subseteq w[s].unlinked
         /\ \A q \in UploadCopyW(s, p, c).unlinked : UploadCopyW(s, p, c).local[q] = w[s].local[q]
         /\ UploadCopyW(s, p, c).baseline = w[s].baseline /\ UploadCopyW(s, p, c).sDrop = w[s].sDrop
         /\ UploadCopyW(s, p, c).sKeep = w[s].sKeep /\ UploadCopyW(s, p, c).sHeld = w[s].sHeld
    BY <2>3, <1>f, <1>k
  <2>9b. /\ (UploadCopyW(s, p, c).pc \in {"consumed", "scanned", "claimed", "cased"} => UploadCopyW(s, p, c).sStage = "none")
         /\ (UploadCopyW(s, p, c).pc = "pulling" => UploadCopyW(s, p, c).sStage \in {"none", "saved"})
    BY <2>3, <1>g, <1>r DEF R1, R5
  <2>9. Rescope' BY <1>m, <2>2, <2>9a, <2>9b, RescopeShrink
  <2>10a. (w[s].pc = "consumed" /\ UploadCopyW(s, p, c).pc = "scanned") => UploadCopyW(s, p, c).deletes \cap w[s].unlinked = {} BY <2>3
  <2>10. NarrowOK BY <2>2, <2>10a, NarrowWrite
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10 DEF IndM1, M1
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* The commit section.                                                      *)

LEMMA PullOnly_M1 ==
  ASSUME IndM1, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, PullOnly_TypeOK
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] BY DEF PullOnly
<1>h. holder' = holder BY DEF PullOnly, bucket
<1>g. On(s) /\ w[s].pc = "scanned" BY DEF PullOnly
<1>2. \A t \in Writers : w'[t] = IF t = s THEN PullOnlyW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ PullOnlyW(s).st = w[s].st
      /\ PullOnlyW(s).pc = "idle"
      /\ PullOnlyW(s).deletes = w[s].deletes
      /\ PullOnlyW(s).inst = doc
      /\ PullOnlyW(s).uploads = w[s].uploads
      /\ PullOnlyW(s).gone = {}
      /\ PullOnlyW(s).unlinked = w[s].unlinked
      /\ PullOnlyW(s).sStage = w[s].sStage
      /\ PullOnlyW(s).local = w[s].local
      /\ PullOnlyW(s).baseline = w[s].baseline
      /\ PullOnlyW(s).sDrop = w[s].sDrop
      /\ PullOnlyW(s).sKeep = w[s].sKeep
      /\ PullOnlyW(s).sHeld = w[s].sHeld
  BY DEF PullOnlyW
<1>4a. (PullOnlyW(s).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => PullOnlyW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. PullOnlyW(s).pc = "cased" => \A q \in PullOnlyW(s).deletes : PullOnlyW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. PullOnlyW(s).deletes \cap PullOnlyW(s).uploads = {} /\ PullOnlyW(s).gone \subseteq PullOnlyW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. PullOnlyW(s).pc \in {"scanned", "claimed", "cased"} => (PullOnlyW(s).uploads \cup PullOnlyW(s).deletes) \cap PullOnlyW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. PullOnlyW(s).st = "off" => PullOnlyW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ PullOnlyW(s).sStage = w[s].sStage /\ PullOnlyW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in PullOnlyW(s).unlinked : PullOnlyW(s).local[q] = w[s].local[q]
       /\ PullOnlyW(s).baseline = w[s].baseline /\ PullOnlyW(s).sDrop = w[s].sDrop /\ PullOnlyW(s).sKeep = w[s].sKeep /\ PullOnlyW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (PullOnlyW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => PullOnlyW(s).sStage = "none")
       /\ (PullOnlyW(s).pc = "pulling" => PullOnlyW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ PullOnlyW(s).pc = "scanned") => PullOnlyW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Claim_M1 ==
  ASSUME IndM1, NEW s \in Writers, Claim(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Claim_TypeOK
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] BY DEF Claim
<1>h. holder' = s BY DEF Claim
<1>g. On(s) /\ w[s].pc = "scanned" /\ holder = "none" BY DEF Claim
<1>2. \A t \in Writers : w'[t] = IF t = s THEN ClaimW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ ClaimW(s).st = w[s].st
      /\ ClaimW(s).pc = "claimed"
      /\ ClaimW(s).deletes = w[s].deletes
      /\ ClaimW(s).inst = w[s].inst
      /\ ClaimW(s).uploads = w[s].uploads
      /\ ClaimW(s).gone = w[s].gone
      /\ ClaimW(s).unlinked = w[s].unlinked
      /\ ClaimW(s).sStage = w[s].sStage
      /\ ClaimW(s).local = w[s].local
      /\ ClaimW(s).baseline = w[s].baseline
      /\ ClaimW(s).sDrop = w[s].sDrop
      /\ ClaimW(s).sKeep = w[s].sKeep
      /\ ClaimW(s).sHeld = w[s].sHeld
  BY DEF ClaimW
<1>4. Inv_OneHolder' /\ Holder'
  <2>1. Inv_OneHolder' BY <1>m, <1>0, <1>2, <1>3, <1>h, <1>g, NoneWriter DEF Inv_OneHolder
  <2>2. Holder' BY <1>2, <1>3, <1>h, <1>g, NoneWriter DEF Holder, CC, On
  <2>. QED BY <2>1, <2>2
<1>5a. ClaimW(s).pc = "cased" => \A q \in ClaimW(s).deletes : ClaimW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. ClaimW(s).deletes \cap ClaimW(s).uploads = {} /\ ClaimW(s).gone \subseteq ClaimW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. ClaimW(s).pc \in {"scanned", "claimed", "cased"} => (ClaimW(s).uploads \cup ClaimW(s).deletes) \cap ClaimW(s).unlinked = {} BY <1>m, <1>3, <1>g DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. ClaimW(s).st = "off" => ClaimW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ ClaimW(s).sStage = w[s].sStage /\ ClaimW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in ClaimW(s).unlinked : ClaimW(s).local[q] = w[s].local[q]
       /\ ClaimW(s).baseline = w[s].baseline /\ ClaimW(s).sDrop = w[s].sDrop /\ ClaimW(s).sKeep = w[s].sKeep /\ ClaimW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (ClaimW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => ClaimW(s).sStage = "none")
       /\ (ClaimW(s).pc = "pulling" => ClaimW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ ClaimW(s).pc = "scanned") => ClaimW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Verify_M1 ==
  ASSUME IndM1, NEW s \in Writers, Verify(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Verify_TypeOK
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] BY DEF Verify
<1>h. holder' = holder BY DEF Verify, bucket
<1>g. On(s) /\ w[s].pc = "claimed" BY DEF Verify
<1>2. \A t \in Writers : w'[t] = IF t = s THEN VerifyW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ VerifyW(s).st = w[s].st
      /\ VerifyW(s).pc = w[s].pc
      /\ VerifyW(s).deletes = w[s].deletes
      /\ VerifyW(s).inst = w[s].inst
      /\ VerifyW(s).uploads = w[s].uploads
      /\ VerifyW(s).gone = IF CommitVerifiesUploads THEN {q \in w[s].uploads \cap w[s].upDone : w[s].snap[q] \notin live} ELSE {}
      /\ VerifyW(s).unlinked = w[s].unlinked
      /\ VerifyW(s).sStage = w[s].sStage
      /\ VerifyW(s).local = w[s].local
      /\ VerifyW(s).baseline = w[s].baseline
      /\ VerifyW(s).sDrop = w[s].sDrop
      /\ VerifyW(s).sKeep = w[s].sKeep
      /\ VerifyW(s).sHeld = w[s].sHeld
  BY DEF VerifyW
<1>4a. (VerifyW(s).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => VerifyW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. VerifyW(s).pc = "cased" => \A q \in VerifyW(s).deletes : VerifyW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. VerifyW(s).deletes \cap VerifyW(s).uploads = {} /\ VerifyW(s).gone \subseteq VerifyW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. VerifyW(s).pc \in {"scanned", "claimed", "cased"} => (VerifyW(s).uploads \cup VerifyW(s).deletes) \cap VerifyW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. VerifyW(s).st = "off" => VerifyW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ VerifyW(s).sStage = w[s].sStage /\ VerifyW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in VerifyW(s).unlinked : VerifyW(s).local[q] = w[s].local[q]
       /\ VerifyW(s).baseline = w[s].baseline /\ VerifyW(s).sDrop = w[s].sDrop /\ VerifyW(s).sKeep = w[s].sKeep /\ VerifyW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (VerifyW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => VerifyW(s).sStage = "none")
       /\ (VerifyW(s).pc = "pulling" => VerifyW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ VerifyW(s).pc = "scanned") => VerifyW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Install_M1 ==
  ASSUME IndM1, NEW s \in Writers, Install(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Install_TypeOK
<1>i. \A q \in w[s].deletes : InstallInst(s)[q] = Nil
  <2>1. DeleteWinsPreserved BY ShippedShape DEF Shipped
  <2>2. \A q \in w[s].deletes : q \notin w[s].gone /\ q \notin InstallMine(s) BY <1>m DEF Mine, InstallMine
  <2>. QED BY <2>1, <2>2, <1>f DEF InstallInst
<1>1. w' = [w EXCEPT ![s] = InstallW(s)] BY DEF Install
<1>h. holder' = holder BY DEF Install
<1>g. On(s) /\ w[s].pc = "claimed" /\ holder = s BY DEF Install
<1>2. \A t \in Writers : w'[t] = IF t = s THEN InstallW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ InstallW(s).st = w[s].st
      /\ InstallW(s).pc = "cased"
      /\ InstallW(s).deletes = w[s].deletes
      /\ InstallW(s).inst = InstallInst(s)
      /\ InstallW(s).uploads = w[s].uploads
      /\ InstallW(s).gone = w[s].gone
      /\ InstallW(s).unlinked = w[s].unlinked
      /\ InstallW(s).sStage = w[s].sStage
      /\ InstallW(s).local = w[s].local
      /\ InstallW(s).baseline = w[s].baseline
      /\ InstallW(s).sDrop = w[s].sDrop
      /\ InstallW(s).sKeep = w[s].sKeep
      /\ InstallW(s).sHeld = w[s].sHeld
  BY DEF InstallW
<1>4a. (InstallW(s).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => InstallW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. InstallW(s).pc = "cased" => \A q \in InstallW(s).deletes : InstallW(s).inst[q] = Nil BY <1>3, <1>i
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. InstallW(s).deletes \cap InstallW(s).uploads = {} /\ InstallW(s).gone \subseteq InstallW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. InstallW(s).pc \in {"scanned", "claimed", "cased"} => (InstallW(s).uploads \cup InstallW(s).deletes) \cap InstallW(s).unlinked = {} BY <1>m, <1>3, <1>g DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. InstallW(s).st = "off" => InstallW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ InstallW(s).sStage = w[s].sStage /\ InstallW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in InstallW(s).unlinked : InstallW(s).local[q] = w[s].local[q]
       /\ InstallW(s).baseline = w[s].baseline /\ InstallW(s).sDrop = w[s].sDrop /\ InstallW(s).sKeep = w[s].sKeep /\ InstallW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (InstallW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => InstallW(s).sStage = "none")
       /\ (InstallW(s).pc = "pulling" => InstallW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ InstallW(s).pc = "scanned") => InstallW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Collect_M1 ==
  ASSUME IndM1, NEW s \in Writers, Collect(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Collect_TypeOK
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] BY DEF Collect
<1>h. holder' = holder BY DEF Collect
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Collect
<1>2. \A t \in Writers : w'[t] = IF t = s THEN CollectW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ CollectW(s).st = w[s].st
      /\ CollectW(s).pc = w[s].pc
      /\ CollectW(s).deletes = w[s].deletes
      /\ CollectW(s).inst = w[s].inst
      /\ CollectW(s).uploads = w[s].uploads
      /\ CollectW(s).gone = w[s].gone
      /\ CollectW(s).unlinked = w[s].unlinked
      /\ CollectW(s).sStage = w[s].sStage
      /\ CollectW(s).local = w[s].local
      /\ CollectW(s).baseline = w[s].baseline
      /\ CollectW(s).sDrop = w[s].sDrop
      /\ CollectW(s).sKeep = w[s].sKeep
      /\ CollectW(s).sHeld = w[s].sHeld
  BY DEF CollectW
<1>4a. (CollectW(s).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => CollectW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. CollectW(s).pc = "cased" => \A q \in CollectW(s).deletes : CollectW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. CollectW(s).deletes \cap CollectW(s).uploads = {} /\ CollectW(s).gone \subseteq CollectW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. CollectW(s).pc \in {"scanned", "claimed", "cased"} => (CollectW(s).uploads \cup CollectW(s).deletes) \cap CollectW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. CollectW(s).st = "off" => CollectW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ CollectW(s).sStage = w[s].sStage /\ CollectW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in CollectW(s).unlinked : CollectW(s).local[q] = w[s].local[q]
       /\ CollectW(s).baseline = w[s].baseline /\ CollectW(s).sDrop = w[s].sDrop /\ CollectW(s).sKeep = w[s].sKeep /\ CollectW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (CollectW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => CollectW(s).sStage = "none")
       /\ (CollectW(s).pc = "pulling" => CollectW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ CollectW(s).pc = "scanned") => CollectW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Finish_M1 ==
  ASSUME IndM1, NEW s \in Writers, Finish(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Finish_TypeOK
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish
<1>h. holder' = "none" BY DEF Finish
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Finish
<1>2. \A t \in Writers : w'[t] = IF t = s THEN FinishW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ FinishW(s).st = w[s].st
      /\ FinishW(s).pc = "idle"
      /\ FinishW(s).deletes = {}
      /\ FinishW(s).inst = w[s].inst
      /\ FinishW(s).uploads = {}
      /\ FinishW(s).gone = {}
      /\ FinishW(s).unlinked = w[s].unlinked
      /\ FinishW(s).sStage = w[s].sStage
      /\ FinishW(s).local = w[s].local
      /\ FinishW(s).baseline = [q \in Paths |-> IF q \in w[s].uploads \cap w[s].upDone THEN w[s].snap[q]
                                        ELSE IF q \in w[s].deletes /\ w[s].inst[q] = Nil THEN Nil
                                        ELSE w[s].baseline[q]]
      /\ FinishW(s).sDrop = w[s].sDrop
      /\ FinishW(s).sKeep = w[s].sKeep
      /\ FinishW(s).sHeld = w[s].sHeld
  BY DEF FinishW
<1>4. Inv_OneHolder' /\ Holder'
  <2>1. Inv_OneHolder' BY <1>m, <1>2, <1>3, <1>g DEF Inv_OneHolder
  <2>2. Holder' BY <1>h, NoneWriter DEF Holder
  <2>. QED BY <2>1, <2>2
<1>5a. FinishW(s).pc = "cased" => \A q \in FinishW(s).deletes : FinishW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. FinishW(s).deletes \cap FinishW(s).uploads = {} /\ FinishW(s).gone \subseteq FinishW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. FinishW(s).pc \in {"scanned", "claimed", "cased"} => (FinishW(s).uploads \cup FinishW(s).deletes) \cap FinishW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. FinishW(s).st = "off" => FinishW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>1. w[s].sStage = "none" BY <1>r, <1>g DEF R1
  <2>2. \A q \in w[s].unlinked : q \notin w[s].uploads /\ q \notin w[s].deletes BY <1>m, <1>g DEF Ups
  <2>3. \A q \in Paths : (q \in FinishW(s).unlinked /\ FinishW(s).sStage = "none") => FinishW(s).local[q] = FinishW(s).baseline[q]
    BY <1>3, <1>r, <1>f, <2>1, <2>2 DEF R2
  <2>. QED BY <1>m, <1>2, <1>3, <2>1, <2>3, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ FinishW(s).pc = "scanned") => FinishW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

------------------------------------------------------------------------------
(* The restart and the sync.                                                *)

LEMMA Restart_M1 ==
  ASSUME IndM1, NEW s \in Writers, Restart(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Restart_TypeOK
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart
<1>h. holder' = IF holder = s THEN "none" ELSE holder BY DEF Restart
<1>g. On(s) BY DEF Restart
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RestartW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RestartW(s).st = w[s].st
      /\ RestartW(s).pc = "idle"
      /\ RestartW(s).deletes = {}
      /\ RestartW(s).inst = [q \in Paths |-> Nil]
      /\ RestartW(s).uploads = {}
      /\ RestartW(s).gone = {}
      /\ RestartW(s).unlinked = w[s].unlinked
      /\ RestartW(s).sStage = IF w[s].sStage = "none" THEN "none" ELSE "saved"
      /\ RestartW(s).local = w[s].local
      /\ RestartW(s).baseline = w[s].baseline
      /\ RestartW(s).sDrop = w[s].sDrop
      /\ RestartW(s).sKeep = {}
      /\ RestartW(s).sHeld = w[s].sHeld
  BY DEF RestartW
<1>4. Inv_OneHolder' /\ Holder'
  <2>1. Inv_OneHolder' BY <1>m, <1>0, <1>2, <1>3, <1>h DEF Inv_OneHolder
  <2>2. Holder' BY <1>m, <1>0, <1>2, <1>3, <1>h DEF Holder
  <2>. QED BY <2>1, <2>2
<1>5a. RestartW(s).pc = "cased" => \A q \in RestartW(s).deletes : RestartW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RestartW(s).deletes \cap RestartW(s).uploads = {} /\ RestartW(s).gone \subseteq RestartW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RestartW(s).pc \in {"scanned", "claimed", "cased"} => (RestartW(s).uploads \cup RestartW(s).deletes) \cap RestartW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RestartW(s).st = "off" => RestartW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>1. \A q \in Paths : (q \in RestartW(s).unlinked /\ RestartW(s).sStage = "none") => RestartW(s).local[q] = RestartW(s).baseline[q]
    BY <1>3, <1>r DEF R2
  <2>2. \A q \in Paths : (q \in RestartW(s).unlinked /\ RestartW(s).sStage \in {"saved", "mid"}) =>
          \/ RestartW(s).local[q] = RestartW(s).baseline[q]
          \/ /\ q \in RestartW(s).sDrop \ RestartW(s).sKeep /\ RestartW(s).baseline[q] = Nil
             /\ RestartW(s).local[q] # Nil /\ RestartW(s).sHeld[q] = RestartW(s).local[q]
    BY <1>3, <1>r, <1>f DEF R3
  <2>3. RestartW(s).sStage = "mid" => \A q \in RestartW(s).sDrop \ RestartW(s).sKeep : RestartW(s).baseline[q] = Nil BY <1>3
  <2>. QED BY <1>m, <1>2, <1>3, <2>1, <2>2, <2>3, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RestartW(s).pc = "scanned") => RestartW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA Sync_M1 ==
  ASSUME IndM1, NEW s \in Writers, Sync(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, Sync_TypeOK
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>h. holder' = holder BY DEF Sync, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage \in {"none", "saved"} BY DEF Sync
<1>2. \A t \in Writers : w'[t] = IF t = s THEN SyncW(s, fail) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ SyncW(s, fail).st = w[s].st
      /\ SyncW(s, fail).pc = w[s].pc
      /\ SyncW(s, fail).deletes = w[s].deletes
      /\ SyncW(s, fail).inst = w[s].inst
      /\ SyncW(s, fail).uploads = w[s].uploads
      /\ SyncW(s, fail).gone = w[s].gone
      /\ SyncW(s, fail).unlinked = w[s].unlinked
      /\ SyncW(s, fail).sStage = w[s].sStage
      /\ SyncW(s, fail).local = [q \in Paths |-> IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]
      /\ SyncW(s, fail).baseline = SyncBl(s, fail)
      /\ SyncW(s, fail).sDrop = w[s].sDrop
      /\ SyncW(s, fail).sKeep = w[s].sKeep
      /\ SyncW(s, fail).sHeld = w[s].sHeld
  BY DEF SyncW
<1>4a. (SyncW(s, fail).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => SyncW(s, fail).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. SyncW(s, fail).pc = "cased" => \A q \in SyncW(s, fail).deletes : SyncW(s, fail).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. SyncW(s, fail).deletes \cap SyncW(s, fail).uploads = {} /\ SyncW(s, fail).gone \subseteq SyncW(s, fail).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. SyncW(s, fail).pc \in {"scanned", "claimed", "cased"} => (SyncW(s, fail).uploads \cup SyncW(s, fail).deletes) \cap SyncW(s, fail).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. SyncW(s, fail).st = "off" => SyncW(s, fail) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>0. \A q \in Paths : q \in SyncOwed(s, fail) => w[s].local[q] = w[s].baseline[q] BY DEF SyncOwed, SyncAll, Owed
  <2>1. \A q \in Paths : /\ (w[s].local[q] = w[s].baseline[q] => SyncW(s, fail).local[q] = SyncW(s, fail).baseline[q])
                        /\ (q \notin SyncOwed(s, fail) => SyncW(s, fail).local[q] = w[s].local[q] /\ SyncW(s, fail).baseline[q] = w[s].baseline[q])
    BY <1>3, <2>0 DEF SyncBl
  <2>2. \A q \in Paths : (q \in SyncW(s, fail).unlinked /\ SyncW(s, fail).sStage = "none") => SyncW(s, fail).local[q] = SyncW(s, fail).baseline[q]
    BY <1>3, <1>r, <2>1 DEF R2
  <2>3. \A q \in Paths : (q \in SyncW(s, fail).unlinked /\ SyncW(s, fail).sStage \in {"saved", "mid"}) =>
          \/ SyncW(s, fail).local[q] = SyncW(s, fail).baseline[q]
          \/ /\ q \in SyncW(s, fail).sDrop \ SyncW(s, fail).sKeep /\ SyncW(s, fail).baseline[q] = Nil
             /\ SyncW(s, fail).local[q] # Nil /\ SyncW(s, fail).sHeld[q] = SyncW(s, fail).local[q]
    BY <1>3, <1>r, <2>0, <2>1 DEF R3
  <2>4. SyncW(s, fail).sStage = "mid" => \A q \in SyncW(s, fail).sDrop \ SyncW(s, fail).sKeep : SyncW(s, fail).baseline[q] = Nil BY <1>3, <1>g
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, <2>3, <2>4, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ SyncW(s, fail).pc = "scanned") => SyncW(s, fail).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

------------------------------------------------------------------------------
(* The narrow / widen verb.                                                 *)

LEMMA RescopeBegin_M1 ==
  ASSUME IndM1, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RescopeBegin_TypeOK
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>h. holder' = holder BY DEF RescopeBegin, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "none" BY DEF RescopeBegin
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RescopeBeginW(s, T) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RescopeBeginW(s, T).st = w[s].st
      /\ RescopeBeginW(s, T).pc = w[s].pc
      /\ RescopeBeginW(s, T).deletes = w[s].deletes
      /\ RescopeBeginW(s, T).inst = w[s].inst
      /\ RescopeBeginW(s, T).uploads = w[s].uploads
      /\ RescopeBeginW(s, T).gone = w[s].gone
      /\ RescopeBeginW(s, T).unlinked = w[s].unlinked
      /\ RescopeBeginW(s, T).sStage = "saved"
      /\ RescopeBeginW(s, T).local = w[s].local
      /\ RescopeBeginW(s, T).baseline = w[s].baseline
      /\ RescopeBeginW(s, T).sDrop = Leaving(s, T)
      /\ RescopeBeginW(s, T).sKeep = {}
      /\ RescopeBeginW(s, T).sHeld = [q \in Paths |-> Nil]
  BY DEF RescopeBeginW
<1>4a. (RescopeBeginW(s, T).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => RescopeBeginW(s, T).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RescopeBeginW(s, T).pc = "cased" => \A q \in RescopeBeginW(s, T).deletes : RescopeBeginW(s, T).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RescopeBeginW(s, T).deletes \cap RescopeBeginW(s, T).uploads = {} /\ RescopeBeginW(s, T).gone \subseteq RescopeBeginW(s, T).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RescopeBeginW(s, T).pc \in {"scanned", "claimed", "cased"} => (RescopeBeginW(s, T).uploads \cup RescopeBeginW(s, T).deletes) \cap RescopeBeginW(s, T).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RescopeBeginW(s, T).st = "off" => RescopeBeginW(s, T) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>2. \A q \in Paths : (q \in RescopeBeginW(s, T).unlinked /\ RescopeBeginW(s, T).sStage \in {"saved", "mid"}) =>
          \/ RescopeBeginW(s, T).local[q] = RescopeBeginW(s, T).baseline[q]
          \/ /\ q \in RescopeBeginW(s, T).sDrop \ RescopeBeginW(s, T).sKeep /\ RescopeBeginW(s, T).baseline[q] = Nil
             /\ RescopeBeginW(s, T).local[q] # Nil /\ RescopeBeginW(s, T).sHeld[q] = RescopeBeginW(s, T).local[q]
    BY <1>3, <1>g, <1>r DEF R2
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RescopeBeginW(s, T).pc = "scanned") => RescopeBeginW(s, T).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA RescopeFirst_M1 ==
  ASSUME IndM1, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RescopeFirst_TypeOK
<1>u. RescopeUnciteFirst BY ShippedShape DEF Shipped
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>h. holder' = holder BY DEF RescopeFirst, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "saved" BY DEF RescopeFirst
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RescopeFirstW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RescopeFirstW(s).st = w[s].st
      /\ RescopeFirstW(s).pc = w[s].pc
      /\ RescopeFirstW(s).deletes = w[s].deletes
      /\ RescopeFirstW(s).inst = w[s].inst
      /\ RescopeFirstW(s).uploads = w[s].uploads
      /\ RescopeFirstW(s).gone = w[s].gone
      /\ RescopeFirstW(s).unlinked = w[s].unlinked
      /\ RescopeFirstW(s).sStage = "mid"
      /\ RescopeFirstW(s).local = w[s].local
      /\ RescopeFirstW(s).baseline = [q \in Paths |-> IF q \in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[q]]
      /\ RescopeFirstW(s).sDrop = w[s].sDrop
      /\ RescopeFirstW(s).sKeep = KeepSet(s)
      /\ RescopeFirstW(s).sHeld = [q \in Paths |-> IF q \in RescopeFirstDd(s) /\ w[s].baseline[q] # Nil
                                     THEN w[s].baseline[q] ELSE w[s].sHeld[q]]
  BY <1>u DEF RescopeFirstW
<1>4a. (RescopeFirstW(s).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => RescopeFirstW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RescopeFirstW(s).pc = "cased" => \A q \in RescopeFirstW(s).deletes : RescopeFirstW(s).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RescopeFirstW(s).deletes \cap RescopeFirstW(s).uploads = {} /\ RescopeFirstW(s).gone \subseteq RescopeFirstW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RescopeFirstW(s).pc \in {"scanned", "claimed", "cased"} => (RescopeFirstW(s).uploads \cup RescopeFirstW(s).deletes) \cap RescopeFirstW(s).unlinked = {} BY <1>m, <1>3 DEF Ups
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RescopeFirstW(s).st = "off" => RescopeFirstW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>d. /\ RescopeFirstDd(s) = w[s].sDrop \ KeepSet(s)
        /\ \A q \in w[s].sDrop : w[s].baseline[q] = Nil => q \notin KeepSet(s)
    BY DEF RescopeFirstDd, KeepSet
  <2>e. \A q \in w[s].unlinked :
          \/ w[s].local[q] = w[s].baseline[q]
          \/ /\ q \in w[s].sDrop \ w[s].sKeep /\ w[s].baseline[q] = Nil
             /\ w[s].local[q] # Nil /\ w[s].sHeld[q] = w[s].local[q]
    BY <1>r, <1>g, <1>f DEF R3
  <2>2. \A q \in Paths : (q \in RescopeFirstW(s).unlinked /\ RescopeFirstW(s).sStage \in {"saved", "mid"}) =>
          \/ RescopeFirstW(s).local[q] = RescopeFirstW(s).baseline[q]
          \/ /\ q \in RescopeFirstW(s).sDrop \ RescopeFirstW(s).sKeep /\ RescopeFirstW(s).baseline[q] = Nil
             /\ RescopeFirstW(s).local[q] # Nil /\ RescopeFirstW(s).sHeld[q] = RescopeFirstW(s).local[q]
    BY <1>3, <1>f, <2>d, <2>e
  <2>3. RescopeFirstW(s).sStage = "mid" => \A q \in RescopeFirstW(s).sDrop \ RescopeFirstW(s).sKeep : RescopeFirstW(s).baseline[q] = Nil
    BY <1>3, <1>f, <2>d
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, <2>3, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RescopeFirstW(s).pc = "scanned") => RescopeFirstW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA RescopeSecond_M1 ==
  ASSUME IndM1, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RescopeSecond_TypeOK
<1>u. RescopeUnciteFirst BY ShippedShape DEF Shipped
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>h. holder' = holder BY DEF RescopeSecond, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage = "mid" BY DEF RescopeSecond
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RescopeSecondW(s, wfail) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RescopeSecondW(s, wfail).st = w[s].st
      /\ RescopeSecondW(s, wfail).pc = w[s].pc
      /\ RescopeSecondW(s, wfail).deletes = w[s].deletes
      /\ RescopeSecondW(s, wfail).inst = w[s].inst
      /\ RescopeSecondW(s, wfail).uploads = w[s].uploads
      /\ RescopeSecondW(s, wfail).gone = w[s].gone
      /\ RescopeSecondW(s, wfail).unlinked = (w[s].unlinked \cup {q \in Paths : RescopeUnlink(s, q) /\ RescopeUnciteFirst}) \ RescopeFetch(s, wfail)
      /\ RescopeSecondW(s, wfail).sStage = "none"
      /\ RescopeSecondW(s, wfail).local = [q \in Paths |-> IF q \in RescopeFetch(s, wfail) /\ (RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal)
                                        THEN doc[q] ELSE RescopeLocal1(s)[q]]
      /\ RescopeSecondW(s, wfail).baseline = [q \in Paths |-> IF q \in RescopeFetch(s, wfail) THEN doc[q] ELSE RescopeBase1(s)[q]]
      /\ RescopeSecondW(s, wfail).sDrop = {}
      /\ RescopeSecondW(s, wfail).sKeep = {}
      /\ RescopeSecondW(s, wfail).sHeld = [q \in Paths |-> Nil]
  BY DEF RescopeSecondW
<1>4a. (RescopeSecondW(s, wfail).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => RescopeSecondW(s, wfail).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RescopeSecondW(s, wfail).pc = "cased" => \A q \in RescopeSecondW(s, wfail).deletes : RescopeSecondW(s, wfail).inst[q] = Nil BY <1>m, <1>3 DEF Cased
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RescopeSecondW(s, wfail).deletes \cap RescopeSecondW(s, wfail).uploads = {} /\ RescopeSecondW(s, wfail).gone \subseteq RescopeSecondW(s, wfail).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RescopeSecondW(s, wfail).pc \in {"scanned", "claimed", "cased"} => (RescopeSecondW(s, wfail).uploads \cup RescopeSecondW(s, wfail).deletes) \cap RescopeSecondW(s, wfail).unlinked = {} BY <1>3, <1>g
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RescopeSecondW(s, wfail).st = "off" => RescopeSecondW(s, wfail) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>a. /\ RescopeLocal1(s) = [q \in Paths |-> IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]]
        /\ RescopeBase1(s) = w[s].baseline
    BY <1>u DEF RescopeLocal1, RescopeBase1
  <2>b. \A q \in Paths : RescopeUnlink(s, q) => q \in w[s].sDrop \ w[s].sKeep /\ w[s].local[q] # Nil
    BY DEF RescopeUnlink, RescopeDrop
  <2>c. \A q \in Paths : (q \in w[s].sDrop \ w[s].sKeep /\ w[s].local[q] # Nil /\ w[s].sHeld[q] = w[s].local[q]) => RescopeUnlink(s, q)
    BY DEF RescopeUnlink, RescopeDrop
  <2>d. \A q \in w[s].sDrop \ w[s].sKeep : w[s].baseline[q] = Nil BY <1>r, <1>g DEF R4
  <2>e. \A q \in w[s].unlinked :
          \/ w[s].local[q] = w[s].baseline[q]
          \/ /\ q \in w[s].sDrop \ w[s].sKeep /\ w[s].baseline[q] = Nil
             /\ w[s].local[q] # Nil /\ w[s].sHeld[q] = w[s].local[q]
    BY <1>r, <1>g, <1>f DEF R3
  <2>1. \A q \in Paths : (q \in RescopeSecondW(s, wfail).unlinked /\ RescopeSecondW(s, wfail).sStage = "none")
          => RescopeSecondW(s, wfail).local[q] = RescopeSecondW(s, wfail).baseline[q]
    <3>. SUFFICES ASSUME NEW q \in Paths, q \in RescopeSecondW(s, wfail).unlinked
                  PROVE  RescopeSecondW(s, wfail).local[q] = RescopeSecondW(s, wfail).baseline[q]
      OBVIOUS
    <3>1. q \notin RescopeFetch(s, wfail) /\ (q \in w[s].unlinked \/ RescopeUnlink(s, q)) BY <1>3, <1>u
    <3>2. /\ RescopeSecondW(s, wfail).local[q] = IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]
          /\ RescopeSecondW(s, wfail).baseline[q] = w[s].baseline[q]
      BY <1>3, <2>a, <3>1
    <3>3. CASE RescopeUnlink(s, q) BY <3>2, <3>3, <2>b, <2>d
    <3>4. CASE ~RescopeUnlink(s, q)
      <4>1. q \in w[s].unlinked BY <3>1, <3>4
      <4>2. w[s].local[q] = w[s].baseline[q] BY <4>1, <2>e, <2>c, <3>4
      <4>. QED BY <3>2, <3>4, <4>2
    <3>. QED BY <3>3, <3>4
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>1, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RescopeSecondW(s, wfail).pc = "scanned") => RescopeSecondW(s, wfail).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

------------------------------------------------------------------------------
(* A reader's tick.                                                         *)

LEMMA RPullRead_M1 ==
  ASSUME IndM1, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RPullRead_TypeOK
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] BY DEF RPullRead
<1>h. holder' = holder BY DEF RPullRead, bucket
<1>g. On(s) /\ w[s].pc = "idle" /\ w[s].sStage \in {"none", "saved"} BY DEF RPullRead
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RPullReadW(s) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RPullReadW(s).st = w[s].st
      /\ RPullReadW(s).pc = "pulling"
      /\ RPullReadW(s).deletes = w[s].deletes
      /\ RPullReadW(s).inst = w[s].inst
      /\ RPullReadW(s).uploads = w[s].uploads
      /\ RPullReadW(s).gone = w[s].gone
      /\ RPullReadW(s).unlinked = w[s].unlinked
      /\ RPullReadW(s).sStage = w[s].sStage
      /\ RPullReadW(s).local = w[s].local
      /\ RPullReadW(s).baseline = w[s].baseline
      /\ RPullReadW(s).sDrop = w[s].sDrop
      /\ RPullReadW(s).sKeep = w[s].sKeep
      /\ RPullReadW(s).sHeld = w[s].sHeld
  BY DEF RPullReadW
<1>4a. (RPullReadW(s).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => RPullReadW(s).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RPullReadW(s).pc = "cased" => \A q \in RPullReadW(s).deletes : RPullReadW(s).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RPullReadW(s).deletes \cap RPullReadW(s).uploads = {} /\ RPullReadW(s).gone \subseteq RPullReadW(s).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RPullReadW(s).pc \in {"scanned", "claimed", "cased"} => (RPullReadW(s).uploads \cup RPullReadW(s).deletes) \cap RPullReadW(s).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RPullReadW(s).st = "off" => RPullReadW(s) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9a. /\ RPullReadW(s).sStage = w[s].sStage /\ RPullReadW(s).unlinked \subseteq w[s].unlinked
       /\ \A q \in RPullReadW(s).unlinked : RPullReadW(s).local[q] = w[s].local[q]
       /\ RPullReadW(s).baseline = w[s].baseline /\ RPullReadW(s).sDrop = w[s].sDrop /\ RPullReadW(s).sKeep = w[s].sKeep /\ RPullReadW(s).sHeld = w[s].sHeld
  BY <1>3, <1>f
<1>9b. /\ (RPullReadW(s).pc \in {"consumed", "scanned", "claimed", "cased"} => RPullReadW(s).sStage = "none")
       /\ (RPullReadW(s).pc = "pulling" => RPullReadW(s).sStage \in {"none", "saved"})
  BY <1>3, <1>g, <1>r DEF R1, R5
<1>9. Rescope' BY <1>m, <1>2, <1>9a, <1>9b, RescopeShrink
<1>10a. (w[s].pc = "consumed" /\ RPullReadW(s).pc = "scanned") => RPullReadW(s).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

LEMMA RPullSync_M1 ==
  ASSUME IndM1, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndM1' /\ NarrowOK
<1>a. IndTypeOK BY DEF IndM1
<1>b. TypeOK BY <1>a DEF IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Cased /\ Mine /\ Ups /\ Off /\ Rescope BY DEF IndM1, M1
<1>r. R1 /\ R2 /\ R3 /\ R4 /\ R5 BY <1>m DEF Rescope
<1>0. w[s] \in Writer /\ holder \in Writers \cup {"none"} BY <1>b DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
      /\ w[s].unlinked \subseteq Paths /\ w[s].uploads \subseteq Paths /\ w[s].deletes \subseteq Paths
      /\ w[s].sDrop \subseteq Paths /\ w[s].sKeep \subseteq Paths
      /\ w[s].sStage \in {"none", "saved", "mid"}
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"}
  BY <1>0, WriterFields
<1>t. IndTypeOK' BY <1>a, RPullSync_TypeOK
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>h. holder' = holder BY DEF RPullSync, bucket
<1>g. On(s) /\ w[s].pc = "pulling" BY DEF RPullSync
<1>2. \A t \in Writers : w'[t] = IF t = s THEN RPullSyncW(s, fail) ELSE w[t] BY <1>b, <1>1, WriteAny
<1>3. /\ RPullSyncW(s, fail).st = w[s].st
      /\ RPullSyncW(s, fail).pc = "idle"
      /\ RPullSyncW(s, fail).deletes = w[s].deletes
      /\ RPullSyncW(s, fail).inst = w[s].inst
      /\ RPullSyncW(s, fail).uploads = w[s].uploads
      /\ RPullSyncW(s, fail).gone = w[s].gone
      /\ RPullSyncW(s, fail).unlinked = w[s].unlinked
      /\ RPullSyncW(s, fail).sStage = w[s].sStage
      /\ RPullSyncW(s, fail).local = [q \in Paths |-> IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]
      /\ RPullSyncW(s, fail).baseline = SyncBl(s, fail)
      /\ RPullSyncW(s, fail).sDrop = w[s].sDrop
      /\ RPullSyncW(s, fail).sKeep = w[s].sKeep
      /\ RPullSyncW(s, fail).sHeld = w[s].sHeld
  BY DEF RPullSyncW
<1>4a. (RPullSyncW(s, fail).pc \in CC <=> w[s].pc \in CC) /\ (w[s].st = "on" => RPullSyncW(s, fail).st = "on") BY <1>3, <1>g DEF CC
<1>4. Inv_OneHolder' /\ Holder' BY <1>m, <1>0, <1>2, <1>4a, <1>h, HolderWrite
<1>5a. RPullSyncW(s, fail).pc = "cased" => \A q \in RPullSyncW(s, fail).deletes : RPullSyncW(s, fail).inst[q] = Nil BY <1>3
<1>5. Cased' BY <1>m, <1>2, <1>5a, CasedWrite
<1>6a. RPullSyncW(s, fail).deletes \cap RPullSyncW(s, fail).uploads = {} /\ RPullSyncW(s, fail).gone \subseteq RPullSyncW(s, fail).uploads BY <1>m, <1>3 DEF Mine
<1>6. Mine' BY <1>m, <1>2, <1>6a, MineWrite
<1>7a. RPullSyncW(s, fail).pc \in {"scanned", "claimed", "cased"} => (RPullSyncW(s, fail).uploads \cup RPullSyncW(s, fail).deletes) \cap RPullSyncW(s, fail).unlinked = {} BY <1>3
<1>7. Ups' BY <1>m, <1>2, <1>7a, UpsWrite
<1>8a. RPullSyncW(s, fail).st = "off" => RPullSyncW(s, fail) = WriterInit BY <1>3, <1>g DEF On
<1>8. Off' BY <1>m, <1>2, <1>8a, OffWrite
<1>9. Rescope'
  <2>0. \A q \in Paths : q \in SyncOwed(s, fail) => w[s].local[q] = w[s].baseline[q] BY DEF SyncOwed, SyncAll, Owed
  <2>1. \A q \in Paths : /\ (w[s].local[q] = w[s].baseline[q] => RPullSyncW(s, fail).local[q] = RPullSyncW(s, fail).baseline[q])
                        /\ (q \notin SyncOwed(s, fail) => RPullSyncW(s, fail).local[q] = w[s].local[q] /\ RPullSyncW(s, fail).baseline[q] = w[s].baseline[q])
    BY <1>3, <2>0 DEF SyncBl
  <2>2. \A q \in Paths : (q \in RPullSyncW(s, fail).unlinked /\ RPullSyncW(s, fail).sStage = "none") => RPullSyncW(s, fail).local[q] = RPullSyncW(s, fail).baseline[q]
    BY <1>3, <1>r, <2>1 DEF R2
  <2>3. \A q \in Paths : (q \in RPullSyncW(s, fail).unlinked /\ RPullSyncW(s, fail).sStage \in {"saved", "mid"}) =>
          \/ RPullSyncW(s, fail).local[q] = RPullSyncW(s, fail).baseline[q]
          \/ /\ q \in RPullSyncW(s, fail).sDrop \ RPullSyncW(s, fail).sKeep /\ RPullSyncW(s, fail).baseline[q] = Nil
             /\ RPullSyncW(s, fail).local[q] # Nil /\ RPullSyncW(s, fail).sHeld[q] = RPullSyncW(s, fail).local[q]
    BY <1>3, <1>r, <2>0, <2>1 DEF R3
  <2>4. RPullSyncW(s, fail).sStage = "mid" => \A q \in RPullSyncW(s, fail).sDrop \ RPullSyncW(s, fail).sKeep : RPullSyncW(s, fail).baseline[q] = Nil BY <1>3, <1>g, <1>r DEF R5
  <2>. QED BY <1>m, <1>2, <1>3, <1>g, <2>2, <2>3, <2>4, RescopeWrite
<1>10a. (w[s].pc = "consumed" /\ RPullSyncW(s, fail).pc = "scanned") => RPullSyncW(s, fail).deletes \cap w[s].unlinked = {} BY <1>3
<1>10. NarrowOK BY <1>2, <1>10a, NarrowWrite
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10 DEF IndM1, M1

------------------------------------------------------------------------------
(* The step, and the theorems.                                              *)

LEMMA Next_M1 == IndM1 /\ Next => IndM1' /\ NarrowOK
<1>. SUFFICES ASSUME IndM1, Next PROVE IndM1' /\ NarrowOK OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM1' /\ NarrowOK BY <3>1, GPut_M1
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM1' /\ NarrowOK BY <3>2, GCas_M1
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM1' /\ NarrowOK BY <3>3, GDelete_M1
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM1' /\ NarrowOK BY <3>4, GRename_M1
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M1
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM1' /\ NarrowOK BY <3>1, Checkout_M1
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM1' /\ NarrowOK BY <3>2, Consume_M1
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM1' /\ NarrowOK BY <3>3, Scan_M1
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM1' /\ NarrowOK BY <3>4, Skip_M1
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM1' /\ NarrowOK BY <3>5, PullOnly_M1
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM1' /\ NarrowOK BY <3>6, Claim_M1
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM1' /\ NarrowOK BY <3>7, Verify_M1
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM1' /\ NarrowOK BY <3>8, Install_M1
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM1' /\ NarrowOK BY <3>9, Collect_M1
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM1' /\ NarrowOK BY <3>10, Finish_M1
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM1' /\ NarrowOK BY <3>11, Restart_M1
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM1' /\ NarrowOK BY <3>12, Sync_M1
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM1' /\ NarrowOK BY <3>13, RescopeBegin_M1
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM1' /\ NarrowOK BY <3>14, RescopeFirst_M1
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM1' /\ NarrowOK BY <3>15, RescopeSecond_M1
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM1' /\ NarrowOK BY <3>16, RPullRead_M1
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM1' /\ NarrowOK BY <3>17, RPullSync_M1
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM1' /\ NarrowOK BY <3>18, Edit_M1
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM1' /\ NarrowOK BY <3>19, Delete_M1
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM1' /\ NarrowOK BY <3>20, Upload_M1
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM1' /\ NarrowOK BY <3>21, Sweep_M1
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M1
  <2>2. CASE RLoad BY <2>2, RLoad_M1
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM1' /\ NarrowOK BY <2>3, Reap_M1
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M1Invariant == Spec => []IndM1
<1>1. Init => IndM1 BY Init_M1
<1>2. IndM1 /\ [Next]_vars => IndM1'
  <2>1. IndM1 /\ Next => IndM1' BY Next_M1
  <2>2. IndM1 /\ UNCHANGED vars => IndM1'
    <3>1. IndM1 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM1 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM1, vars
    <3>. QED BY <3>1, <3>2 DEF IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

THEOREM OneHolder == Spec => []Inv_OneHolder
<1>1. IndM1 => Inv_OneHolder BY DEF IndM1, M1
<1>. QED BY M1Invariant, <1>1, PTL

THEOREM DeleteSettles == Spec => Prop_DeleteSettles
<1>1. ASSUME IndM1, NEW s \in Writers, Finish(s)
      PROVE  \A p \in w[s].deletes : w[s].inst[p] = Nil \/ w'[s].local[p] = w[s].inst[p]
  BY <1>1 DEF IndM1, M1, Cased, Finish
<1>2. IndM1 => [\A s \in Writers :
                  Finish(s) => \A p \in w[s].deletes : w[s].inst[p] = Nil \/ w'[s].local[p] = w[s].inst[p]]_vars
  BY <1>1
<1>. QED BY M1Invariant, <1>2, PTL DEF Prop_DeleteSettles

THEOREM NarrowNeverDeletes == Spec => Prop_NarrowNeverDeletes
<1>1. IndM1 /\ [Next]_vars => [NarrowOK]_vars
  <2>1. IndM1 /\ Next => NarrowOK BY Next_M1
  <2>. QED BY <2>1
<1>. QED BY M1Invariant, <1>1, PTL DEF Spec, Prop_NarrowNeverDeletes, NarrowOK
==============================================================================
