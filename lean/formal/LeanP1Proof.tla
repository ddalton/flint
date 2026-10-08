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
   once (`<1>3`) and discharges each lemma's hypothesis from them.
   M2 (after M1): `Inv_CitationsLive` and `Inv_OneName` over `IndM2` --
   IndM1 and the plan's I2-I5 as the proof needs them (results/
   2026-10-07-tlaps-m2/NOTES.txt): a handle never PUT is private to its
   minter; an upload is uncited from its PUT to the CAS; a verified one
   is live; a save in flight is live, uncited and in no tree.  Each step
   states its events in four normalised facts (what the document, the
   saves in flight, the retired set and the written tree may now hold)
   and one lemma per conjunct consumes them.
   M3 (after M2): `Inv_ShortcutSound` and `Inv_ReaderSound` over `IndM3`
   -- IndM2 and the plan's I8-I10 as the proof needs them (results/
   2026-10-07-tlaps-m3/NOTES.txt): while the pointer is the one a writer
   last derived against, every held path where the document and the
   baseline differ is skipped; a cased writer carries what its finish
   needs from the CAS.  Each step states one event (`SeqEv`) and its
   written tree; `M3Write` turns five facts about that tree into M3'.
   M4 (after M3): `Inv_ReaderFetches` over `IndM4` -- IndM3 and the
   plan's I11 with the never-re-cited lemma (results/2026-10-07-tlaps-m4/
   NOTES.txt): a cited handle is never retiring or aged, and a reader
   that loaded the document less than G ago holds handles each live,
   not aged, and still cited or retiring.
   M5 (after M4), part A: the history conjuncts over `IndM5` -- `base`,
   `anc` and `orig` are written only at the handle a step mints, so
   Derives and Content between minted handles never move
   (results/2026-10-07-tlaps-m5/NOTES.txt).                            *)
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

------------------------------------------------------------------------------
(* M2: Inv_CitationsLive, Inv_OneName.                                      *)

CitedSet == {doc[p] : p \in {q \in Paths : doc[q] # Nil}}
\* In no other tree, in no other snapshot.
Priv(s, h) == \A t \in Writers \ {s}, q \in Paths : w[t].local[q] # h /\ w[t].snap[q] # h
\* I2: freshness -- what makes a mint new.
Fresh ==
  /\ live \subseteq upped /\ upped \subseteq minted
  /\ (retiring \cup aged) \subseteq upped
  /\ nextGen <= MaxMint + 1
  /\ \A h \in minted : Gen(h) < nextGen \/ (Gen(h) > MaxMint /\ Gen(h) <= MaxMint + copies)
\* ...and a snapshot holds minted handles (doc, gw, local: Minted).
SnapMinted == \A s \in Writers, p \in Paths : w[s].snap[p] \in Opt(minted)
\* I3: a save in flight is live, uncited, unretired, at its own path, and in
\* no tree or snapshot.
Flight == \A p \in Paths : gw[p] # Nil =>
            /\ gw[p] \in live /\ ~Cited(gw[p]) /\ gw[p] \notin retiring \cup aged
            /\ gw[p][1] = p
            /\ \A s \in Writers, q \in Paths : w[s].local[q] # gw[p] /\ w[s].snap[q] # gw[p]
\* A tree entry never PUT is its writer's own, at its own path.
Private == \A s \in Writers, p \in Paths :
             (w[s].local[p] # Nil /\ w[s].local[p] \notin upped)
               => w[s].local[p][1] = p /\ Priv(s, w[s].local[p])
\* An upload before its PUT: a never-PUT handle (private, at its path) or an
\* already-PUT one (the copy case).
Pending == \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
             \A p \in w[s].uploads \ w[s].upDone :
               /\ w[s].snap[p] # Nil
               /\ (w[s].snap[p] \notin upped => w[s].snap[p][1] = p /\ Priv(s, w[s].snap[p]))
\* I4: an upload from its PUT to the CAS.
Up(s, h) == /\ h # Nil /\ h \in upped /\ ~Cited(h) /\ h \notin retiring \cup aged
            /\ \A q \in Paths : gw[q] # h
            /\ Priv(s, h)
Uploaded == \A s \in Writers : w[s].pc \in {"scanned", "claimed"} =>
              \A p \in w[s].upDone : w[s].snap[p][1] = p /\ Up(s, w[s].snap[p])
\* I5: a verified upload is live until the CAS.
Verified == \A s \in Writers : (w[s].pc = "claimed" /\ w[s].verified) =>
              \A p \in (w[s].uploads \cap w[s].upDone) \ w[s].gone : w[s].snap[p] \in live
\* A scanned writer has not verified yet.
Unverified == \A s \in Writers : w[s].pc = "scanned" => ~w[s].verified
M2 == Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
IndM2 == IndM1 /\ M2

\* The step's events, as the conjunct lemmas read them: what the document
\* may now cite (NC: this step's new citations), what may be in flight, what
\* may be retired, and what a written tree may hold.
DocEv(NC) == \A k \in Paths : doc'[k] = doc[k] \/ doc'[k] = Nil \/ doc'[k] \in CitedSet \/ doc'[k] \in NC
GwEv == \A k \in Paths : gw'[k] = gw[k] \/ gw'[k] = Nil \/ gw'[k] \notin minted
RetEv == retiring' \subseteq retiring \cup CitedSet /\ aged' \subseteq aged \cup retiring
TreeEv(t, R) == t \in Writers =>
  /\ \A q \in Paths : R.local[q] = w[t].local[q] \/ R.local[q] = Nil \/ R.local[q] \notin minted \/ R.local[q] \in CitedSet
  /\ \A q \in Paths : R.snap[q] = w[t].snap[q] \/ R.snap[q] = w[t].local[q] \/ R.snap[q] = Nil \/ R.snap[q] \notin minted
\* What a shrinking `live` loses: uncited, not in flight, and aged or spared
\* by the holder (the sweep spares its own PUTs).
LiveEv == \A h \in live : h \notin live' =>
            /\ ~Cited(h) /\ (\A k \in Paths : gw[k] # h)
            /\ (h \in aged \/ \A u \in Writers : holder = u => \A k \in w[u].upDone : w[u].snap[k] # h)
\* The written tree: t's tree becomes R, or no tree changes (t = "none").
Wr(t, R) == \A u \in Writers : w'[u] = IF u = t THEN R ELSE w[u]

------------------------------------------------------------------------------
(* What the events give each conjunct.                                      *)

LEMMA RetFacts ==
  ASSUME RetireAge, RetUpdate, aged' = aged
  PROVE  RetEv
BY DEF RetUpdate, RetEv, CitedSet

LEMMA WrNone == w' = w => Wr("none", w)
BY NoneWriter DEF Wr

\* A handle never PUT: not live, so not cited, not retiring or aged, not in flight.
LEMMA Unupped ==
  ASSUME Fresh, Flight, Inv_CitationsLive, NEW h, h # Nil, h \notin upped
  PROVE  ~Cited(h) /\ h \notin retiring \cup aged /\ (\A q \in Paths : gw[q] # h) /\ h \notin live
<1>1. h \notin live BY DEF Fresh
<1>2. ~Cited(h) BY <1>1 DEF Inv_CitationsLive, Cited
<1>3. h \notin retiring \cup aged BY DEF Fresh
<1>4. \A q \in Paths : gw[q] # h BY <1>1 DEF Flight
<1>. QED BY <1>1, <1>2, <1>3, <1>4

LEMMA PrivKeep ==
  ASSUME NEW s \in Writers, NEW h, h # Nil, h \in minted, ~Cited(h), Priv(s, h),
         NEW t, NEW R, Wr(t, R), TreeEv(t, R)
  PROVE  Priv(s, h)'
BY DEF Priv, Wr, TreeEv, CitedSet, Cited

LEMMA UpKeep ==
  ASSUME NEW s \in Writers, NEW h, Up(s, h), h \in minted, NEW NC, h \notin NC,
         upped \subseteq upped', DocEv(NC), GwEv, RetEv,
         NEW t, NEW R, Wr(t, R), TreeEv(t, R)
  PROVE  Up(s, h)'
<1>1. h # Nil /\ h \in upped' /\ ~Cited(h) /\ Priv(s, h) BY DEF Up
<1>2. ~Cited(h)' BY <1>1 DEF DocEv, CitedSet, Cited
<1>3. h \notin retiring' \cup aged' BY <1>1 DEF Up, RetEv, CitedSet, Cited
<1>4. \A q \in Paths : gw'[q] # h BY <1>1 DEF Up, GwEv
<1>5. Priv(s, h)' BY <1>1, PrivKeep
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Up

\* A fresh mint is in no tree, no snapshot, no save in flight, not cited.
LEMMA MintPriv ==
  ASSUME Minted, SnapMinted, NEW t \in Writers, NEW h, h # Nil, h \notin minted,
         \A u \in Writers : u # t => w'[u] = w[u]
  PROVE  Priv(t, h)'
BY DEF Priv, Minted, SnapMinted, Opt

LEMMA MintUp ==
  ASSUME Minted, SnapMinted, Fresh, NEW t \in Writers, NEW h, h # Nil, h \notin minted, h \in upped',
         doc' = doc, gw' = gw, retiring' = retiring, aged' = aged,
         \A u \in Writers : u # t => w'[u] = w[u]
  PROVE  Up(t, h)'
<1>1. ~Cited(h)' BY DEF Minted, Cited, Opt
<1>2. h \notin retiring' \cup aged' BY DEF Fresh
<1>3. \A q \in Paths : gw'[q] # h BY DEF Minted, Opt
<1>4. Priv(t, h)' BY MintPriv
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF Up

LEMMA FlightKeep ==
  ASSUME Flight, Minted, NEW NC,
         \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin NC,
         DocEv(NC), RetEv, LiveEv,
         NEW t, NEW R, Wr(t, R), TreeEv(t, R)
  PROVE  Flight'
<1>. SUFFICES ASSUME NEW p \in Paths, gw'[p] # Nil
              PROVE  /\ gw'[p] \in live' /\ ~(\E k \in Paths : doc'[k] = gw'[p]) /\ gw'[p] \notin retiring' \cup aged'
                     /\ gw'[p][1] = p
                     /\ \A s \in Writers, q \in Paths : w'[s].local[q] # gw'[p] /\ w'[s].snap[q] # gw'[p]
  BY DEF Flight, Cited
<1>1. gw'[p] = gw[p] /\ gw[p] \notin NC /\ gw[p] # Nil OBVIOUS
<1>2a. gw[p] \in live /\ ~Cited(gw[p]) /\ gw[p] \notin retiring \cup aged /\ gw[p][1] = p BY <1>1 DEF Flight
<1>2b. \A s \in Writers, q \in Paths : w[s].local[q] # gw[p] /\ w[s].snap[q] # gw[p] BY <1>1 DEF Flight
<1>2. /\ gw[p] \in live /\ ~Cited(gw[p]) /\ gw[p] \notin retiring \cup aged /\ gw[p][1] = p
      /\ \A s \in Writers, q \in Paths : w[s].local[q] # gw[p] /\ w[s].snap[q] # gw[p]
  BY <1>2a, <1>2b
<1>3. gw[p] \in minted BY <1>1 DEF Minted, Opt
<1>4. gw[p] \in live' BY <1>2 DEF LiveEv
<1>5. ~(\E k \in Paths : doc'[k] = gw[p]) BY <1>1, <1>2 DEF DocEv, CitedSet, Cited
<1>6. gw[p] \notin retiring' \cup aged' BY <1>2 DEF RetEv, CitedSet, Cited
<1>7. \A s \in Writers, q \in Paths : w'[s].local[q] # gw[p] /\ w'[s].snap[q] # gw[p]
  BY <1>1, <1>2, <1>3 DEF Wr, TreeEv, CitedSet, Cited
<1>. QED BY <1>1, <1>2, <1>4, <1>5, <1>6, <1>7

\* GPut: the new save in flight.
LEMMA MintFlight ==
  ASSUME Flight, Minted, SnapMinted, Fresh, NEW p \in Paths, NEW h, h # Nil, h \notin minted, h[1] = p,
         gw \in [Paths -> Opt(Handles)],
         gw' = [gw EXCEPT ![p] = h], live' = live \cup {h}, doc' = doc, RetEv, w' = w, Inv_CitationsLive
  PROVE  Flight'
<1>0. (retiring' \cup aged') \subseteq minted BY DEF RetEv, Fresh, CitedSet, Inv_CitationsLive
<1>1. /\ h \in live' /\ ~Cited(h)' /\ h \notin retiring' \cup aged' /\ h[1] = p
      /\ \A s \in Writers, q \in Paths : w'[s].local[q] # h /\ w'[s].snap[q] # h
  BY <1>0 DEF Minted, SnapMinted, Fresh, Cited, Opt
<1>2. \A q \in Paths : q # p /\ gw[q] # Nil =>
        /\ gw[q] \in live' /\ ~Cited(gw[q])' /\ gw[q] \notin retiring' \cup aged' /\ gw[q][1] = q
        /\ \A s \in Writers, r \in Paths : w'[s].local[r] # gw[q] /\ w'[s].snap[r] # gw[q]
  BY DEF Flight, RetEv, CitedSet, Cited
<1>. QED BY <1>1, <1>2 DEF Flight

LEMMA FreshKeep ==
  ASSUME Fresh, Inv_CitationsLive, minted' = minted, nextGen' = nextGen, copies' = copies, upped' = upped,
         live' \subseteq live, RetEv
  PROVE  Fresh'
<1>1. CitedSet \subseteq live BY DEF CitedSet, Inv_CitationsLive
<1>2. (retiring' \cup aged') \subseteq upped BY <1>1 DEF RetEv, Fresh
<1>. QED BY <1>2 DEF Fresh

\* Upload's first case: a PUT of a handle minted earlier.
LEMMA FreshPut ==
  ASSUME Fresh, Inv_CitationsLive, NEW h \in minted, minted' = minted, nextGen' = nextGen, copies' = copies,
         upped' = upped \cup {h}, live' = live \cup {h}, RetEv
  PROVE  Fresh'
<1>1. CitedSet \subseteq live BY DEF CitedSet, Inv_CitationsLive
<1>2. (retiring' \cup aged') \subseteq upped BY <1>1 DEF RetEv, Fresh
<1>. QED BY <1>2 DEF Fresh

\* GPut and Edit: a mint at nextGen.
LEMMA FreshMint ==
  ASSUME Fresh, Inv_CitationsLive, minted \subseteq Handles, NEW p \in Paths, nextGen \in Nat, nextGen <= MaxMint,
         minted' = minted \cup {<<p, nextGen>>}, nextGen' = nextGen + 1, copies' = copies,
         upped' \in {upped, upped \cup {<<p, nextGen>>}}, live' \in {live, live \cup {<<p, nextGen>>}},
         live' \subseteq upped', RetEv
  PROVE  Fresh' /\ <<p, nextGen>> \notin minted
<1>1. CitedSet \subseteq live BY DEF CitedSet, Inv_CitationsLive
<1>2. (retiring' \cup aged') \subseteq upped' BY <1>1 DEF RetEv, Fresh
<1>g. Gen(<<p, nextGen>>) = nextGen BY DEF Gen
<1>3. <<p, nextGen>> \notin minted BY <1>g, MaxMintNat, MaxCopiesNat DEF Fresh
<1>4. \A h \in minted' : Gen(h) < nextGen' \/ (Gen(h) > MaxMint /\ Gen(h) <= MaxMint + copies')
  <2>. SUFFICES ASSUME NEW h \in minted' PROVE Gen(h) < nextGen' \/ (Gen(h) > MaxMint /\ Gen(h) <= MaxMint + copies') OBVIOUS
  <2>0. h \in minted => Gen(h) \in Nat BY HandleGen, MaxMintNat, MaxCopiesNat DEF Gens, Seed
  <2>1. CASE h \in minted BY <2>0, <2>1, MaxMintNat DEF Fresh
  <2>2. CASE h = <<p, nextGen>> BY <2>2, <1>g
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>2, <1>3, <1>4, MaxMintNat DEF Fresh

\* UploadCopy: a mint at MaxMint + copies + 1, PUT at once.
LEMMA FreshCopy ==
  ASSUME Fresh, Ghosts, Inv_CitationsLive, minted \subseteq Handles, NEW p \in Paths, copies < MaxCopies, nextGen \in Nat,
         minted' = minted \cup {<<p, MaxMint + copies + 1>>}, nextGen' = nextGen, copies' = copies + 1,
         upped' = upped \cup {<<p, MaxMint + copies + 1>>}, live' = live \cup {<<p, MaxMint + copies + 1>>}, RetEv
  PROVE  Fresh' /\ <<p, MaxMint + copies + 1>> \notin minted
<1>1. CitedSet \subseteq live BY DEF CitedSet, Inv_CitationsLive
<1>2. (retiring' \cup aged') \subseteq upped' BY <1>1 DEF RetEv, Fresh
<1>g. Gen(<<p, MaxMint + copies + 1>>) = MaxMint + copies + 1 BY DEF Gen
<1>3. <<p, MaxMint + copies + 1>> \notin minted BY <1>g, MaxMintNat, MaxCopiesNat DEF Fresh, Ghosts
<1>4. \A h \in minted' : Gen(h) < nextGen' \/ (Gen(h) > MaxMint /\ Gen(h) <= MaxMint + copies')
  <2>. SUFFICES ASSUME NEW h \in minted' PROVE Gen(h) < nextGen' \/ (Gen(h) > MaxMint /\ Gen(h) <= MaxMint + copies') OBVIOUS
  <2>0. h \in minted => Gen(h) \in Nat BY HandleGen, MaxMintNat, MaxCopiesNat DEF Gens, Seed
  <2>1. CASE h \in minted BY <2>0, <2>1, MaxMintNat, MaxCopiesNat DEF Fresh, Ghosts
  <2>2. CASE h = <<p, MaxMint + copies + 1>> BY <2>2, <1>g, MaxMintNat, MaxCopiesNat DEF Ghosts
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>2, <1>3, <1>4 DEF Fresh

LEMMA SnapMintedWrite ==
  ASSUME SnapMinted, minted \subseteq minted', NEW t, NEW R, Wr(t, R),
         t \in Writers => \A q \in Paths : R.snap[q] \in Opt(minted')
  PROVE  SnapMinted'
BY DEF SnapMinted, Wr, Opt

LEMMA PrivateWrite ==
  ASSUME Private, Fresh, Flight, Inv_CitationsLive, Minted, SnapMinted,
         upped \subseteq upped', NEW t, NEW R, Wr(t, R), TreeEv(t, R),
         t \in Writers =>
           \A p \in Paths : (R.local[p] # Nil /\ R.local[p] \notin upped') =>
             \/ R.local[p] = w[t].local[p]
             \/ (R.local[p][1] = p /\ R.local[p] \notin minted)
  PROVE  Private'
<1>. SUFFICES ASSUME NEW s \in Writers, NEW p \in Paths, w'[s].local[p] # Nil, w'[s].local[p] \notin upped'
              PROVE  /\ w'[s].local[p][1] = p
                     /\ \A u \in Writers \ {s}, q \in Paths : w'[u].local[q] # w'[s].local[p] /\ w'[u].snap[q] # w'[s].local[p]
  BY DEF Private, Priv
<1>1. CASE s # t \/ w'[s].local[p] = w[s].local[p]
  <2>1. w'[s].local[p] = w[s].local[p] BY <1>1 DEF Wr
  <2>1a. w[s].local[p] # Nil /\ w[s].local[p] \notin upped BY <2>1
  <2>2. w[s].local[p][1] = p /\ Priv(s, w[s].local[p]) BY <2>1a DEF Private
  <2>3. ~Cited(w[s].local[p]) BY <2>1a, Unupped
  <2>4. w[s].local[p] \in minted BY <2>1a DEF Minted, Opt
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, PrivKeep DEF Priv
<1>2. CASE s = t /\ w'[s].local[p] # w[s].local[p]
  <2>1. w'[s].local[p] = R.local[p] BY <1>2 DEF Wr
  <2>2. R.local[p][1] = p /\ R.local[p] \notin minted /\ R.local[p] # Nil BY <1>2, <2>1
  <2>3. \A u \in Writers : u # s => w'[u] = w[u] BY <1>2 DEF Wr
  <2>. QED BY <1>2, <2>1, <2>2, <2>3, MintPriv DEF Priv
<1>. QED BY <1>1, <1>2

LEMMA PendingWrite ==
  ASSUME Pending, Private, Fresh, Flight, Inv_CitationsLive, Minted, SnapMinted,
         w' \in [Writers -> Writer],
         upped \subseteq upped', NEW t, NEW R, Wr(t, R), TreeEv(t, R),
         t \in Writers => (R.pc \in {"scanned", "claimed"} =>
           \A p \in R.uploads \ R.upDone :
             /\ R.snap[p] # Nil
             /\ (R.snap[p] \notin upped' =>
                   \/ (R.snap[p] = w[t].snap[p] /\ w[t].pc \in {"scanned", "claimed"} /\ p \in w[t].uploads \ w[t].upDone)
                   \/ R.snap[p] = w[t].local[p]
                   \/ (R.snap[p][1] = p /\ R.snap[p] \notin minted)))
  PROVE  Pending'
<1>. SUFFICES ASSUME NEW s \in Writers, w'[s].pc \in {"scanned", "claimed"},
                     NEW p \in w'[s].uploads \ w'[s].upDone
              PROVE  /\ w'[s].snap[p] # Nil
                     /\ (w'[s].snap[p] \notin upped' =>
                           /\ w'[s].snap[p][1] = p
                           /\ \A u \in Writers \ {s}, q \in Paths : w'[u].local[q] # w'[s].snap[p] /\ w'[u].snap[q] # w'[s].snap[p])
  BY DEF Pending, Priv
<1>0. p \in Paths BY WriterFields
<1>1. CASE s # t
  <2>1. w'[s] = w[s] BY <1>1 DEF Wr
  <2>2. w[s].snap[p] # Nil /\ (w[s].snap[p] \notin upped => w[s].snap[p][1] = p /\ Priv(s, w[s].snap[p]))
    BY <2>1 DEF Pending
  <2>3. w[s].snap[p] \in minted BY <1>0, <2>2 DEF SnapMinted, Opt
  <2>. QED BY <2>1, <2>2, <2>3, Unupped, PrivKeep DEF Priv
<1>2. CASE s = t
  <2>0. w'[s] = R BY <1>2 DEF Wr
  <2>1. R.snap[p] # Nil BY <1>2, <2>0
  <2>2. ASSUME R.snap[p] \notin upped' PROVE R.snap[p][1] = p /\ Priv(s, R.snap[p])'
    <3>1. R.snap[p] \notin upped BY <2>2
    <3>2. CASE R.snap[p] = w[t].snap[p] /\ w[t].pc \in {"scanned", "claimed"} /\ p \in w[t].uploads \ w[t].upDone
      <4>1. w[s].snap[p][1] = p /\ Priv(s, w[s].snap[p]) BY <1>2, <3>1, <3>2 DEF Pending
      <4>2. ~Cited(w[s].snap[p]) /\ w[s].snap[p] \in minted BY <1>0, <1>2, <2>1, <3>1, <3>2, Unupped DEF SnapMinted, Opt
      <4>. QED BY <1>2, <2>1, <3>2, <4>1, <4>2, PrivKeep DEF Priv
    <3>3. CASE R.snap[p] = w[t].local[p]
      <4>1. w[s].local[p][1] = p /\ Priv(s, w[s].local[p]) BY <1>0, <1>2, <2>1, <3>1, <3>3 DEF Private
      <4>2. ~Cited(w[s].local[p]) /\ w[s].local[p] \in minted BY <1>0, <1>2, <2>1, <3>1, <3>3, Unupped DEF Minted, Opt
      <4>. QED BY <1>2, <2>1, <3>3, <4>1, <4>2, PrivKeep DEF Priv
    <3>4. CASE R.snap[p][1] = p /\ R.snap[p] \notin minted
      <4>1. \A u \in Writers : u # s => w'[u] = w[u] BY <1>2 DEF Wr
      <4>. QED BY <1>2, <2>1, <3>4, <4>1, MintPriv DEF Priv
    <3>. QED BY <1>2, <2>0, <2>2, <3>1, <3>2, <3>3, <3>4
  <2>. QED BY <2>0, <2>1, <2>2 DEF Priv
<1>. QED BY <1>1, <1>2

LEMMA UploadedWrite ==
  ASSUME Uploaded, Pending, Fresh, Flight, Inv_CitationsLive, Minted, SnapMinted, NEW NC,
         w' \in [Writers -> Writer],
         upped \subseteq upped', DocEv(NC), GwEv, RetEv,
         NEW t, NEW R, Wr(t, R), TreeEv(t, R),
         \A u \in Writers, p \in Paths : (u # t /\ w[u].pc \in {"scanned", "claimed"} /\ p \in w[u].upDone)
                                          => w[u].snap[p] \notin NC,
         t \in Writers => (R.pc \in {"scanned", "claimed"} =>
           \A p \in R.upDone : R.snap[p][1] = p /\
             \/ (R.snap[p] = w[t].snap[p] /\ w[t].pc \in {"scanned", "claimed"} /\ p \in w[t].upDone /\ w[t].snap[p] \notin NC)
             \/ (R.snap[p] = w[t].snap[p] /\ w[t].pc \in {"scanned", "claimed"} /\ p \in w[t].uploads \ w[t].upDone
                 /\ w[t].snap[p] \notin upped /\ w[t].snap[p] \in upped' /\ w[t].snap[p] \notin NC)
             \/ (R.snap[p] # Nil /\ R.snap[p] \notin minted /\ R.snap[p] \in upped' /\ R.snap[p] \notin NC
                 /\ \A k \in Paths : gw'[k] # R.snap[p]))
  PROVE  Uploaded'
<1>. SUFFICES ASSUME NEW s \in Writers, w'[s].pc \in {"scanned", "claimed"}, NEW p \in w'[s].upDone
              PROVE  /\ w'[s].snap[p][1] = p
                     /\ w'[s].snap[p] # Nil /\ w'[s].snap[p] \in upped'
                     /\ ~(\E k \in Paths : doc'[k] = w'[s].snap[p]) /\ w'[s].snap[p] \notin retiring' \cup aged'
                     /\ \A q \in Paths : gw'[q] # w'[s].snap[p]
                     /\ \A u \in Writers \ {s}, q \in Paths : w'[u].local[q] # w'[s].snap[p] /\ w'[u].snap[q] # w'[s].snap[p]
  BY DEF Uploaded, Up, Priv, Cited
<1>0. p \in Paths BY WriterFields
<1>1. CASE s # t
  <2>1. w'[s] = w[s] BY <1>1 DEF Wr
  <2>2. w[s].snap[p][1] = p /\ Up(s, w[s].snap[p]) BY <2>1 DEF Uploaded
  <2>3. w[s].snap[p] \in minted /\ w[s].snap[p] \notin NC BY <1>0, <1>1, <2>1, <2>2 DEF SnapMinted, Opt, Up
  <2>. QED BY <2>1, <2>2, <2>3, UpKeep DEF Up, Priv, Cited
<1>2. CASE s = t
  <2>0. w'[s] = R BY <1>2 DEF Wr
  <2>1. R.snap[p][1] = p BY <1>2, <2>0
  <2>2. CASE R.snap[p] = w[t].snap[p] /\ w[t].pc \in {"scanned", "claimed"} /\ p \in w[t].upDone /\ w[t].snap[p] \notin NC
    <3>1. Up(s, w[s].snap[p]) /\ w[s].snap[p] \in minted BY <1>0, <1>2, <2>2 DEF Uploaded, Up, SnapMinted, Opt
    <3>. QED BY <1>2, <2>0, <2>1, <2>2, <3>1, UpKeep DEF Up, Priv, Cited
  <2>3. CASE R.snap[p] = w[t].snap[p] /\ w[t].pc \in {"scanned", "claimed"} /\ p \in w[t].uploads \ w[t].upDone
             /\ w[t].snap[p] \notin upped /\ w[t].snap[p] \in upped' /\ w[t].snap[p] \notin NC
    <3>1. w[s].snap[p] # Nil /\ w[s].snap[p][1] = p /\ Priv(s, w[s].snap[p]) BY <1>2, <2>3 DEF Pending
    <3>2. /\ ~Cited(w[s].snap[p]) /\ w[s].snap[p] \notin retiring \cup aged
          /\ \A q \in Paths : gw[q] # w[s].snap[p]
      BY <1>2, <2>3, <3>1, Unupped
    <3>3. w[s].snap[p] \in minted BY <1>0, <3>1 DEF SnapMinted, Opt
    <3>4. ~(\E k \in Paths : doc'[k] = w[s].snap[p]) BY <1>2, <2>3, <3>1, <3>2 DEF DocEv, CitedSet, Cited
    <3>5. w[s].snap[p] \notin retiring' \cup aged' BY <3>2 DEF RetEv, CitedSet, Cited
    <3>6. \A q \in Paths : gw'[q] # w[s].snap[p] BY <3>1, <3>2, <3>3 DEF GwEv
    <3>7. \A u \in Writers \ {s}, q \in Paths : w'[u].local[q] # w[s].snap[p] /\ w'[u].snap[q] # w[s].snap[p]
      BY <3>1, <3>2, <3>3, PrivKeep DEF Priv
    <3>8. w'[s].snap[p] = w[s].snap[p] BY <1>2, <2>0, <2>3
    <3>. QED BY <1>2, <2>1, <2>3, <3>1, <3>4, <3>5, <3>6, <3>7, <3>8
  <2>4. CASE R.snap[p] # Nil /\ R.snap[p] \notin minted /\ R.snap[p] \in upped' /\ R.snap[p] \notin NC /\ \A k \in Paths : gw'[k] # R.snap[p]
    <3>. DEFINE h == R.snap[p]
    <3>1. h # Nil BY <2>4
    <3>2. ~Cited(h)' BY <2>4, <3>1 DEF DocEv, CitedSet, Cited, Minted, Opt
    <3>3. h \notin retiring' \cup aged' BY <2>4 DEF RetEv, CitedSet, Cited, Fresh, Minted, Opt
    <3>4. \A u \in Writers : u # s => w'[u] = w[u] BY <1>2 DEF Wr
    <3>5. Priv(s, h)' BY <1>2, <2>4, <3>1, <3>4, MintPriv
    <3>. QED BY <1>2, <2>0, <2>1, <2>4, <3>1, <3>2, <3>3, <3>5 DEF Up, Priv, Cited
  <2>. QED BY <1>2, <2>0, <2>1, <2>2, <2>3, <2>4
<1>. QED BY <1>1, <1>2

LEMMA VerifiedWrite ==
  ASSUME Verified, Uploaded, Inv_OneHolder, LiveEv, NEW t, NEW R, Wr(t, R),
         t \in Writers => ((R.pc = "claimed" /\ R.verified) =>
                             \A p \in (R.uploads \cap R.upDone) \ R.gone : R.snap[p] \in live')
  PROVE  Verified'
<1>. SUFFICES ASSUME NEW s \in Writers, w'[s].pc = "claimed", w'[s].verified,
                     NEW p \in (w'[s].uploads \cap w'[s].upDone) \ w'[s].gone
              PROVE  w'[s].snap[p] \in live'
  BY DEF Verified
<1>1. CASE s = t BY <1>1 DEF Wr
<1>2. CASE s # t
  <2>1. w'[s] = w[s] BY <1>2 DEF Wr
  <2>2. w[s].snap[p] \in live BY <2>1 DEF Verified
  <2>3. w[s].snap[p] \notin aged BY <2>1 DEF Uploaded, Up
  <2>4. holder = s BY <2>1 DEF Inv_OneHolder
  <2>. QED BY <2>1, <2>2, <2>3, <2>4 DEF LiveEv
<1>. QED BY <1>1, <1>2

LEMMA UnverifiedWrite ==
  ASSUME Unverified, NEW t, NEW R, Wr(t, R), t \in Writers => (R.pc = "scanned" => ~R.verified)
  PROVE  Unverified'
BY DEF Unverified, Wr

LEMMA CLWrite ==
  ASSUME Inv_CitationsLive, NEW NC, DocEv(NC), \A h \in NC : h \in live', LiveEv
  PROVE  Inv_CitationsLive'
BY DEF Inv_CitationsLive, DocEv, LiveEv, CitedSet, Cited

\* M2 after a step that moves nothing M2 reads.
LEMMA M2Same ==
  ASSUME M2, UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped, retiring, aged, w>>
  PROVE  M2'
BY DEF M2, Fresh, SnapMinted, Flight, Private, Pending, Uploaded, Verified, Unverified,
       Inv_CitationsLive, Inv_OneName, Priv, Up, Cited

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M2 == Init => IndM2
<1>. SUFFICES ASSUME Init PROVE IndM2 OBVIOUS
<1>1. IndM1 BY Init_M1
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>3. \A s \in Writers : /\ w[s].pc = "idle" /\ w[s].verified = FALSE
                         /\ w[s].local = [p \in Paths |-> Nil] /\ w[s].snap = [p \in Paths |-> Nil]
  BY <1>2 DEF WriterInit
<1>4. Seed \in Gens /\ \A p \in Paths : <<p, Seed>> \in Handles /\ Gen(<<p, Seed>>) = Seed
  BY MaxMintNat, MaxCopiesNat DEF Seed, Gens, Handles, Gen
<1>5. Fresh BY <1>4, MaxMintNat, MaxCopiesNat DEF Init, Fresh, Seed
<1>6. SnapMinted /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
  BY <1>3 DEF Init, SnapMinted, Private, Pending, Uploaded, Verified, Unverified, Opt
<1>7. Flight BY DEF Init, Flight
<1>8. Inv_CitationsLive /\ Inv_OneName BY DEF Init, Inv_CitationsLive, Inv_OneName
<1>. QED BY <1>1, <1>5, <1>6, <1>7, <1>8 DEF IndM2, M2

------------------------------------------------------------------------------
(* The gateway.                                                             *)

LEMMA GPut_M2 ==
  ASSUME IndM2, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, GPut_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>. DEFINE h == <<p, nextGen>>
<1>1. /\ nextGen <= MaxMint
      /\ live' = live \cup {h} /\ minted' = minted \cup {h} /\ gw' = [gw EXCEPT ![p] = h]
      /\ nextGen' = nextGen + 1 /\ upped' = upped \cup {h}
      /\ UNCHANGED <<doc, copies, w>>
  BY DEF GPut
<1>2. h \in Handles /\ h # Nil /\ h[1] = p BY <1>a, <1>1, MintHandle, NilHandle DEF IndTypeOK
<1>4. Fresh' /\ h \notin minted BY <1>c, <1>0, <1>1, <1>r, FreshMint DEF Fresh
<1>5. SnapMinted' BY <1>c, <1>1 DEF SnapMinted, Opt
<1>6. Flight' BY <1>c, <1>d, <1>0, <1>1, <1>2, <1>4, <1>r, MintFlight
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv("none", w) /\ Wr("none", w) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>4, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv
<1>7. Private' BY <1>c, <1>d, <1>e, NoneWriter, PrivateWrite
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>e, NoneWriter, PendingWrite
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>e, <1>r, NoneWriter, UploadedWrite
<1>10. Verified' BY <1>c, <1>m, <1>e, NoneWriter, VerifiedWrite
<1>11. Unverified' BY <1>c, <1>1 DEF Unverified
<1>12. Inv_CitationsLive' BY <1>c, <1>1 DEF Inv_CitationsLive
<1>13. Inv_OneName' BY <1>c, <1>1 DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA GCas_M2 ==
  ASSUME IndM2, NEW p \in Paths, GCas(p), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, GCas_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>1. /\ gw[p] # Nil
      /\ doc' \in {[doc EXCEPT ![p] = gw[p]], doc}
      /\ gw' = [gw EXCEPT ![p] = Nil]
      /\ UNCHANGED <<live, minted, nextGen, copies, upped, w>>
  BY DEF GCas, aux
<1>2. gw[p] \in live /\ ~Cited(gw[p]) /\ gw[p][1] = p /\ gw[p] \in minted BY <1>c, <1>d, <1>1 DEF Flight, Minted, Opt
<1>e. /\ DocEv({gw[p]}) /\ GwEv /\ TreeEv("none", w) /\ Wr("none", w) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>5. SnapMinted' BY <1>c, <1>1 DEF SnapMinted
<1>6a. \A q \in Paths : gw'[q] # Nil => gw'[q] = gw[q] /\ gw[q] \notin {gw[p]} BY <1>c, <1>0, <1>1 DEF Flight
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, FlightKeep
<1>7. Private' BY <1>c, <1>1, <1>e DEF Private, Priv, TreeEv, Wr
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>e, NoneWriter, PendingWrite
<1>9a. \A u \in Writers, q \in Paths : (u # "none" /\ w[u].pc \in {"scanned", "claimed"} /\ q \in w[u].upDone) => w[u].snap[q] \notin {gw[p]}
  BY <1>c DEF Uploaded, Up
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>e, <1>r, <1>9a, NoneWriter, UploadedWrite
<1>10. Verified' BY <1>c, <1>m, <1>e, NoneWriter, VerifiedWrite
<1>11. Unverified' BY <1>c, <1>1 DEF Unverified
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>1, <1>2, CLWrite
<1>13. Inv_OneName' BY <1>c, <1>0, <1>1, <1>2 DEF Inv_OneName, Cited
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA GRename_M2 ==
  ASSUME IndM2, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, GRename_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>0r. RenameAtomic BY ShippedShape DEF Shipped
<1>1a. /\ p # q /\ doc[p] # Nil /\ doc[q] = Nil
       /\ doc' = [doc EXCEPT ![q] = doc[p], ![p] = Nil]
       /\ tomb' = [tomb EXCEPT ![q] = Nil, ![p] = doc[p]]
       /\ acked' = acked \cup {<<q, doc[p]>>}
       /\ mv' = mv
       /\ seq' = seq + 1 /\ reqs' = reqs + 1
       /\ UNCHANGED <<live, minted, base, conflicts, holder, gw, udel, nextGen, ui, barriers, w, aux>>
  BY <1>0r DEF GRename
<1>1. /\ p # q /\ doc[p] # Nil /\ doc[q] = Nil
      /\ doc' = [doc EXCEPT ![q] = doc[p], ![p] = Nil]
      /\ UNCHANGED <<live, minted, gw, nextGen, copies, upped, w>>
  BY <1>1a DEF aux
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv("none", w) /\ Wr("none", w) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv, CitedSet
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>5. SnapMinted' BY <1>c, <1>1 DEF SnapMinted
<1>6a. \A k \in Paths : gw'[k] # Nil => gw'[k] = gw[k] /\ gw[k] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, FlightKeep
<1>7. Private' BY <1>c, <1>1, <1>e DEF Private, Priv, TreeEv, Wr
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>e, NoneWriter, PendingWrite
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>e, <1>r, NoneWriter, UploadedWrite
<1>10. Verified' BY <1>c, <1>m, <1>e, NoneWriter, VerifiedWrite
<1>11. Unverified' BY <1>c, <1>1 DEF Unverified
<1>12. Inv_CitationsLive' BY <1>c, <1>e, CLWrite
<1>13. Inv_OneName' BY <1>c, <1>0, <1>1 DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

\* Never enabled (mv = Nil).
LEMMA GRenameFinish_M2 ==
  ASSUME IndM2, GRenameFinish, Frame
  PROVE  IndM2'
BY DEF IndM2, IndM1, IndTypeOK, GRenameFinish

LEMMA GDelete_M2 ==
  ASSUME IndM2, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, GDelete_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>1. /\ doc[p] # Nil /\ doc' = [doc EXCEPT ![p] = Nil]
      /\ UNCHANGED <<live, minted, gw, nextGen, copies, upped, w>>
  BY DEF GDelete, aux
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv("none", w) /\ Wr("none", w) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>5. SnapMinted' BY <1>c, <1>1 DEF SnapMinted
<1>6a. \A k \in Paths : gw'[k] # Nil => gw'[k] = gw[k] /\ gw[k] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, FlightKeep
<1>7. Private' BY <1>c, <1>1, <1>e DEF Private, Priv, TreeEv, Wr
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>e, NoneWriter, PendingWrite
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>e, <1>r, NoneWriter, UploadedWrite
<1>10. Verified' BY <1>c, <1>m, <1>e, NoneWriter, VerifiedWrite
<1>11. Unverified' BY <1>c, <1>1 DEF Unverified
<1>12. Inv_CitationsLive' BY <1>c, <1>e, CLWrite
<1>13. Inv_OneName' BY <1>c, <1>0, <1>1 DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Sweep_M2 ==
  ASSUME IndM2, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Sweep_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>1. /\ h \in live /\ ~Cited(h)
      /\ (SweepUnderLease => (holder = s /\ w[s].pc \in {"claimed", "cased"}))
      /\ (GatewaySweepGrace => ~InFlight(h))
      /\ ~\E q \in w[s].upDone : w[s].snap[q] = h
      /\ live' = live \ {h}
      /\ UNCHANGED <<minted, doc, gw, nextGen, copies, upped, w>>
  BY DEF Sweep, aux
\* The two rules the sweep's events rest on (plan section 5: each a control).
<1>1h. holder = s BY <1>1, <1>s
<1>1i. ~InFlight(h) BY <1>1, <1>s
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv("none", w) /\ Wr("none", w) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>1h, <1>1i, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv, InFlight
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>5. SnapMinted' BY <1>c, <1>1 DEF SnapMinted
<1>6a. \A k \in Paths : gw'[k] # Nil => gw'[k] = gw[k] /\ gw[k] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, FlightKeep
<1>7. Private' BY <1>c, <1>1, <1>e DEF Private, Priv, TreeEv, Wr
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>e, NoneWriter, PendingWrite
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>e, <1>r, NoneWriter, UploadedWrite
<1>10. Verified' BY <1>c, <1>m, <1>e, NoneWriter, VerifiedWrite
<1>11. Unverified' BY <1>c, <1>1 DEF Unverified
<1>12. Inv_CitationsLive' BY <1>c, <1>e, CLWrite
<1>13. Inv_OneName' BY <1>c, <1>1 DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Reap_M2 ==
  ASSUME IndM2, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>t. IndM1' BY <1>a, Reap_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>1. /\ h \in aged /\ h \in live /\ ~Cited(h)
      /\ live' = live \ {h} /\ aged' = aged \ {h}
      /\ UNCHANGED <<minted, doc, gw, nextGen, copies, upped, w, retiring>>
  BY DEF Reap, aux
<1>r. RetEv BY <1>1 DEF RetEv
<1>1i. \A k \in Paths : gw[k] # h BY <1>c, <1>1, NilHandle DEF Flight
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv("none", w) /\ Wr("none", w) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>1i, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>5. SnapMinted' BY <1>c, <1>1 DEF SnapMinted
<1>6a. \A k \in Paths : gw'[k] # Nil => gw'[k] = gw[k] /\ gw[k] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, FlightKeep
<1>7. Private' BY <1>c, <1>1, <1>e DEF Private, Priv, TreeEv, Wr
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>e, NoneWriter, PendingWrite
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>e, <1>r, NoneWriter, UploadedWrite
<1>10. Verified' BY <1>c, <1>m, <1>e, NoneWriter, VerifiedWrite
<1>11. Unverified' BY <1>c, <1>1 DEF Unverified
<1>12. Inv_CitationsLive' BY <1>c, <1>e, CLWrite
<1>13. Inv_OneName' BY <1>c, <1>1 DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Age_M2 ==
  ASSUME IndM2, Age, UNCHANGED anc
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>t. IndM1' BY <1>a, Age_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>1. /\ aged' = aged \cup retiring /\ retiring' = {}
      /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped, w>>
  BY DEF Age, aux
<1>r. RetEv BY <1>1 DEF RetEv
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv("none", w) /\ Wr("none", w) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>5. SnapMinted' BY <1>c, <1>1 DEF SnapMinted
<1>6a. \A k \in Paths : gw'[k] # Nil => gw'[k] = gw[k] /\ gw[k] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, FlightKeep
<1>7. Private' BY <1>c, <1>1, <1>e DEF Private, Priv, TreeEv, Wr
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>e, NoneWriter, PendingWrite
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>e, <1>r, NoneWriter, UploadedWrite
<1>10. Verified' BY <1>c, <1>m, <1>e, NoneWriter, VerifiedWrite
<1>11. Unverified' BY <1>c, <1>1 DEF Unverified
<1>12. Inv_CitationsLive' BY <1>c, <1>e, CLWrite
<1>13. Inv_OneName' BY <1>c, <1>1 DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA RLoad_M2 ==
  ASSUME IndM2, RLoad, UNCHANGED anc
  PROVE  IndM2'
<1>1. IndM1' BY RLoad_M1 DEF IndM2
<1>2. M2' BY M2Same DEF IndM2, RLoad, aux
<1>. QED BY <1>1, <1>2 DEF IndM2

------------------------------------------------------------------------------
(* The agent.                                                               *)

LEMMA Edit_M2 ==
  ASSUME IndM2, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Edit_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. /\ nextGen <= MaxMint /\ minted' = minted \cup {<<p, nextGen>>} /\ nextGen' = nextGen + 1
      /\ w' = [w EXCEPT ![s] = EditW(s, p)] /\ UNCHANGED <<live, doc, gw, copies, upped>>
  BY DEF Edit, aux
<1>g. On(s) BY DEF Edit
<1>2. Wr(s, EditW(s, p)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ EditW(s, p).pc = w[s].pc
      /\ EditW(s, p).uploads = w[s].uploads
      /\ EditW(s, p).upDone = w[s].upDone
      /\ EditW(s, p).gone = w[s].gone
      /\ EditW(s, p).verified = w[s].verified
      /\ EditW(s, p).local = [w[s].local EXCEPT ![p] = <<p, nextGen>>]
      /\ EditW(s, p).snap = w[s].snap
  BY DEF EditW
<1>4. Fresh' /\ <<p, nextGen>> \notin minted BY <1>c, <1>0, <1>1, <1>r, FreshMint DEF Fresh
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, EditW(s, p)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>4, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : EditW(s, p).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (EditW(s, p).local[pp] # Nil /\ EditW(s, p).local[pp] \notin upped') =>
          \/ EditW(s, p).local[pp] = w[s].local[pp]
          \/ (EditW(s, p).local[pp][1] = pp /\ EditW(s, p).local[pp] \notin minted)
  BY <1>3, <1>4, <1>f
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. EditW(s, p).pc \in {"scanned", "claimed"} =>
          \A pp \in EditW(s, p).uploads \ EditW(s, p).upDone :
            /\ EditW(s, p).snap[pp] # Nil
            /\ (EditW(s, p).snap[pp] \notin upped' =>
                  \/ (EditW(s, p).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ EditW(s, p).snap[pp] = w[s].local[pp]
                  \/ (EditW(s, p).snap[pp][1] = pp /\ EditW(s, p).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. EditW(s, p).pc \in {"scanned", "claimed"} =>
          \A pp \in EditW(s, p).upDone : EditW(s, p).snap[pp][1] = pp /\
            \/ (EditW(s, p).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (EditW(s, p).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (EditW(s, p).snap[pp] # Nil /\ EditW(s, p).snap[pp] \notin minted /\ EditW(s, p).snap[pp] \in upped' /\ EditW(s, p).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # EditW(s, p).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (EditW(s, p).pc = "claimed" /\ EditW(s, p).verified) => \A pp \in (EditW(s, p).uploads \cap EditW(s, p).upDone) \ EditW(s, p).gone : EditW(s, p).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. EditW(s, p).pc = "scanned" => ~EditW(s, p).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Delete_M2 ==
  ASSUME IndM2, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Delete_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Delete, bucket, aux
<1>g. On(s) BY DEF Delete
<1>2. Wr(s, DeleteW(s, p)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ DeleteW(s, p).pc = w[s].pc
      /\ DeleteW(s, p).uploads = w[s].uploads
      /\ DeleteW(s, p).upDone = w[s].upDone
      /\ DeleteW(s, p).gone = w[s].gone
      /\ DeleteW(s, p).verified = w[s].verified
      /\ DeleteW(s, p).local = [w[s].local EXCEPT ![p] = Nil]
      /\ DeleteW(s, p).snap = w[s].snap
  BY DEF DeleteW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, DeleteW(s, p)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : DeleteW(s, p).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (DeleteW(s, p).local[pp] # Nil /\ DeleteW(s, p).local[pp] \notin upped') =>
          \/ DeleteW(s, p).local[pp] = w[s].local[pp]
          \/ (DeleteW(s, p).local[pp][1] = pp /\ DeleteW(s, p).local[pp] \notin minted)
  BY <1>3, <1>f
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. DeleteW(s, p).pc \in {"scanned", "claimed"} =>
          \A pp \in DeleteW(s, p).uploads \ DeleteW(s, p).upDone :
            /\ DeleteW(s, p).snap[pp] # Nil
            /\ (DeleteW(s, p).snap[pp] \notin upped' =>
                  \/ (DeleteW(s, p).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ DeleteW(s, p).snap[pp] = w[s].local[pp]
                  \/ (DeleteW(s, p).snap[pp][1] = pp /\ DeleteW(s, p).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. DeleteW(s, p).pc \in {"scanned", "claimed"} =>
          \A pp \in DeleteW(s, p).upDone : DeleteW(s, p).snap[pp][1] = pp /\
            \/ (DeleteW(s, p).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (DeleteW(s, p).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (DeleteW(s, p).snap[pp] # Nil /\ DeleteW(s, p).snap[pp] \notin minted /\ DeleteW(s, p).snap[pp] \in upped' /\ DeleteW(s, p).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # DeleteW(s, p).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (DeleteW(s, p).pc = "claimed" /\ DeleteW(s, p).verified) => \A pp \in (DeleteW(s, p).uploads \cap DeleteW(s, p).upDone) \ DeleteW(s, p).gone : DeleteW(s, p).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. DeleteW(s, p).pc = "scanned" => ~DeleteW(s, p).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Checkout_M2 ==
  ASSUME IndM2, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Checkout_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Checkout, bucket, aux
<1>g. w[s].st = "off" BY DEF Checkout
<1>2. Wr(s, CheckoutW(s, T)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ CheckoutW(s, T).pc = w[s].pc
      /\ CheckoutW(s, T).uploads = w[s].uploads
      /\ CheckoutW(s, T).upDone = w[s].upDone
      /\ CheckoutW(s, T).gone = w[s].gone
      /\ CheckoutW(s, T).verified = w[s].verified
      /\ CheckoutW(s, T).local = CheckoutHeld(s, T)
      /\ CheckoutW(s, T).snap = w[s].snap
  BY DEF CheckoutW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, CheckoutW(s, T)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CheckoutHeld, CitedSet
<1>5a. \A q \in Paths : CheckoutW(s, T).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (CheckoutW(s, T).local[pp] # Nil /\ CheckoutW(s, T).local[pp] \notin upped') =>
          \/ CheckoutW(s, T).local[pp] = w[s].local[pp]
          \/ (CheckoutW(s, T).local[pp][1] = pp /\ CheckoutW(s, T).local[pp] \notin minted)
  BY <1>3, <1>e, <1>c, <1>f, <1>0 DEF CheckoutHeld, Inv_CitationsLive, Fresh
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. CheckoutW(s, T).pc \in {"scanned", "claimed"} =>
          \A pp \in CheckoutW(s, T).uploads \ CheckoutW(s, T).upDone :
            /\ CheckoutW(s, T).snap[pp] # Nil
            /\ (CheckoutW(s, T).snap[pp] \notin upped' =>
                  \/ (CheckoutW(s, T).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ CheckoutW(s, T).snap[pp] = w[s].local[pp]
                  \/ (CheckoutW(s, T).snap[pp][1] = pp /\ CheckoutW(s, T).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. CheckoutW(s, T).pc \in {"scanned", "claimed"} =>
          \A pp \in CheckoutW(s, T).upDone : CheckoutW(s, T).snap[pp][1] = pp /\
            \/ (CheckoutW(s, T).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (CheckoutW(s, T).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (CheckoutW(s, T).snap[pp] # Nil /\ CheckoutW(s, T).snap[pp] \notin minted /\ CheckoutW(s, T).snap[pp] \in upped' /\ CheckoutW(s, T).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # CheckoutW(s, T).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (CheckoutW(s, T).pc = "claimed" /\ CheckoutW(s, T).verified) => \A pp \in (CheckoutW(s, T).uploads \cap CheckoutW(s, T).upDone) \ CheckoutW(s, T).gone : CheckoutW(s, T).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. CheckoutW(s, T).pc = "scanned" => ~CheckoutW(s, T).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Consume_M2 ==
  ASSUME IndM2, NEW s \in Writers, Consume(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Consume_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <1>1 DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" BY DEF Consume
  <2>2. Wr(s, ConsumeCheapW(s)) BY <1>a, <2>1, WriteAny DEF Wr
  <2>3. /\ ConsumeCheapW(s).pc = "consumed"
        /\ ConsumeCheapW(s).uploads = w[s].uploads
        /\ ConsumeCheapW(s).upDone = w[s].upDone
        /\ ConsumeCheapW(s).gone = w[s].gone
        /\ ConsumeCheapW(s).verified = w[s].verified
        /\ ConsumeCheapW(s).local = w[s].local
        /\ ConsumeCheapW(s).snap = w[s].snap
    BY DEF ConsumeCheapW
  <2>4. Fresh' BY <1>c, <2>1, <1>r, FreshKeep
  <2>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, ConsumeCheapW(s)) /\ LiveEv
        /\ minted \subseteq minted' /\ upped \subseteq upped'
    BY <2>1, <2>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
  <2>5a. \A q \in Paths : ConsumeCheapW(s).snap[q] \in Opt(minted') BY <2>1, <2>3, <1>c, <1>d, <2>e, <1>f DEF SnapMinted, Minted, Opt
  <2>5. SnapMinted' BY <1>c, <2>2, <2>e, <2>5a, SnapMintedWrite
  <2>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <2>1
  <2>6. Flight' BY <1>c, <1>d, <2>6a, <2>e, <1>r, <2>2, FlightKeep
  <2>7a. \A pp \in Paths : (ConsumeCheapW(s).local[pp] # Nil /\ ConsumeCheapW(s).local[pp] \notin upped') =>
            \/ ConsumeCheapW(s).local[pp] = w[s].local[pp]
            \/ (ConsumeCheapW(s).local[pp][1] = pp /\ ConsumeCheapW(s).local[pp] \notin minted)
    BY <2>3
  <2>7. Private' BY <1>c, <1>d, <2>2, <2>e, <2>7a, PrivateWrite
  <2>8a. ConsumeCheapW(s).pc \in {"scanned", "claimed"} =>
            \A pp \in ConsumeCheapW(s).uploads \ ConsumeCheapW(s).upDone :
              /\ ConsumeCheapW(s).snap[pp] # Nil
              /\ (ConsumeCheapW(s).snap[pp] \notin upped' =>
                    \/ (ConsumeCheapW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                    \/ ConsumeCheapW(s).snap[pp] = w[s].local[pp]
                    \/ (ConsumeCheapW(s).snap[pp][1] = pp /\ ConsumeCheapW(s).snap[pp] \notin minted))
    BY <2>3, <2>g, <1>c DEF Pending
  <2>8. Pending' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <2>8a, PendingWrite
  <2>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
    OBVIOUS
  <2>9b. ConsumeCheapW(s).pc \in {"scanned", "claimed"} =>
            \A pp \in ConsumeCheapW(s).upDone : ConsumeCheapW(s).snap[pp][1] = pp /\
              \/ (ConsumeCheapW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
              \/ (ConsumeCheapW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                  /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
              \/ (ConsumeCheapW(s).snap[pp] # Nil /\ ConsumeCheapW(s).snap[pp] \notin minted /\ ConsumeCheapW(s).snap[pp] \in upped' /\ ConsumeCheapW(s).snap[pp] \notin {}
                  /\ \A k \in Paths : gw'[k] # ConsumeCheapW(s).snap[pp])
    BY <2>3, <2>g, <1>c DEF Uploaded
  <2>9. Uploaded' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <1>r, <2>9a, <2>9b, UploadedWrite
  <2>10a. (ConsumeCheapW(s).pc = "claimed" /\ ConsumeCheapW(s).verified) => \A pp \in (ConsumeCheapW(s).uploads \cap ConsumeCheapW(s).upDone) \ ConsumeCheapW(s).gone : ConsumeCheapW(s).snap[pp] \in live'
    BY <2>1, <2>3, <2>g, <1>c DEF Verified
  <2>10. Verified' BY <1>c, <1>m, <2>2, <2>e, <2>10a, VerifiedWrite
  <2>11a. ConsumeCheapW(s).pc = "scanned" => ~ConsumeCheapW(s).verified BY <2>3, <2>g, <1>c DEF Unverified
  <2>11. Unverified' BY <1>c, <2>2, <2>11a, UnverifiedWrite
  <2>12a. \A h \in {} : h \in live' OBVIOUS
  <2>12. Inv_CitationsLive' BY <1>c, <2>e, <2>12a, CLWrite
  <2>13. Inv_OneName' BY <2>1, <1>c DEF Inv_OneName
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13 DEF IndM2, M2
<1>2. CASE ~CheapPath(s)
  <2>1. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <1>2 DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" BY DEF Consume
  <2>2. Wr(s, ConsumeW(s, fail)) BY <1>a, <2>1, WriteAny DEF Wr
  <2>3. /\ ConsumeW(s, fail).pc = "consumed"
        /\ ConsumeW(s, fail).uploads = w[s].uploads
        /\ ConsumeW(s, fail).upDone = w[s].upDone
        /\ ConsumeW(s, fail).gone = w[s].gone
        /\ ConsumeW(s, fail).verified = w[s].verified
        /\ ConsumeW(s, fail).local = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].local[q]]
        /\ ConsumeW(s, fail).snap = w[s].snap
    BY DEF ConsumeW
  <2>4. Fresh' BY <1>c, <2>1, <1>r, FreshKeep
  <2>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, ConsumeW(s, fail)) /\ LiveEv
        /\ minted \subseteq minted' /\ upped \subseteq upped'
    BY <2>1, <2>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet
  <2>5a. \A q \in Paths : ConsumeW(s, fail).snap[q] \in Opt(minted') BY <2>1, <2>3, <1>c, <1>d, <2>e, <1>f DEF SnapMinted, Minted, Opt
  <2>5. SnapMinted' BY <1>c, <2>2, <2>e, <2>5a, SnapMintedWrite
  <2>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <2>1
  <2>6. Flight' BY <1>c, <1>d, <2>6a, <2>e, <1>r, <2>2, FlightKeep
  <2>7a. \A pp \in Paths : (ConsumeW(s, fail).local[pp] # Nil /\ ConsumeW(s, fail).local[pp] \notin upped') =>
            \/ ConsumeW(s, fail).local[pp] = w[s].local[pp]
            \/ (ConsumeW(s, fail).local[pp][1] = pp /\ ConsumeW(s, fail).local[pp] \notin minted)
    BY <2>3, <2>e, <1>c, <1>f, <1>0 DEF Inv_CitationsLive, Fresh
  <2>7. Private' BY <1>c, <1>d, <2>2, <2>e, <2>7a, PrivateWrite
  <2>8a. ConsumeW(s, fail).pc \in {"scanned", "claimed"} =>
            \A pp \in ConsumeW(s, fail).uploads \ ConsumeW(s, fail).upDone :
              /\ ConsumeW(s, fail).snap[pp] # Nil
              /\ (ConsumeW(s, fail).snap[pp] \notin upped' =>
                    \/ (ConsumeW(s, fail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                    \/ ConsumeW(s, fail).snap[pp] = w[s].local[pp]
                    \/ (ConsumeW(s, fail).snap[pp][1] = pp /\ ConsumeW(s, fail).snap[pp] \notin minted))
    BY <2>3, <2>g, <1>c DEF Pending
  <2>8. Pending' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <2>8a, PendingWrite
  <2>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
    OBVIOUS
  <2>9b. ConsumeW(s, fail).pc \in {"scanned", "claimed"} =>
            \A pp \in ConsumeW(s, fail).upDone : ConsumeW(s, fail).snap[pp][1] = pp /\
              \/ (ConsumeW(s, fail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
              \/ (ConsumeW(s, fail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                  /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
              \/ (ConsumeW(s, fail).snap[pp] # Nil /\ ConsumeW(s, fail).snap[pp] \notin minted /\ ConsumeW(s, fail).snap[pp] \in upped' /\ ConsumeW(s, fail).snap[pp] \notin {}
                  /\ \A k \in Paths : gw'[k] # ConsumeW(s, fail).snap[pp])
    BY <2>3, <2>g, <1>c DEF Uploaded
  <2>9. Uploaded' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <1>r, <2>9a, <2>9b, UploadedWrite
  <2>10a. (ConsumeW(s, fail).pc = "claimed" /\ ConsumeW(s, fail).verified) => \A pp \in (ConsumeW(s, fail).uploads \cap ConsumeW(s, fail).upDone) \ ConsumeW(s, fail).gone : ConsumeW(s, fail).snap[pp] \in live'
    BY <2>1, <2>3, <2>g, <1>c DEF Verified
  <2>10. Verified' BY <1>c, <1>m, <2>2, <2>e, <2>10a, VerifiedWrite
  <2>11a. ConsumeW(s, fail).pc = "scanned" => ~ConsumeW(s, fail).verified BY <2>3, <2>g, <1>c DEF Unverified
  <2>11. Unverified' BY <1>c, <2>2, <2>11a, UnverifiedWrite
  <2>12a. \A h \in {} : h \in live' OBVIOUS
  <2>12. Inv_CitationsLive' BY <1>c, <2>e, <2>12a, CLWrite
  <2>13. Inv_OneName' BY <2>1, <1>c DEF Inv_OneName
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13 DEF IndM2, M2
<1>. QED BY <1>1, <1>2

LEMMA Scan_M2 ==
  ASSUME IndM2, NEW s \in Writers, Scan(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Scan_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. PICK dels \in SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Scan, bucket, aux
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Scan
<1>2. Wr(s, ScanW(s, dels)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ ScanW(s, dels).pc = "scanned"
      /\ ScanW(s, dels).uploads = ScanUps(s)
      /\ ScanW(s, dels).upDone = {}
      /\ ScanW(s, dels).gone = {}
      /\ ScanW(s, dels).verified = FALSE
      /\ ScanW(s, dels).local = w[s].local
      /\ ScanW(s, dels).snap = w[s].local
  BY DEF ScanW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, ScanW(s, dels)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : ScanW(s, dels).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (ScanW(s, dels).local[pp] # Nil /\ ScanW(s, dels).local[pp] \notin upped') =>
          \/ ScanW(s, dels).local[pp] = w[s].local[pp]
          \/ (ScanW(s, dels).local[pp][1] = pp /\ ScanW(s, dels).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. ScanW(s, dels).pc \in {"scanned", "claimed"} =>
          \A pp \in ScanW(s, dels).uploads \ ScanW(s, dels).upDone :
            /\ ScanW(s, dels).snap[pp] # Nil
            /\ (ScanW(s, dels).snap[pp] \notin upped' =>
                  \/ (ScanW(s, dels).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ ScanW(s, dels).snap[pp] = w[s].local[pp]
                  \/ (ScanW(s, dels).snap[pp][1] = pp /\ ScanW(s, dels).snap[pp] \notin minted))
  BY <1>3 DEF ScanUps, ScanDirty
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. ScanW(s, dels).pc \in {"scanned", "claimed"} =>
          \A pp \in ScanW(s, dels).upDone : ScanW(s, dels).snap[pp][1] = pp /\
            \/ (ScanW(s, dels).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (ScanW(s, dels).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (ScanW(s, dels).snap[pp] # Nil /\ ScanW(s, dels).snap[pp] \notin minted /\ ScanW(s, dels).snap[pp] \in upped' /\ ScanW(s, dels).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # ScanW(s, dels).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (ScanW(s, dels).pc = "claimed" /\ ScanW(s, dels).verified) => \A pp \in (ScanW(s, dels).uploads \cap ScanW(s, dels).upDone) \ ScanW(s, dels).gone : ScanW(s, dels).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. ScanW(s, dels).pc = "scanned" => ~ScanW(s, dels).verified BY <1>3
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Skip_M2 ==
  ASSUME IndM2, NEW s \in Writers, Skip(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Skip_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Skip, bucket, aux
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Skip
<1>2. Wr(s, SkipW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ SkipW(s).pc = "idle"
      /\ SkipW(s).uploads = w[s].uploads
      /\ SkipW(s).upDone = w[s].upDone
      /\ SkipW(s).gone = w[s].gone
      /\ SkipW(s).verified = w[s].verified
      /\ SkipW(s).local = w[s].local
      /\ SkipW(s).snap = w[s].snap
  BY DEF SkipW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, SkipW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : SkipW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (SkipW(s).local[pp] # Nil /\ SkipW(s).local[pp] \notin upped') =>
          \/ SkipW(s).local[pp] = w[s].local[pp]
          \/ (SkipW(s).local[pp][1] = pp /\ SkipW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. SkipW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in SkipW(s).uploads \ SkipW(s).upDone :
            /\ SkipW(s).snap[pp] # Nil
            /\ (SkipW(s).snap[pp] \notin upped' =>
                  \/ (SkipW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ SkipW(s).snap[pp] = w[s].local[pp]
                  \/ (SkipW(s).snap[pp][1] = pp /\ SkipW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. SkipW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in SkipW(s).upDone : SkipW(s).snap[pp][1] = pp /\
            \/ (SkipW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (SkipW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (SkipW(s).snap[pp] # Nil /\ SkipW(s).snap[pp] \notin minted /\ SkipW(s).snap[pp] \in upped' /\ SkipW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # SkipW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (SkipW(s).pc = "claimed" /\ SkipW(s).verified) => \A pp \in (SkipW(s).uploads \cap SkipW(s).upDone) \ SkipW(s).gone : SkipW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. SkipW(s).pc = "scanned" => ~SkipW(s).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Upload_M2 ==
  ASSUME IndM2, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Upload_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>g. On(s) /\ w[s].pc = "scanned" /\ p \in w[s].uploads \ w[s].upDone BY DEF Upload
<1>h. w[s].snap[p] # Nil /\ w[s].snap[p] \in minted BY <1>g, <1>c DEF Pending, SnapMinted, Opt
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. /\ live' = live \cup {w[s].snap[p]} /\ upped' = upped \cup {w[s].snap[p]}
        /\ w' = [w EXCEPT ![s] = UploadW(s, p)] /\ UNCHANGED <<minted, doc, gw, nextGen, copies>>
    BY <1>1 DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" /\ p \in w[s].uploads \ w[s].upDone BY DEF Upload
  <2>2. Wr(s, UploadW(s, p)) BY <1>a, <2>1, WriteAny DEF Wr
  <2>3. /\ UploadW(s, p).pc = w[s].pc
        /\ UploadW(s, p).uploads = w[s].uploads
        /\ UploadW(s, p).upDone = w[s].upDone \cup {p}
        /\ UploadW(s, p).gone = w[s].gone
        /\ UploadW(s, p).verified = w[s].verified
        /\ UploadW(s, p).local = w[s].local
        /\ UploadW(s, p).snap = w[s].snap
    BY DEF UploadW
  <2>4. Fresh' BY <1>c, <1>h, <2>1, <1>r, FreshPut
  <2>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, UploadW(s, p)) /\ LiveEv
        /\ minted \subseteq minted' /\ upped \subseteq upped'
    BY <2>1, <2>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
  <2>5a. \A q \in Paths : UploadW(s, p).snap[q] \in Opt(minted') BY <2>1, <2>3, <1>c, <1>d, <2>e, <1>f DEF SnapMinted, Minted, Opt
  <2>5. SnapMinted' BY <1>c, <2>2, <2>e, <2>5a, SnapMintedWrite
  <2>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <2>1
  <2>6. Flight' BY <1>c, <1>d, <2>6a, <2>e, <1>r, <2>2, FlightKeep
  <2>7a. \A pp \in Paths : (UploadW(s, p).local[pp] # Nil /\ UploadW(s, p).local[pp] \notin upped') =>
            \/ UploadW(s, p).local[pp] = w[s].local[pp]
            \/ (UploadW(s, p).local[pp][1] = pp /\ UploadW(s, p).local[pp] \notin minted)
    BY <2>3
  <2>7. Private' BY <1>c, <1>d, <2>2, <2>e, <2>7a, PrivateWrite
  <2>8a. UploadW(s, p).pc \in {"scanned", "claimed"} =>
            \A pp \in UploadW(s, p).uploads \ UploadW(s, p).upDone :
              /\ UploadW(s, p).snap[pp] # Nil
              /\ (UploadW(s, p).snap[pp] \notin upped' =>
                    \/ (UploadW(s, p).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                    \/ UploadW(s, p).snap[pp] = w[s].local[pp]
                    \/ (UploadW(s, p).snap[pp][1] = pp /\ UploadW(s, p).snap[pp] \notin minted))
    BY <2>3, <1>g, <1>c DEF Pending
  <2>8. Pending' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <2>8a, PendingWrite
  <2>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
    OBVIOUS
  <2>9b. UploadW(s, p).pc \in {"scanned", "claimed"} =>
            \A pp \in UploadW(s, p).upDone : UploadW(s, p).snap[pp][1] = pp /\
              \/ (UploadW(s, p).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
              \/ (UploadW(s, p).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                  /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
              \/ (UploadW(s, p).snap[pp] # Nil /\ UploadW(s, p).snap[pp] \notin minted /\ UploadW(s, p).snap[pp] \in upped' /\ UploadW(s, p).snap[pp] \notin {}
                  /\ \A k \in Paths : gw'[k] # UploadW(s, p).snap[pp])
    BY <2>1, <2>3, <1>1, <1>g, <1>c DEF Uploaded, Pending
  <2>9. Uploaded' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <1>r, <2>9a, <2>9b, UploadedWrite
  <2>10a. (UploadW(s, p).pc = "claimed" /\ UploadW(s, p).verified) => \A pp \in (UploadW(s, p).uploads \cap UploadW(s, p).upDone) \ UploadW(s, p).gone : UploadW(s, p).snap[pp] \in live'
    BY <2>1, <2>3, <2>g, <1>c DEF Verified
  <2>10. Verified' BY <1>c, <1>m, <2>2, <2>e, <2>10a, VerifiedWrite
  <2>11a. UploadW(s, p).pc = "scanned" => ~UploadW(s, p).verified BY <2>3, <2>g, <1>c DEF Unverified
  <2>11. Unverified' BY <1>c, <2>2, <2>11a, UnverifiedWrite
  <2>12a. \A h \in {} : h \in live' OBVIOUS
  <2>12. Inv_CitationsLive' BY <1>c, <2>e, <2>12a, CLWrite
  <2>13. Inv_OneName' BY <2>1, <1>c DEF Inv_OneName
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13 DEF IndM2, M2
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. /\ copies < MaxCopies
        /\ live' = live \cup {c} /\ upped' = upped \cup {c} /\ minted' = minted \cup {c} /\ copies' = copies + 1
        /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] /\ UNCHANGED <<doc, gw, nextGen>>
    BY <1>2 DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" /\ p \in w[s].uploads \ w[s].upDone BY DEF Upload
  <2>2. Wr(s, UploadCopyW(s, p, c)) BY <1>a, <2>1, WriteAny DEF Wr
  <2>3. /\ UploadCopyW(s, p, c).pc = w[s].pc
        /\ UploadCopyW(s, p, c).uploads = w[s].uploads
        /\ UploadCopyW(s, p, c).upDone = w[s].upDone \cup {p}
        /\ UploadCopyW(s, p, c).gone = w[s].gone
        /\ UploadCopyW(s, p, c).verified = w[s].verified
        /\ UploadCopyW(s, p, c).local = [w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]]
        /\ UploadCopyW(s, p, c).snap = [w[s].snap EXCEPT ![p] = c]
    BY DEF UploadCopyW
  <2>4. Fresh' /\ c \notin minted BY <1>c, <1>d, <1>0, <2>1, <1>r, FreshCopy
  <2>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, UploadCopyW(s, p, c)) /\ LiveEv
        /\ minted \subseteq minted' /\ upped \subseteq upped'
    BY <2>1, <2>3, <2>4, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
  <2>5a. \A q \in Paths : UploadCopyW(s, p, c).snap[q] \in Opt(minted') BY <2>1, <2>3, <1>c, <1>d, <2>e, <1>f DEF SnapMinted, Minted, Opt
  <2>5. SnapMinted' BY <1>c, <2>2, <2>e, <2>5a, SnapMintedWrite
  <2>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <2>1
  <2>6. Flight' BY <1>c, <1>d, <2>6a, <2>e, <1>r, <2>2, FlightKeep
  <2>7a. \A pp \in Paths : (UploadCopyW(s, p, c).local[pp] # Nil /\ UploadCopyW(s, p, c).local[pp] \notin upped') =>
            \/ UploadCopyW(s, p, c).local[pp] = w[s].local[pp]
            \/ (UploadCopyW(s, p, c).local[pp][1] = pp /\ UploadCopyW(s, p, c).local[pp] \notin minted)
    BY <2>3, <2>4, <1>f
  <2>7. Private' BY <1>c, <1>d, <2>2, <2>e, <2>7a, PrivateWrite
  <2>8a. UploadCopyW(s, p, c).pc \in {"scanned", "claimed"} =>
            \A pp \in UploadCopyW(s, p, c).uploads \ UploadCopyW(s, p, c).upDone :
              /\ UploadCopyW(s, p, c).snap[pp] # Nil
              /\ (UploadCopyW(s, p, c).snap[pp] \notin upped' =>
                    \/ (UploadCopyW(s, p, c).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                    \/ UploadCopyW(s, p, c).snap[pp] = w[s].local[pp]
                    \/ (UploadCopyW(s, p, c).snap[pp][1] = pp /\ UploadCopyW(s, p, c).snap[pp] \notin minted))
    BY <2>3, <1>g, <1>c, <1>f DEF Pending
  <2>8. Pending' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <2>8a, PendingWrite
  <2>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
    OBVIOUS
  <2>9b. UploadCopyW(s, p, c).pc \in {"scanned", "claimed"} =>
            \A pp \in UploadCopyW(s, p, c).upDone : UploadCopyW(s, p, c).snap[pp][1] = pp /\
              \/ (UploadCopyW(s, p, c).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
              \/ (UploadCopyW(s, p, c).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                  /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
              \/ (UploadCopyW(s, p, c).snap[pp] # Nil /\ UploadCopyW(s, p, c).snap[pp] \notin minted /\ UploadCopyW(s, p, c).snap[pp] \in upped' /\ UploadCopyW(s, p, c).snap[pp] \notin {}
                  /\ \A k \in Paths : gw'[k] # UploadCopyW(s, p, c).snap[pp])
    BY <2>1, <2>3, <2>4, <1>g, <1>c, <1>d, <1>0, <1>f, CopyHandle, NilHandle DEF Uploaded, Minted, Opt
  <2>9. Uploaded' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <1>r, <2>9a, <2>9b, UploadedWrite
  <2>10a. (UploadCopyW(s, p, c).pc = "claimed" /\ UploadCopyW(s, p, c).verified) => \A pp \in (UploadCopyW(s, p, c).uploads \cap UploadCopyW(s, p, c).upDone) \ UploadCopyW(s, p, c).gone : UploadCopyW(s, p, c).snap[pp] \in live'
    BY <2>1, <2>3, <2>g, <1>c DEF Verified
  <2>10. Verified' BY <1>c, <1>m, <2>2, <2>e, <2>10a, VerifiedWrite
  <2>11a. UploadCopyW(s, p, c).pc = "scanned" => ~UploadCopyW(s, p, c).verified BY <2>3, <2>g, <1>c DEF Unverified
  <2>11. Unverified' BY <1>c, <2>2, <2>11a, UnverifiedWrite
  <2>12a. \A h \in {} : h \in live' OBVIOUS
  <2>12. Inv_CitationsLive' BY <1>c, <2>e, <2>12a, CLWrite
  <2>13. Inv_OneName' BY <2>1, <1>c DEF Inv_OneName
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13 DEF IndM2, M2
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* The commit section.                                                      *)

LEMMA PullOnly_M2 ==
  ASSUME IndM2, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, PullOnly_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF PullOnly, bucket, aux
<1>g. On(s) /\ w[s].pc = "scanned" BY DEF PullOnly
<1>2. Wr(s, PullOnlyW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ PullOnlyW(s).pc = "idle"
      /\ PullOnlyW(s).uploads = w[s].uploads
      /\ PullOnlyW(s).upDone = {}
      /\ PullOnlyW(s).gone = {}
      /\ PullOnlyW(s).verified = FALSE
      /\ PullOnlyW(s).local = w[s].local
      /\ PullOnlyW(s).snap = [q \in Paths |-> Nil]
  BY DEF PullOnlyW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, PullOnlyW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : PullOnlyW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (PullOnlyW(s).local[pp] # Nil /\ PullOnlyW(s).local[pp] \notin upped') =>
          \/ PullOnlyW(s).local[pp] = w[s].local[pp]
          \/ (PullOnlyW(s).local[pp][1] = pp /\ PullOnlyW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. PullOnlyW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in PullOnlyW(s).uploads \ PullOnlyW(s).upDone :
            /\ PullOnlyW(s).snap[pp] # Nil
            /\ (PullOnlyW(s).snap[pp] \notin upped' =>
                  \/ (PullOnlyW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ PullOnlyW(s).snap[pp] = w[s].local[pp]
                  \/ (PullOnlyW(s).snap[pp][1] = pp /\ PullOnlyW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. PullOnlyW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in PullOnlyW(s).upDone : PullOnlyW(s).snap[pp][1] = pp /\
            \/ (PullOnlyW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (PullOnlyW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (PullOnlyW(s).snap[pp] # Nil /\ PullOnlyW(s).snap[pp] \notin minted /\ PullOnlyW(s).snap[pp] \in upped' /\ PullOnlyW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # PullOnlyW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (PullOnlyW(s).pc = "claimed" /\ PullOnlyW(s).verified) => \A pp \in (PullOnlyW(s).uploads \cap PullOnlyW(s).upDone) \ PullOnlyW(s).gone : PullOnlyW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. PullOnlyW(s).pc = "scanned" => ~PullOnlyW(s).verified BY <1>3
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Claim_M2 ==
  ASSUME IndM2, NEW s \in Writers, Claim(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Claim_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Claim, aux
<1>g. On(s) /\ w[s].pc = "scanned" BY DEF Claim
<1>2. Wr(s, ClaimW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ ClaimW(s).pc = "claimed"
      /\ ClaimW(s).uploads = w[s].uploads
      /\ ClaimW(s).upDone = w[s].upDone
      /\ ClaimW(s).gone = w[s].gone
      /\ ClaimW(s).verified = w[s].verified
      /\ ClaimW(s).local = w[s].local
      /\ ClaimW(s).snap = w[s].snap
  BY DEF ClaimW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, ClaimW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : ClaimW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (ClaimW(s).local[pp] # Nil /\ ClaimW(s).local[pp] \notin upped') =>
          \/ ClaimW(s).local[pp] = w[s].local[pp]
          \/ (ClaimW(s).local[pp][1] = pp /\ ClaimW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. ClaimW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in ClaimW(s).uploads \ ClaimW(s).upDone :
            /\ ClaimW(s).snap[pp] # Nil
            /\ (ClaimW(s).snap[pp] \notin upped' =>
                  \/ (ClaimW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ ClaimW(s).snap[pp] = w[s].local[pp]
                  \/ (ClaimW(s).snap[pp][1] = pp /\ ClaimW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. ClaimW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in ClaimW(s).upDone : ClaimW(s).snap[pp][1] = pp /\
            \/ (ClaimW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (ClaimW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (ClaimW(s).snap[pp] # Nil /\ ClaimW(s).snap[pp] \notin minted /\ ClaimW(s).snap[pp] \in upped' /\ ClaimW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # ClaimW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (ClaimW(s).pc = "claimed" /\ ClaimW(s).verified) => \A pp \in (ClaimW(s).uploads \cap ClaimW(s).upDone) \ ClaimW(s).gone : ClaimW(s).snap[pp] \in live'
  BY <1>3, <1>g, <1>c DEF Unverified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. ClaimW(s).pc = "scanned" => ~ClaimW(s).verified BY <1>3
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Verify_M2 ==
  ASSUME IndM2, NEW s \in Writers, Verify(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Verify_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Verify, bucket, aux
<1>g. On(s) /\ w[s].pc = "claimed" BY DEF Verify
<1>2. Wr(s, VerifyW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ VerifyW(s).pc = w[s].pc
      /\ VerifyW(s).uploads = w[s].uploads
      /\ VerifyW(s).upDone = w[s].upDone
      /\ VerifyW(s).gone = IF CommitVerifiesUploads THEN {q \in w[s].uploads \cap w[s].upDone : w[s].snap[q] \notin live} ELSE {}
      /\ VerifyW(s).verified = TRUE
      /\ VerifyW(s).local = w[s].local
      /\ VerifyW(s).snap = w[s].snap
  BY DEF VerifyW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, VerifyW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : VerifyW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (VerifyW(s).local[pp] # Nil /\ VerifyW(s).local[pp] \notin upped') =>
          \/ VerifyW(s).local[pp] = w[s].local[pp]
          \/ (VerifyW(s).local[pp][1] = pp /\ VerifyW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. VerifyW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in VerifyW(s).uploads \ VerifyW(s).upDone :
            /\ VerifyW(s).snap[pp] # Nil
            /\ (VerifyW(s).snap[pp] \notin upped' =>
                  \/ (VerifyW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ VerifyW(s).snap[pp] = w[s].local[pp]
                  \/ (VerifyW(s).snap[pp][1] = pp /\ VerifyW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. VerifyW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in VerifyW(s).upDone : VerifyW(s).snap[pp][1] = pp /\
            \/ (VerifyW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (VerifyW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (VerifyW(s).snap[pp] # Nil /\ VerifyW(s).snap[pp] \notin minted /\ VerifyW(s).snap[pp] \in upped' /\ VerifyW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # VerifyW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (VerifyW(s).pc = "claimed" /\ VerifyW(s).verified) => \A pp \in (VerifyW(s).uploads \cap VerifyW(s).upDone) \ VerifyW(s).gone : VerifyW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>s
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. VerifyW(s).pc = "scanned" => ~VerifyW(s).verified BY <1>3, <1>g
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Install_M2 ==
  ASSUME IndM2, NEW s \in Writers, Install(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Install_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>i. \A k \in Paths : InstallInst(s)[k] = Nil \/ InstallInst(s)[k] = doc[k]
                      \/ (k \in InstallMine(s) \ w[s].gone /\ InstallInst(s)[k] = w[s].snap[k])
  BY DEF InstallInst
<1>j. \A k \in InstallMine(s) \ w[s].gone : w[s].snap[k][1] = k /\ ~Cited(w[s].snap[k]) /\ w[s].snap[k] # Nil /\ w[s].snap[k] \in live
  BY <1>c DEF Uploaded, Up, Verified, InstallMine, Install
<1>1. doc' = InstallInst(s) /\ w' = [w EXCEPT ![s] = InstallW(s)] /\ UNCHANGED <<live, minted, gw, nextGen, copies, upped>>
  BY DEF Install, aux
<1>g. On(s) /\ w[s].pc = "claimed" /\ w[s].verified BY DEF Install
<1>2. Wr(s, InstallW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ InstallW(s).pc = "cased"
      /\ InstallW(s).uploads = w[s].uploads
      /\ InstallW(s).upDone = w[s].upDone \ w[s].gone
      /\ InstallW(s).gone = w[s].gone
      /\ InstallW(s).verified = w[s].verified
      /\ InstallW(s).local = w[s].local
      /\ InstallW(s).snap = w[s].snap
  BY DEF InstallW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({w[s].snap[k] : k \in InstallMine(s) \ w[s].gone}) /\ GwEv /\ TreeEv(s, InstallW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>i, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet
<1>5a. \A q \in Paths : InstallW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {w[s].snap[k] : k \in InstallMine(s) \ w[s].gone} BY <1>1, <1>c, <1>f DEF Flight, InstallMine
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (InstallW(s).local[pp] # Nil /\ InstallW(s).local[pp] \notin upped') =>
          \/ InstallW(s).local[pp] = w[s].local[pp]
          \/ (InstallW(s).local[pp][1] = pp /\ InstallW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. InstallW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in InstallW(s).uploads \ InstallW(s).upDone :
            /\ InstallW(s).snap[pp] # Nil
            /\ (InstallW(s).snap[pp] \notin upped' =>
                  \/ (InstallW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ InstallW(s).snap[pp] = w[s].local[pp]
                  \/ (InstallW(s).snap[pp][1] = pp /\ InstallW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {w[s].snap[k] : k \in InstallMine(s) \ w[s].gone}
  BY <1>c, <1>f DEF Uploaded, Up, Priv, InstallMine
<1>9b. InstallW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in InstallW(s).upDone : InstallW(s).snap[pp][1] = pp /\
            \/ (InstallW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {w[s].snap[k] : k \in InstallMine(s) \ w[s].gone})
            \/ (InstallW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {w[s].snap[k] : k \in InstallMine(s) \ w[s].gone})
            \/ (InstallW(s).snap[pp] # Nil /\ InstallW(s).snap[pp] \notin minted /\ InstallW(s).snap[pp] \in upped' /\ InstallW(s).snap[pp] \notin {w[s].snap[k] : k \in InstallMine(s) \ w[s].gone}
                /\ \A k \in Paths : gw'[k] # InstallW(s).snap[pp])
  BY <1>3
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (InstallW(s).pc = "claimed" /\ InstallW(s).verified) => \A pp \in (InstallW(s).uploads \cap InstallW(s).upDone) \ InstallW(s).gone : InstallW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. InstallW(s).pc = "scanned" => ~InstallW(s).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {w[s].snap[k] : k \in InstallMine(s) \ w[s].gone} : h \in live' BY <1>1, <1>j
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName'
  <2>1. \A k \in InstallMine(s) \ w[s].gone, j \in Paths : doc[j] # w[s].snap[k] BY <1>j DEF Cited
  <2>. QED BY <1>1, <1>c, <1>i, <1>j, <2>1 DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Collect_M2 ==
  ASSUME IndM2, NEW s \in Writers, Collect(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Collect_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. live' = live /\ w' = [w EXCEPT ![s] = CollectW(s)] /\ UNCHANGED <<minted, doc, gw, nextGen, copies, upped>>
  BY <1>s DEF Collect, aux
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Collect
<1>2. Wr(s, CollectW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ CollectW(s).pc = w[s].pc
      /\ CollectW(s).uploads = w[s].uploads
      /\ CollectW(s).upDone = w[s].upDone
      /\ CollectW(s).gone = w[s].gone
      /\ CollectW(s).verified = w[s].verified
      /\ CollectW(s).local = w[s].local
      /\ CollectW(s).snap = w[s].snap
  BY DEF CollectW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, CollectW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : CollectW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (CollectW(s).local[pp] # Nil /\ CollectW(s).local[pp] \notin upped') =>
          \/ CollectW(s).local[pp] = w[s].local[pp]
          \/ (CollectW(s).local[pp][1] = pp /\ CollectW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. CollectW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in CollectW(s).uploads \ CollectW(s).upDone :
            /\ CollectW(s).snap[pp] # Nil
            /\ (CollectW(s).snap[pp] \notin upped' =>
                  \/ (CollectW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ CollectW(s).snap[pp] = w[s].local[pp]
                  \/ (CollectW(s).snap[pp][1] = pp /\ CollectW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. CollectW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in CollectW(s).upDone : CollectW(s).snap[pp][1] = pp /\
            \/ (CollectW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (CollectW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (CollectW(s).snap[pp] # Nil /\ CollectW(s).snap[pp] \notin minted /\ CollectW(s).snap[pp] \in upped' /\ CollectW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # CollectW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (CollectW(s).pc = "claimed" /\ CollectW(s).verified) => \A pp \in (CollectW(s).uploads \cap CollectW(s).upDone) \ CollectW(s).gone : CollectW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. CollectW(s).pc = "scanned" => ~CollectW(s).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Finish_M2 ==
  ASSUME IndM2, NEW s \in Writers, Finish(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Finish_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Finish, aux
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Finish
<1>2. Wr(s, FinishW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ FinishW(s).pc = "idle"
      /\ FinishW(s).uploads = {}
      /\ FinishW(s).upDone = {}
      /\ FinishW(s).gone = {}
      /\ FinishW(s).verified = FALSE
      /\ FinishW(s).local = w[s].local
      /\ FinishW(s).snap = [q \in Paths |-> Nil]
  BY DEF FinishW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, FinishW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : FinishW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (FinishW(s).local[pp] # Nil /\ FinishW(s).local[pp] \notin upped') =>
          \/ FinishW(s).local[pp] = w[s].local[pp]
          \/ (FinishW(s).local[pp][1] = pp /\ FinishW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. FinishW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in FinishW(s).uploads \ FinishW(s).upDone :
            /\ FinishW(s).snap[pp] # Nil
            /\ (FinishW(s).snap[pp] \notin upped' =>
                  \/ (FinishW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ FinishW(s).snap[pp] = w[s].local[pp]
                  \/ (FinishW(s).snap[pp][1] = pp /\ FinishW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. FinishW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in FinishW(s).upDone : FinishW(s).snap[pp][1] = pp /\
            \/ (FinishW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (FinishW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (FinishW(s).snap[pp] # Nil /\ FinishW(s).snap[pp] \notin minted /\ FinishW(s).snap[pp] \in upped' /\ FinishW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # FinishW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (FinishW(s).pc = "claimed" /\ FinishW(s).verified) => \A pp \in (FinishW(s).uploads \cap FinishW(s).upDone) \ FinishW(s).gone : FinishW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. FinishW(s).pc = "scanned" => ~FinishW(s).verified BY <1>3
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

------------------------------------------------------------------------------
(* The restart and the sync.                                                *)

LEMMA Restart_M2 ==
  ASSUME IndM2, NEW s \in Writers, Restart(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Restart_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Restart
<1>g. On(s) BY DEF Restart
<1>2. Wr(s, RestartW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ RestartW(s).pc = "idle"
      /\ RestartW(s).uploads = {}
      /\ RestartW(s).upDone = {}
      /\ RestartW(s).gone = {}
      /\ RestartW(s).verified = FALSE
      /\ RestartW(s).local = w[s].local
      /\ RestartW(s).snap = [q \in Paths |-> Nil]
  BY DEF RestartW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, RestartW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : RestartW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (RestartW(s).local[pp] # Nil /\ RestartW(s).local[pp] \notin upped') =>
          \/ RestartW(s).local[pp] = w[s].local[pp]
          \/ (RestartW(s).local[pp][1] = pp /\ RestartW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. RestartW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in RestartW(s).uploads \ RestartW(s).upDone :
            /\ RestartW(s).snap[pp] # Nil
            /\ (RestartW(s).snap[pp] \notin upped' =>
                  \/ (RestartW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ RestartW(s).snap[pp] = w[s].local[pp]
                  \/ (RestartW(s).snap[pp][1] = pp /\ RestartW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. RestartW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in RestartW(s).upDone : RestartW(s).snap[pp][1] = pp /\
            \/ (RestartW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (RestartW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (RestartW(s).snap[pp] # Nil /\ RestartW(s).snap[pp] \notin minted /\ RestartW(s).snap[pp] \in upped' /\ RestartW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # RestartW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (RestartW(s).pc = "claimed" /\ RestartW(s).verified) => \A pp \in (RestartW(s).uploads \cap RestartW(s).upDone) \ RestartW(s).gone : RestartW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. RestartW(s).pc = "scanned" => ~RestartW(s).verified BY <1>3
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Sync_M2 ==
  ASSUME IndM2, NEW s \in Writers, Sync(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Sync_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Sync, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF Sync
<1>2. Wr(s, SyncW(s, fail)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ SyncW(s, fail).pc = w[s].pc
      /\ SyncW(s, fail).uploads = w[s].uploads
      /\ SyncW(s, fail).upDone = w[s].upDone
      /\ SyncW(s, fail).gone = w[s].gone
      /\ SyncW(s, fail).verified = w[s].verified
      /\ SyncW(s, fail).local = [q \in Paths |-> IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]
      /\ SyncW(s, fail).snap = w[s].snap
  BY DEF SyncW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, SyncW(s, fail)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet
<1>5a. \A q \in Paths : SyncW(s, fail).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (SyncW(s, fail).local[pp] # Nil /\ SyncW(s, fail).local[pp] \notin upped') =>
          \/ SyncW(s, fail).local[pp] = w[s].local[pp]
          \/ (SyncW(s, fail).local[pp][1] = pp /\ SyncW(s, fail).local[pp] \notin minted)
  BY <1>3, <1>e, <1>c, <1>f, <1>0 DEF Inv_CitationsLive, Fresh
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. SyncW(s, fail).pc \in {"scanned", "claimed"} =>
          \A pp \in SyncW(s, fail).uploads \ SyncW(s, fail).upDone :
            /\ SyncW(s, fail).snap[pp] # Nil
            /\ (SyncW(s, fail).snap[pp] \notin upped' =>
                  \/ (SyncW(s, fail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ SyncW(s, fail).snap[pp] = w[s].local[pp]
                  \/ (SyncW(s, fail).snap[pp][1] = pp /\ SyncW(s, fail).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. SyncW(s, fail).pc \in {"scanned", "claimed"} =>
          \A pp \in SyncW(s, fail).upDone : SyncW(s, fail).snap[pp][1] = pp /\
            \/ (SyncW(s, fail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (SyncW(s, fail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (SyncW(s, fail).snap[pp] # Nil /\ SyncW(s, fail).snap[pp] \notin minted /\ SyncW(s, fail).snap[pp] \in upped' /\ SyncW(s, fail).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # SyncW(s, fail).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (SyncW(s, fail).pc = "claimed" /\ SyncW(s, fail).verified) => \A pp \in (SyncW(s, fail).uploads \cap SyncW(s, fail).upDone) \ SyncW(s, fail).gone : SyncW(s, fail).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. SyncW(s, fail).pc = "scanned" => ~SyncW(s, fail).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

------------------------------------------------------------------------------
(* The narrow / widen verb.                                                 *)

LEMMA RescopeBegin_M2 ==
  ASSUME IndM2, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, RescopeBegin_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF RescopeBegin, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RescopeBegin
<1>2. Wr(s, RescopeBeginW(s, T)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ RescopeBeginW(s, T).pc = w[s].pc
      /\ RescopeBeginW(s, T).uploads = w[s].uploads
      /\ RescopeBeginW(s, T).upDone = w[s].upDone
      /\ RescopeBeginW(s, T).gone = w[s].gone
      /\ RescopeBeginW(s, T).verified = w[s].verified
      /\ RescopeBeginW(s, T).local = w[s].local
      /\ RescopeBeginW(s, T).snap = w[s].snap
  BY DEF RescopeBeginW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, RescopeBeginW(s, T)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : RescopeBeginW(s, T).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (RescopeBeginW(s, T).local[pp] # Nil /\ RescopeBeginW(s, T).local[pp] \notin upped') =>
          \/ RescopeBeginW(s, T).local[pp] = w[s].local[pp]
          \/ (RescopeBeginW(s, T).local[pp][1] = pp /\ RescopeBeginW(s, T).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. RescopeBeginW(s, T).pc \in {"scanned", "claimed"} =>
          \A pp \in RescopeBeginW(s, T).uploads \ RescopeBeginW(s, T).upDone :
            /\ RescopeBeginW(s, T).snap[pp] # Nil
            /\ (RescopeBeginW(s, T).snap[pp] \notin upped' =>
                  \/ (RescopeBeginW(s, T).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ RescopeBeginW(s, T).snap[pp] = w[s].local[pp]
                  \/ (RescopeBeginW(s, T).snap[pp][1] = pp /\ RescopeBeginW(s, T).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. RescopeBeginW(s, T).pc \in {"scanned", "claimed"} =>
          \A pp \in RescopeBeginW(s, T).upDone : RescopeBeginW(s, T).snap[pp][1] = pp /\
            \/ (RescopeBeginW(s, T).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (RescopeBeginW(s, T).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (RescopeBeginW(s, T).snap[pp] # Nil /\ RescopeBeginW(s, T).snap[pp] \notin minted /\ RescopeBeginW(s, T).snap[pp] \in upped' /\ RescopeBeginW(s, T).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # RescopeBeginW(s, T).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (RescopeBeginW(s, T).pc = "claimed" /\ RescopeBeginW(s, T).verified) => \A pp \in (RescopeBeginW(s, T).uploads \cap RescopeBeginW(s, T).upDone) \ RescopeBeginW(s, T).gone : RescopeBeginW(s, T).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. RescopeBeginW(s, T).pc = "scanned" => ~RescopeBeginW(s, T).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA RescopeFirst_M2 ==
  ASSUME IndM2, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, RescopeFirst_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF RescopeFirst, bucket, aux
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RescopeFirst
<1>2. Wr(s, RescopeFirstW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ RescopeFirstW(s).pc = w[s].pc
      /\ RescopeFirstW(s).uploads = w[s].uploads
      /\ RescopeFirstW(s).upDone = w[s].upDone
      /\ RescopeFirstW(s).gone = w[s].gone
      /\ RescopeFirstW(s).verified = w[s].verified
      /\ RescopeFirstW(s).local = w[s].local
      /\ RescopeFirstW(s).snap = w[s].snap
  BY <1>s DEF RescopeFirstW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, RescopeFirstW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : RescopeFirstW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (RescopeFirstW(s).local[pp] # Nil /\ RescopeFirstW(s).local[pp] \notin upped') =>
          \/ RescopeFirstW(s).local[pp] = w[s].local[pp]
          \/ (RescopeFirstW(s).local[pp][1] = pp /\ RescopeFirstW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. RescopeFirstW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in RescopeFirstW(s).uploads \ RescopeFirstW(s).upDone :
            /\ RescopeFirstW(s).snap[pp] # Nil
            /\ (RescopeFirstW(s).snap[pp] \notin upped' =>
                  \/ (RescopeFirstW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ RescopeFirstW(s).snap[pp] = w[s].local[pp]
                  \/ (RescopeFirstW(s).snap[pp][1] = pp /\ RescopeFirstW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. RescopeFirstW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in RescopeFirstW(s).upDone : RescopeFirstW(s).snap[pp][1] = pp /\
            \/ (RescopeFirstW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (RescopeFirstW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (RescopeFirstW(s).snap[pp] # Nil /\ RescopeFirstW(s).snap[pp] \notin minted /\ RescopeFirstW(s).snap[pp] \in upped' /\ RescopeFirstW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # RescopeFirstW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (RescopeFirstW(s).pc = "claimed" /\ RescopeFirstW(s).verified) => \A pp \in (RescopeFirstW(s).uploads \cap RescopeFirstW(s).upDone) \ RescopeFirstW(s).gone : RescopeFirstW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. RescopeFirstW(s).pc = "scanned" => ~RescopeFirstW(s).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA RescopeSecond_M2 ==
  ASSUME IndM2, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, RescopeSecond_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>u. RescopeLocal1(s) = [q \in Paths |-> IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]] BY <1>s DEF RescopeLocal1
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>
  BY DEF RescopeSecond, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RescopeSecond
<1>2. Wr(s, RescopeSecondW(s, wfail)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ RescopeSecondW(s, wfail).pc = w[s].pc
      /\ RescopeSecondW(s, wfail).uploads = w[s].uploads
      /\ RescopeSecondW(s, wfail).upDone = w[s].upDone
      /\ RescopeSecondW(s, wfail).gone = w[s].gone
      /\ RescopeSecondW(s, wfail).verified = w[s].verified
      /\ RescopeSecondW(s, wfail).local = [q \in Paths |-> IF q \in RescopeFetch(s, wfail) /\ (RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal)
                                        THEN doc[q] ELSE RescopeLocal1(s)[q]]
      /\ RescopeSecondW(s, wfail).snap = w[s].snap
  BY DEF RescopeSecondW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, RescopeSecondW(s, wfail)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>u, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet
<1>5a. \A q \in Paths : RescopeSecondW(s, wfail).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (RescopeSecondW(s, wfail).local[pp] # Nil /\ RescopeSecondW(s, wfail).local[pp] \notin upped') =>
          \/ RescopeSecondW(s, wfail).local[pp] = w[s].local[pp]
          \/ (RescopeSecondW(s, wfail).local[pp][1] = pp /\ RescopeSecondW(s, wfail).local[pp] \notin minted)
  BY <1>3, <1>u, <1>e, <1>c, <1>f, <1>0 DEF Inv_CitationsLive, Fresh
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. RescopeSecondW(s, wfail).pc \in {"scanned", "claimed"} =>
          \A pp \in RescopeSecondW(s, wfail).uploads \ RescopeSecondW(s, wfail).upDone :
            /\ RescopeSecondW(s, wfail).snap[pp] # Nil
            /\ (RescopeSecondW(s, wfail).snap[pp] \notin upped' =>
                  \/ (RescopeSecondW(s, wfail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ RescopeSecondW(s, wfail).snap[pp] = w[s].local[pp]
                  \/ (RescopeSecondW(s, wfail).snap[pp][1] = pp /\ RescopeSecondW(s, wfail).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. RescopeSecondW(s, wfail).pc \in {"scanned", "claimed"} =>
          \A pp \in RescopeSecondW(s, wfail).upDone : RescopeSecondW(s, wfail).snap[pp][1] = pp /\
            \/ (RescopeSecondW(s, wfail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (RescopeSecondW(s, wfail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (RescopeSecondW(s, wfail).snap[pp] # Nil /\ RescopeSecondW(s, wfail).snap[pp] \notin minted /\ RescopeSecondW(s, wfail).snap[pp] \in upped' /\ RescopeSecondW(s, wfail).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # RescopeSecondW(s, wfail).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (RescopeSecondW(s, wfail).pc = "claimed" /\ RescopeSecondW(s, wfail).verified) => \A pp \in (RescopeSecondW(s, wfail).uploads \cap RescopeSecondW(s, wfail).upDone) \ RescopeSecondW(s, wfail).gone : RescopeSecondW(s, wfail).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. RescopeSecondW(s, wfail).pc = "scanned" => ~RescopeSecondW(s, wfail).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

------------------------------------------------------------------------------
(* A reader's tick.                                                         *)

LEMMA RPullRead_M2 ==
  ASSUME IndM2, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, RPullRead_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF RPullRead, bucket, aux
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RPullRead
<1>2. Wr(s, RPullReadW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ RPullReadW(s).pc = "pulling"
      /\ RPullReadW(s).uploads = w[s].uploads
      /\ RPullReadW(s).upDone = w[s].upDone
      /\ RPullReadW(s).gone = w[s].gone
      /\ RPullReadW(s).verified = w[s].verified
      /\ RPullReadW(s).local = w[s].local
      /\ RPullReadW(s).snap = w[s].snap
  BY DEF RPullReadW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, RPullReadW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : RPullReadW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (RPullReadW(s).local[pp] # Nil /\ RPullReadW(s).local[pp] \notin upped') =>
          \/ RPullReadW(s).local[pp] = w[s].local[pp]
          \/ (RPullReadW(s).local[pp][1] = pp /\ RPullReadW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. RPullReadW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in RPullReadW(s).uploads \ RPullReadW(s).upDone :
            /\ RPullReadW(s).snap[pp] # Nil
            /\ (RPullReadW(s).snap[pp] \notin upped' =>
                  \/ (RPullReadW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ RPullReadW(s).snap[pp] = w[s].local[pp]
                  \/ (RPullReadW(s).snap[pp][1] = pp /\ RPullReadW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. RPullReadW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in RPullReadW(s).upDone : RPullReadW(s).snap[pp][1] = pp /\
            \/ (RPullReadW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (RPullReadW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (RPullReadW(s).snap[pp] # Nil /\ RPullReadW(s).snap[pp] \notin minted /\ RPullReadW(s).snap[pp] \in upped' /\ RPullReadW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # RPullReadW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (RPullReadW(s).pc = "claimed" /\ RPullReadW(s).verified) => \A pp \in (RPullReadW(s).uploads \cap RPullReadW(s).upDone) \ RPullReadW(s).gone : RPullReadW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. RPullReadW(s).pc = "scanned" => ~RPullReadW(s).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA RPullSync_M2 ==
  ASSUME IndM2, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, RPullSync_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF RPullSync, bucket
<1>g. On(s) /\ w[s].pc = "pulling" BY DEF RPullSync
<1>2. Wr(s, RPullSyncW(s, fail)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ RPullSyncW(s, fail).pc = "idle"
      /\ RPullSyncW(s, fail).uploads = w[s].uploads
      /\ RPullSyncW(s, fail).upDone = w[s].upDone
      /\ RPullSyncW(s, fail).gone = w[s].gone
      /\ RPullSyncW(s, fail).verified = w[s].verified
      /\ RPullSyncW(s, fail).local = [q \in Paths |-> IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]
      /\ RPullSyncW(s, fail).snap = w[s].snap
  BY DEF RPullSyncW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, RPullSyncW(s, fail)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet
<1>5a. \A q \in Paths : RPullSyncW(s, fail).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (RPullSyncW(s, fail).local[pp] # Nil /\ RPullSyncW(s, fail).local[pp] \notin upped') =>
          \/ RPullSyncW(s, fail).local[pp] = w[s].local[pp]
          \/ (RPullSyncW(s, fail).local[pp][1] = pp /\ RPullSyncW(s, fail).local[pp] \notin minted)
  BY <1>3, <1>e, <1>c, <1>f, <1>0 DEF Inv_CitationsLive, Fresh
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. RPullSyncW(s, fail).pc \in {"scanned", "claimed"} =>
          \A pp \in RPullSyncW(s, fail).uploads \ RPullSyncW(s, fail).upDone :
            /\ RPullSyncW(s, fail).snap[pp] # Nil
            /\ (RPullSyncW(s, fail).snap[pp] \notin upped' =>
                  \/ (RPullSyncW(s, fail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ RPullSyncW(s, fail).snap[pp] = w[s].local[pp]
                  \/ (RPullSyncW(s, fail).snap[pp][1] = pp /\ RPullSyncW(s, fail).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. RPullSyncW(s, fail).pc \in {"scanned", "claimed"} =>
          \A pp \in RPullSyncW(s, fail).upDone : RPullSyncW(s, fail).snap[pp][1] = pp /\
            \/ (RPullSyncW(s, fail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (RPullSyncW(s, fail).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (RPullSyncW(s, fail).snap[pp] # Nil /\ RPullSyncW(s, fail).snap[pp] \notin minted /\ RPullSyncW(s, fail).snap[pp] \in upped' /\ RPullSyncW(s, fail).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # RPullSyncW(s, fail).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (RPullSyncW(s, fail).pc = "claimed" /\ RPullSyncW(s, fail).verified) => \A pp \in (RPullSyncW(s, fail).uploads \cap RPullSyncW(s, fail).upDone) \ RPullSyncW(s, fail).gone : RPullSyncW(s, fail).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. RPullSyncW(s, fail).pc = "scanned" => ~RPullSyncW(s, fail).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

------------------------------------------------------------------------------
(* The step, and the theorems.                                              *)

LEMMA Next_M2 == IndM2 /\ Next => IndM2'
<1>. SUFFICES ASSUME IndM2, Next PROVE IndM2' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM2' BY <3>1, GPut_M2
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM2' BY <3>2, GCas_M2
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM2' BY <3>3, GDelete_M2
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM2' BY <3>4, GRename_M2
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M2
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM2' BY <3>1, Checkout_M2
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM2' BY <3>2, Consume_M2
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM2' BY <3>3, Scan_M2
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM2' BY <3>4, Skip_M2
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM2' BY <3>5, PullOnly_M2
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM2' BY <3>6, Claim_M2
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM2' BY <3>7, Verify_M2
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM2' BY <3>8, Install_M2
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM2' BY <3>9, Collect_M2
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM2' BY <3>10, Finish_M2
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM2' BY <3>11, Restart_M2
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM2' BY <3>12, Sync_M2
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM2' BY <3>13, RescopeBegin_M2
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM2' BY <3>14, RescopeFirst_M2
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM2' BY <3>15, RescopeSecond_M2
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM2' BY <3>16, RPullRead_M2
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM2' BY <3>17, RPullSync_M2
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM2' BY <3>18, Edit_M2
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM2' BY <3>19, Delete_M2
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM2' BY <3>20, Upload_M2
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM2' BY <3>21, Sweep_M2
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M2
  <2>2. CASE RLoad BY <2>2, RLoad_M2
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM2' BY <2>3, Reap_M2
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M2Invariant == Spec => []IndM2
<1>1. Init => IndM2 BY Init_M2
<1>2. IndM2 /\ [Next]_vars => IndM2'
  <2>1. IndM2 /\ Next => IndM2' BY Next_M2
  <2>2. IndM2 /\ UNCHANGED vars => IndM2'
    <3>1. IndM2 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM2 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM2, IndM1, vars
    <3>3. IndM2 /\ UNCHANGED vars => M2' BY M2Same DEF IndM2, vars, aux, ret
    <3>. QED BY <3>1, <3>2, <3>3 DEF IndM2, IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

THEOREM CitationsLive == Spec => []Inv_CitationsLive
<1>1. IndM2 => Inv_CitationsLive BY DEF IndM2, M2
<1>. QED BY M2Invariant, <1>1, PTL

THEOREM OneName == Spec => []Inv_OneName
<1>1. IndM2 => Inv_OneName BY DEF IndM2, M2
<1>. QED BY M2Invariant, <1>1, PTL

------------------------------------------------------------------------------
(* M3: Inv_ShortcutSound, Inv_ReaderSound.                                  *)

\* seq starts at 1, so a record of 0 ("nothing derived") never matches it.
M3Seq == seq >= 1
\* I10 and I9's bounds, for every tree (a non-reader's memo stays 0).
Bounds == \A s \in Writers :
            /\ w[s].derived <= seq /\ w[s].memo <= seq /\ w[s].rnow <= seq
            /\ (w[s].derived # 0 => w[s].memo <= w[s].derived)
\* I8: the cheap path's record.
Record == \A s \in Writers : (w[s].derived = seq /\ w[s].sStage = "none") =>
            \A p \in Paths : (Held(s, p) /\ doc[p] # w[s].baseline[p]) => p \in w[s].skipped
\* Between the CAS and the finish: the install's own paths hold the snapshot;
\* with `adv`, every other held path where the INSTALLED document differs
\* from the baseline is skipped; a record still current means the install
\* moved nothing.
CasedMine == \A s \in Writers : w[s].pc = "cased" =>
               \A p \in w[s].uploads \cap w[s].upDone : w[s].inst[p] = w[s].snap[p]
CasedAdv == \A s \in Writers : (w[s].pc = "cased" /\ w[s].adv) =>
              \A p \in Paths \ ((w[s].uploads \cap w[s].upDone) \cup w[s].deletes) :
                (Held(s, p) /\ w[s].inst[p] # w[s].baseline[p]) => p \in w[s].skipped
CasedSame == \A s \in Writers : (w[s].pc = "cased" /\ w[s].derived = seq) =>
               w[s].adv /\ w[s].inst = doc
M3 == M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame
IndM3 == IndM2 /\ M3

\* The step's event: the pointer never goes back, and it moves when the
\* document does.
SeqEv == seq' \in Nat /\ seq <= seq' /\ (seq' = seq => doc' = doc)
\* What the written tree R must satisfy, read off R (the primed pointer and
\* document are the step's).
HeldR(R, p) == R.baseline[p] # Nil \/ p \in R.scope
RB(R) == /\ R.derived <= seq' /\ R.memo <= seq' /\ R.rnow <= seq'
         /\ (R.derived # 0 => R.memo <= R.derived)
RRec(R) == (R.derived = seq' /\ R.sStage = "none") =>
             \A p \in Paths : (HeldR(R, p) /\ doc'[p] # R.baseline[p]) => p \in R.skipped
RMine(R) == R.pc = "cased" => \A p \in R.uploads \cap R.upDone : R.inst[p] = R.snap[p]
RAdv(R) == (R.pc = "cased" /\ R.adv) =>
             \A p \in Paths \ ((R.uploads \cap R.upDone) \cup R.deletes) :
               (HeldR(R, p) /\ R.inst[p] # R.baseline[p]) => p \in R.skipped
RSame(R) == (R.pc = "cased" /\ R.derived = seq') => R.adv /\ R.inst = doc'

------------------------------------------------------------------------------
(* What the event and the tree give M3.                                     *)

LEMMA M3Write ==
  ASSUME M3, seq \in Nat, SeqEv, w \in [Writers -> Writer], w' \in [Writers -> Writer],
         NEW t, NEW R, Wr(t, R),
         t \in Writers => RB(R) /\ RRec(R) /\ RMine(R) /\ RAdv(R) /\ RSame(R)
  PROVE  M3'
<1>0. seq' \in Nat /\ seq <= seq' /\ (seq' = seq => doc' = doc) BY DEF SeqEv
<1>1. M3Seq' BY <1>0 DEF M3, M3Seq
<1>n. \A u \in Writers : w[u].derived \in Nat /\ w[u].memo \in Nat /\ w[u].rnow \in Nat
  <2>. SUFFICES ASSUME NEW u \in Writers PROVE w[u].derived \in Nat /\ w[u].memo \in Nat /\ w[u].rnow \in Nat
    OBVIOUS
  <2>1. w[u] \in Writer OBVIOUS
  <2>. QED BY <2>1, WriterFields
<1>k. \A u \in Writers : u # t => w'[u] = w[u] BY DEF Wr
<1>t. t \in Writers => w'[t] = R BY DEF Wr
<1>2. Bounds'
  <2>. SUFFICES ASSUME NEW u \in Writers
                PROVE  /\ w'[u].derived <= seq' /\ w'[u].memo <= seq' /\ w'[u].rnow <= seq'
                       /\ (w'[u].derived # 0 => w'[u].memo <= w'[u].derived)
    BY DEF Bounds
  <2>1. CASE u = t BY <2>1, <1>t DEF RB
  <2>2. CASE u # t
    <3>1. w'[u] = w[u] BY <2>2, <1>k
    <3>2. /\ w[u].derived <= seq /\ w[u].memo <= seq /\ w[u].rnow <= seq
          /\ (w[u].derived # 0 => w[u].memo <= w[u].derived)
      BY DEF M3, Bounds
    <3>. QED BY <3>1, <3>2, <1>0, <1>n
  <2>. QED BY <2>1, <2>2
<1>3. Record'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].derived = seq', w'[u].sStage = "none",
                       NEW p \in Paths, w'[u].baseline[p] # Nil \/ p \in w'[u].scope,
                       doc'[p] # w'[u].baseline[p]
                PROVE  p \in w'[u].skipped
    BY DEF Record, Held
  <2>1. CASE u = t BY <2>1, <1>t DEF RRec, HeldR
  <2>2. CASE u # t
    <3>1. w'[u] = w[u] BY <2>2, <1>k
    <3>2. w[u].derived <= seq BY DEF M3, Bounds
    <3>3. seq' = seq /\ doc' = doc BY <3>1, <3>2, <1>0, <1>n
    <3>. QED BY <3>1, <3>3 DEF M3, Record, Held
  <2>. QED BY <2>1, <2>2
<1>4. CasedMine'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "cased"
                PROVE  \A p \in w'[u].uploads \cap w'[u].upDone : w'[u].inst[p] = w'[u].snap[p]
    BY DEF CasedMine
  <2>1. CASE u = t BY <2>1, <1>t DEF RMine
  <2>2. CASE u # t BY <2>2, <1>k DEF M3, CasedMine
  <2>. QED BY <2>1, <2>2
<1>5. CasedAdv'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "cased", w'[u].adv,
                       NEW p \in Paths \ ((w'[u].uploads \cap w'[u].upDone) \cup w'[u].deletes),
                       w'[u].baseline[p] # Nil \/ p \in w'[u].scope,
                       w'[u].inst[p] # w'[u].baseline[p]
                PROVE  p \in w'[u].skipped
    BY DEF CasedAdv, Held
  <2>1. CASE u = t BY <2>1, <1>t DEF RAdv, HeldR
  <2>2. CASE u # t BY <2>2, <1>k DEF M3, CasedAdv, Held
  <2>. QED BY <2>1, <2>2
<1>6. CasedSame'
  <2>. SUFFICES ASSUME NEW u \in Writers, w'[u].pc = "cased", w'[u].derived = seq'
                PROVE  w'[u].adv /\ w'[u].inst = doc'
    BY DEF CasedSame
  <2>1. CASE u = t BY <2>1, <1>t DEF RSame
  <2>2. CASE u # t
    <3>1. w'[u] = w[u] BY <2>2, <1>k
    <3>2. w[u].derived <= seq BY DEF M3, Bounds
    <3>3. seq' = seq /\ doc' = doc BY <3>1, <3>2, <1>0, <1>n
    <3>. QED BY <3>1, <3>3 DEF M3, CasedSame
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6 DEF M3

\* M3 after a step that moves nothing M3 reads.
LEMMA M3Same ==
  ASSUME M3, UNCHANGED <<doc, seq, w>>
  PROVE  M3'
BY DEF M3, M3Seq, Bounds, Record, CasedMine, CasedAdv, CasedSame, Held

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M3 == Init => IndM3
<1>. SUFFICES ASSUME Init PROVE IndM3 OBVIOUS
<1>1. IndM2 BY Init_M2
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>3. \A s \in Writers : /\ w[s].pc = "idle" /\ w[s].derived = 0 /\ w[s].memo = 0 /\ w[s].rnow = 0
  BY <1>2 DEF WriterInit
<1>4. seq = 1 BY DEF Init
<1>5. M3Seq /\ Bounds BY <1>3, <1>4 DEF M3Seq, Bounds
<1>6. Record BY <1>3, <1>4 DEF Record
<1>7. CasedMine /\ CasedAdv /\ CasedSame BY <1>3 DEF CasedMine, CasedAdv, CasedSame
<1>. QED BY <1>1, <1>5, <1>6, <1>7 DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the gateway and the steps that write no tree.                        *)

LEMMA GPut_M3 ==
  ASSUME IndM3, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GPut_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF GPut
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA GCas_M3 ==
  ASSUME IndM3, NEW p \in Paths, GCas(p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GCas_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ (seq' = seq + 1 \/ (seq' = seq /\ doc' = doc)) BY DEF GCas
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA GRename_M3 ==
  ASSUME IndM3, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GRename_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ seq' = seq + 1 BY DEF GRename
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA GRenameFinish_M3 ==
  ASSUME IndM3, GRenameFinish, Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GRenameFinish_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ seq' = seq + 1 BY DEF GRenameFinish
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA GDelete_M3 ==
  ASSUME IndM3, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, GDelete_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ seq' = seq + 1 BY DEF GDelete
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA Sweep_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Sweep_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF Sweep
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA Reap_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Reap_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF Reap
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA Age_M3 ==
  ASSUME IndM3, Age, UNCHANGED anc
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Age_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF Age
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

LEMMA RLoad_M3 ==
  ASSUME IndM3, RLoad, UNCHANGED anc
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RLoad_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ UNCHANGED <<doc, seq>> BY DEF RLoad
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>2. Wr("none", w) BY <1>1, WrNone
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>e, <1>2, NoneWriter, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the agent and the commit section.                                    *)

LEMMA Edit_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Edit_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] /\ UNCHANGED <<doc, seq>> BY DEF Edit
<1>g. On(s) BY DEF Edit
<1>2. Wr(s, EditW(s, p)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ EditW(s, p).pc = w[s].pc
      /\ EditW(s, p).derived = w[s].derived
      /\ EditW(s, p).memo = w[s].memo
      /\ EditW(s, p).rnow = w[s].rnow
      /\ EditW(s, p).sStage = w[s].sStage
      /\ EditW(s, p).scope = w[s].scope
      /\ EditW(s, p).skipped = w[s].skipped
  BY DEF EditW
<1>3b. /\ EditW(s, p).baseline = w[s].baseline
      /\ EditW(s, p).uploads = w[s].uploads
      /\ EditW(s, p).upDone = w[s].upDone
      /\ EditW(s, p).deletes = w[s].deletes
      /\ EditW(s, p).snap = w[s].snap
      /\ EditW(s, p).inst = w[s].inst
      /\ EditW(s, p).adv = w[s].adv
  BY DEF EditW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(EditW(s, p)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(EditW(s, p)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(EditW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(EditW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(EditW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Delete_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Delete_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] /\ UNCHANGED <<doc, seq>> BY DEF Delete, bucket
<1>g. On(s) BY DEF Delete
<1>2. Wr(s, DeleteW(s, p)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ DeleteW(s, p).pc = w[s].pc
      /\ DeleteW(s, p).derived = w[s].derived
      /\ DeleteW(s, p).memo = w[s].memo
      /\ DeleteW(s, p).rnow = w[s].rnow
      /\ DeleteW(s, p).sStage = w[s].sStage
      /\ DeleteW(s, p).scope = w[s].scope
      /\ DeleteW(s, p).skipped = w[s].skipped
  BY DEF DeleteW
<1>3b. /\ DeleteW(s, p).baseline = w[s].baseline
      /\ DeleteW(s, p).uploads = w[s].uploads
      /\ DeleteW(s, p).upDone = w[s].upDone
      /\ DeleteW(s, p).deletes = w[s].deletes
      /\ DeleteW(s, p).snap = w[s].snap
      /\ DeleteW(s, p).inst = w[s].inst
      /\ DeleteW(s, p).adv = w[s].adv
  BY DEF DeleteW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(DeleteW(s, p)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(DeleteW(s, p)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(DeleteW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(DeleteW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(DeleteW(s, p)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Checkout_M3 ==
  ASSUME IndM3, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Checkout_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] /\ UNCHANGED <<doc, seq>> BY DEF Checkout, bucket
<1>g. w[s].st = "off" BY DEF Checkout
<1>2. Wr(s, CheckoutW(s, T)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ CheckoutW(s, T).pc = w[s].pc
      /\ CheckoutW(s, T).derived = seq
      /\ CheckoutW(s, T).memo = w[s].memo
      /\ CheckoutW(s, T).rnow = w[s].rnow
      /\ CheckoutW(s, T).sStage = w[s].sStage
      /\ CheckoutW(s, T).scope = T
      /\ CheckoutW(s, T).skipped = {}
  BY DEF CheckoutW
<1>3b. /\ CheckoutW(s, T).baseline = CheckoutHeld(s, T)
      /\ CheckoutW(s, T).uploads = w[s].uploads
      /\ CheckoutW(s, T).upDone = w[s].upDone
      /\ CheckoutW(s, T).deletes = w[s].deletes
      /\ CheckoutW(s, T).snap = w[s].snap
      /\ CheckoutW(s, T).inst = doc
      /\ CheckoutW(s, T).adv = w[s].adv
  BY DEF CheckoutW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(CheckoutW(s, T)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(CheckoutW(s, T)) BY <1>1, <1>3, <1>3b DEF RRec, HeldR, CheckoutHeld
<1>6. RMine(CheckoutW(s, T)) BY <1>3, <1>g, <1>m DEF RMine, Off, WriterInit
<1>7. RAdv(CheckoutW(s, T)) BY <1>3, <1>g, <1>m DEF RAdv, Off, WriterInit
<1>8. RSame(CheckoutW(s, T)) BY <1>3, <1>g, <1>m DEF RSame, Off, WriterInit
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Consume_M3 ==
  ASSUME IndM3, NEW s \in Writers, Consume(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Consume_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>s. ConsumeKeepsLeft /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s)] /\ UNCHANGED <<doc, seq>> BY <1>1 DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" BY DEF Consume
  <2>2. Wr(s, ConsumeCheapW(s)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ ConsumeCheapW(s).pc = "consumed"
        /\ ConsumeCheapW(s).derived = w[s].derived
        /\ ConsumeCheapW(s).memo = w[s].memo
        /\ ConsumeCheapW(s).rnow = w[s].rnow
        /\ ConsumeCheapW(s).sStage = w[s].sStage
        /\ ConsumeCheapW(s).scope = w[s].scope
        /\ ConsumeCheapW(s).skipped = w[s].skipped
    BY DEF ConsumeCheapW
  <2>3b. /\ ConsumeCheapW(s).baseline = w[s].baseline
        /\ ConsumeCheapW(s).uploads = w[s].uploads
        /\ ConsumeCheapW(s).upDone = w[s].upDone
        /\ ConsumeCheapW(s).deletes = w[s].deletes
        /\ ConsumeCheapW(s).snap = w[s].snap
        /\ ConsumeCheapW(s).inst = w[s].inst
        /\ ConsumeCheapW(s).adv = w[s].adv
    BY DEF ConsumeCheapW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(ConsumeCheapW(s)) BY <2>1, <2>3, <2>3b, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(ConsumeCheapW(s)) BY <2>1, <2>3, <2>3b, <1>c DEF RRec, Record, HeldR, Held
  <2>6. RMine(ConsumeCheapW(s)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(ConsumeCheapW(s)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(ConsumeCheapW(s)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>2. CASE ~CheapPath(s)
  <2>1. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail)] /\ UNCHANGED <<doc, seq>> BY <1>2 DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" BY DEF Consume
  <2>2. Wr(s, ConsumeW(s, fail)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ ConsumeW(s, fail).pc = "consumed"
        /\ ConsumeW(s, fail).derived = IF fail # {} /\ ConsumeKeepsLeft THEN 0 ELSE seq
        /\ ConsumeW(s, fail).memo = w[s].memo
        /\ ConsumeW(s, fail).rnow = w[s].rnow
        /\ ConsumeW(s, fail).sStage = w[s].sStage
        /\ ConsumeW(s, fail).scope = w[s].scope
        /\ ConsumeW(s, fail).skipped = {q \in Paths \ ConsumeTaken(s, fail) : doc[q] # w[s].baseline[q] /\ Held(s, q)
                                              /\ w[s].local[q] # w[s].baseline[q]}
    BY DEF ConsumeW
  <2>3b. /\ ConsumeW(s, fail).baseline = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]]
        /\ ConsumeW(s, fail).uploads = w[s].uploads
        /\ ConsumeW(s, fail).upDone = w[s].upDone
        /\ ConsumeW(s, fail).deletes = w[s].deletes
        /\ ConsumeW(s, fail).snap = w[s].snap
        /\ ConsumeW(s, fail).inst = w[s].inst
        /\ ConsumeW(s, fail).adv = w[s].adv
    BY DEF ConsumeW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(ConsumeW(s, fail)) BY <2>1, <2>3, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(ConsumeW(s, fail)) 
    <3>1. ASSUME ConsumeW(s, fail).derived = seq' PROVE fail = {}
      BY <3>1, <2>1, <2>3, <1>s, <1>c DEF M3Seq
    <3>2. ASSUME NEW p \in Paths, fail = {},
                 HeldR(ConsumeW(s, fail), p), doc'[p] # ConsumeW(s, fail).baseline[p]
          PROVE  p \in ConsumeW(s, fail).skipped
      <4>1. p \notin ConsumeTaken(s, fail) BY <3>2, <2>1, <2>3b
      <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF ConsumeTaken, ConsumeOwed
      <4>3. ConsumeW(s, fail).baseline[p] = w[s].baseline[p] BY <4>1, <2>3b
      <4>4. Held(s, p) BY <3>2, <4>3, <2>3 DEF HeldR, Held
      <4>5. w[s].local[p] # w[s].baseline[p] BY <4>2, <4>4, <4>3, <3>2, <2>1, <1>s DEF Owed
      <4>. QED BY <4>1, <4>3, <4>4, <4>5, <3>2, <2>1, <2>3
    <3>. QED BY <3>1, <3>2 DEF RRec
  <2>6. RMine(ConsumeW(s, fail)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(ConsumeW(s, fail)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(ConsumeW(s, fail)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>. QED BY <1>1, <1>2

LEMMA Scan_M3 ==
  ASSUME IndM3, NEW s \in Writers, Scan(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Scan_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. PICK dels \in SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] /\ UNCHANGED <<doc, seq>> BY DEF Scan, bucket
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Scan
<1>2. Wr(s, ScanW(s, dels)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ ScanW(s, dels).pc = "scanned"
      /\ ScanW(s, dels).derived = w[s].derived
      /\ ScanW(s, dels).memo = w[s].memo
      /\ ScanW(s, dels).rnow = w[s].rnow
      /\ ScanW(s, dels).sStage = w[s].sStage
      /\ ScanW(s, dels).scope = w[s].scope
      /\ ScanW(s, dels).skipped = w[s].skipped
  BY DEF ScanW
<1>3b. /\ ScanW(s, dels).baseline = w[s].baseline
      /\ ScanW(s, dels).inst = w[s].inst
      /\ ScanW(s, dels).adv = w[s].adv
  BY DEF ScanW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(ScanW(s, dels)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(ScanW(s, dels)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(ScanW(s, dels)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(ScanW(s, dels)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(ScanW(s, dels)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Skip_M3 ==
  ASSUME IndM3, NEW s \in Writers, Skip(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Skip_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Skip, bucket
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Skip
<1>2. Wr(s, SkipW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ SkipW(s).pc = "idle"
      /\ SkipW(s).derived = w[s].derived
      /\ SkipW(s).memo = w[s].memo
      /\ SkipW(s).rnow = w[s].rnow
      /\ SkipW(s).sStage = w[s].sStage
      /\ SkipW(s).scope = w[s].scope
      /\ SkipW(s).skipped = w[s].skipped
  BY DEF SkipW
<1>3b. /\ SkipW(s).baseline = w[s].baseline
      /\ SkipW(s).uploads = w[s].uploads
      /\ SkipW(s).upDone = w[s].upDone
      /\ SkipW(s).deletes = w[s].deletes
      /\ SkipW(s).snap = w[s].snap
      /\ SkipW(s).inst = w[s].inst
      /\ SkipW(s).adv = w[s].adv
  BY DEF SkipW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(SkipW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(SkipW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(SkipW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(SkipW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(SkipW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Upload_M3 ==
  ASSUME IndM3, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Upload_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] /\ UNCHANGED <<doc, seq>> BY <1>1 DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" BY DEF Upload
  <2>2. Wr(s, UploadW(s, p)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ UploadW(s, p).pc = w[s].pc
        /\ UploadW(s, p).derived = w[s].derived
        /\ UploadW(s, p).memo = w[s].memo
        /\ UploadW(s, p).rnow = w[s].rnow
        /\ UploadW(s, p).sStage = w[s].sStage
        /\ UploadW(s, p).scope = w[s].scope
        /\ UploadW(s, p).skipped = w[s].skipped
    BY DEF UploadW
  <2>3b. /\ UploadW(s, p).baseline = w[s].baseline
        /\ UploadW(s, p).uploads = w[s].uploads
        /\ UploadW(s, p).deletes = w[s].deletes
        /\ UploadW(s, p).snap = w[s].snap
        /\ UploadW(s, p).inst = w[s].inst
        /\ UploadW(s, p).adv = w[s].adv
    BY DEF UploadW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(UploadW(s, p)) BY <2>1, <2>3, <2>3b, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(UploadW(s, p)) BY <2>1, <2>3, <2>3b, <1>c DEF RRec, Record, HeldR, Held
  <2>6. RMine(UploadW(s, p)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(UploadW(s, p)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(UploadW(s, p)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>1. w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] /\ UNCHANGED <<doc, seq>> BY <1>2 DEF Upload
  <2>g. On(s) /\ w[s].pc = "scanned" BY DEF Upload
  <2>2. Wr(s, UploadCopyW(s, p, c)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. /\ UploadCopyW(s, p, c).pc = w[s].pc
        /\ UploadCopyW(s, p, c).derived = w[s].derived
        /\ UploadCopyW(s, p, c).memo = w[s].memo
        /\ UploadCopyW(s, p, c).rnow = w[s].rnow
        /\ UploadCopyW(s, p, c).sStage = w[s].sStage
        /\ UploadCopyW(s, p, c).scope = w[s].scope
        /\ UploadCopyW(s, p, c).skipped = w[s].skipped
    BY DEF UploadCopyW
  <2>3b. /\ UploadCopyW(s, p, c).baseline = w[s].baseline
        /\ UploadCopyW(s, p, c).uploads = w[s].uploads
        /\ UploadCopyW(s, p, c).deletes = w[s].deletes
        /\ UploadCopyW(s, p, c).inst = w[s].inst
        /\ UploadCopyW(s, p, c).adv = w[s].adv
    BY DEF UploadCopyW
  <2>e. SeqEv BY <2>1, <1>y DEF SeqEv
  <2>4. RB(UploadCopyW(s, p, c)) BY <2>1, <2>3, <2>3b, <1>c, <1>f, <1>y DEF RB, Bounds
  <2>5. RRec(UploadCopyW(s, p, c)) BY <2>1, <2>3, <2>3b, <1>c DEF RRec, Record, HeldR, Held
  <2>6. RMine(UploadCopyW(s, p, c)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RMine, CasedMine
  <2>7. RAdv(UploadCopyW(s, p, c)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
  <2>8. RSame(UploadCopyW(s, p, c)) BY <2>1, <2>g, <2>3, <2>3b, <1>c DEF RSame, CasedSame
  <2>. QED BY <1>a, <1>c, <1>y, <1>t2, <2>2, <2>e, <2>4, <2>5, <2>6, <2>7, <2>8, M3Write DEF IndM3, M3
<1>. QED BY <1>1, <1>2

LEMMA PullOnly_M3 ==
  ASSUME IndM3, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, PullOnly_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] /\ UNCHANGED <<doc, seq>> BY DEF PullOnly, bucket
<1>g. On(s) /\ w[s].pc = "scanned" BY DEF PullOnly
<1>2. Wr(s, PullOnlyW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ PullOnlyW(s).pc = "idle"
      /\ PullOnlyW(s).derived = w[s].derived
      /\ PullOnlyW(s).memo = w[s].memo
      /\ PullOnlyW(s).rnow = w[s].rnow
      /\ PullOnlyW(s).sStage = w[s].sStage
      /\ PullOnlyW(s).scope = w[s].scope
      /\ PullOnlyW(s).skipped = w[s].skipped
  BY DEF PullOnlyW
<1>3b. /\ PullOnlyW(s).baseline = w[s].baseline
      /\ PullOnlyW(s).uploads = w[s].uploads
      /\ PullOnlyW(s).deletes = w[s].deletes
      /\ PullOnlyW(s).adv = w[s].adv
  BY DEF PullOnlyW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(PullOnlyW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(PullOnlyW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(PullOnlyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(PullOnlyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(PullOnlyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Claim_M3 ==
  ASSUME IndM3, NEW s \in Writers, Claim(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Claim_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Claim
<1>g. On(s) /\ w[s].pc = "scanned" BY DEF Claim
<1>2. Wr(s, ClaimW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ ClaimW(s).pc = "claimed"
      /\ ClaimW(s).derived = w[s].derived
      /\ ClaimW(s).memo = w[s].memo
      /\ ClaimW(s).rnow = w[s].rnow
      /\ ClaimW(s).sStage = w[s].sStage
      /\ ClaimW(s).scope = w[s].scope
      /\ ClaimW(s).skipped = w[s].skipped
  BY DEF ClaimW
<1>3b. /\ ClaimW(s).baseline = w[s].baseline
      /\ ClaimW(s).uploads = w[s].uploads
      /\ ClaimW(s).upDone = w[s].upDone
      /\ ClaimW(s).deletes = w[s].deletes
      /\ ClaimW(s).snap = w[s].snap
      /\ ClaimW(s).inst = w[s].inst
      /\ ClaimW(s).adv = w[s].adv
  BY DEF ClaimW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(ClaimW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(ClaimW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(ClaimW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(ClaimW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(ClaimW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Verify_M3 ==
  ASSUME IndM3, NEW s \in Writers, Verify(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Verify_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Verify, bucket
<1>g. On(s) /\ w[s].pc = "claimed" BY DEF Verify
<1>2. Wr(s, VerifyW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ VerifyW(s).pc = w[s].pc
      /\ VerifyW(s).derived = w[s].derived
      /\ VerifyW(s).memo = w[s].memo
      /\ VerifyW(s).rnow = w[s].rnow
      /\ VerifyW(s).sStage = w[s].sStage
      /\ VerifyW(s).scope = w[s].scope
      /\ VerifyW(s).skipped = w[s].skipped
  BY DEF VerifyW
<1>3b. /\ VerifyW(s).baseline = w[s].baseline
      /\ VerifyW(s).uploads = w[s].uploads
      /\ VerifyW(s).upDone = w[s].upDone
      /\ VerifyW(s).deletes = w[s].deletes
      /\ VerifyW(s).snap = w[s].snap
      /\ VerifyW(s).inst = w[s].inst
      /\ VerifyW(s).adv = w[s].adv
  BY DEF VerifyW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(VerifyW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(VerifyW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(VerifyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(VerifyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(VerifyW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Install_M3 ==
  ASSUME IndM3, NEW s \in Writers, Install(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Install_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>s. CommitAdvanceGuarded BY ShippedShape DEF Shipped
<1>g0. w[s].pc = "claimed" BY DEF Install
<1>r1. w[s].sStage = "none" BY <1>m, <1>g0 DEF R1
<1>ii. \A k \in Paths : k \notin (w[s].uploads \cap (w[s].upDone \ w[s].gone)) \cup w[s].deletes
                       => InstallInst(s)[k] = doc[k]
  BY DEF InstallInst, InstallMine
<1>1. /\ doc' = InstallInst(s) /\ w' = [w EXCEPT ![s] = InstallW(s)]
      /\ seq' = IF InstallInst(s) = doc THEN seq ELSE seq + 1
  BY DEF Install
<1>g. On(s) /\ w[s].pc = "claimed" BY DEF Install
<1>2. Wr(s, InstallW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ InstallW(s).pc = "cased"
      /\ InstallW(s).derived = w[s].derived
      /\ InstallW(s).memo = w[s].memo
      /\ InstallW(s).rnow = w[s].rnow
      /\ InstallW(s).sStage = w[s].sStage
      /\ InstallW(s).scope = w[s].scope
      /\ InstallW(s).skipped = w[s].skipped
  BY DEF InstallW
<1>3b. /\ InstallW(s).baseline = w[s].baseline
      /\ InstallW(s).uploads = w[s].uploads
      /\ InstallW(s).upDone = w[s].upDone \ w[s].gone
      /\ InstallW(s).deletes = w[s].deletes
      /\ InstallW(s).snap = w[s].snap
      /\ InstallW(s).inst = InstallInst(s)
      /\ InstallW(s).adv = IF CommitAdvanceGuarded THEN seq = w[s].derived ELSE TRUE
  BY DEF InstallW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(InstallW(s)) BY <1>1, <1>3, <1>e, <1>c, <1>f, <1>y DEF RB, Bounds, SeqEv
<1>5. RRec(InstallW(s)) 
  <2>1. ASSUME InstallW(s).derived = seq' PROVE seq' = seq /\ doc' = doc
    BY <2>1, <1>3, <1>1, <1>e, <1>c, <1>f, <1>y DEF Bounds, SeqEv
  <2>. QED BY <2>1, <1>3, <1>3b, <1>r1, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(InstallW(s)) 
  <2>. SUFFICES ASSUME NEW p \in w[s].uploads \cap (w[s].upDone \ w[s].gone) PROVE InstallInst(s)[p] = w[s].snap[p]
    BY <1>3, <1>3b DEF RMine
  <2>1. p \in Paths /\ p \notin w[s].gone /\ p \in InstallMine(s) BY <1>f DEF InstallMine
  <2>. QED BY <2>1 DEF InstallInst
<1>7. RAdv(InstallW(s)) 
  <2>1. ASSUME InstallW(s).adv PROVE w[s].derived = seq BY <2>1, <1>3b, <1>s
  <2>2. ASSUME InstallW(s).adv,
               NEW p \in Paths \ ((InstallW(s).uploads \cap InstallW(s).upDone) \cup InstallW(s).deletes),
               HeldR(InstallW(s), p), InstallW(s).inst[p] # InstallW(s).baseline[p]
        PROVE  p \in InstallW(s).skipped
    <3>1. InstallInst(s)[p] = doc[p] BY <2>2, <1>3b, <1>ii
    <3>2. Held(s, p) /\ doc[p] # w[s].baseline[p] BY <2>2, <3>1, <1>3, <1>3b DEF HeldR, Held
    <3>. QED BY <2>1, <2>2, <3>2, <1>3, <1>r1, <1>c DEF Record
  <2>. QED BY <2>1, <2>2 DEF RAdv
<1>8. RSame(InstallW(s)) 
  <2>1. ASSUME InstallW(s).derived = seq' PROVE seq' = seq /\ w[s].derived = seq
    BY <2>1, <1>3, <1>1, <1>e, <1>c, <1>f, <1>y DEF Bounds, SeqEv
  <2>. QED BY <2>1, <1>1, <1>3, <1>3b, <1>s DEF RSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Collect_M3 ==
  ASSUME IndM3, NEW s \in Writers, Collect(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Collect_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Collect
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Collect
<1>2. Wr(s, CollectW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ CollectW(s).pc = w[s].pc
      /\ CollectW(s).derived = w[s].derived
      /\ CollectW(s).memo = w[s].memo
      /\ CollectW(s).rnow = w[s].rnow
      /\ CollectW(s).sStage = w[s].sStage
      /\ CollectW(s).scope = w[s].scope
      /\ CollectW(s).skipped = w[s].skipped
  BY DEF CollectW
<1>3b. /\ CollectW(s).baseline = w[s].baseline
      /\ CollectW(s).uploads = w[s].uploads
      /\ CollectW(s).upDone = w[s].upDone
      /\ CollectW(s).deletes = w[s].deletes
      /\ CollectW(s).snap = w[s].snap
      /\ CollectW(s).inst = w[s].inst
      /\ CollectW(s).adv = w[s].adv
  BY DEF CollectW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(CollectW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(CollectW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(CollectW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(CollectW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(CollectW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Finish_M3 ==
  ASSUME IndM3, NEW s \in Writers, Finish(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Finish_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Finish
<1>g. On(s) /\ w[s].pc = "cased" BY DEF Finish
<1>2. Wr(s, FinishW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ FinishW(s).pc = "idle"
      /\ FinishW(s).derived = IF w[s].adv /\ w[s].inst = doc THEN seq ELSE w[s].derived
      /\ FinishW(s).memo = w[s].memo
      /\ FinishW(s).rnow = w[s].rnow
      /\ FinishW(s).sStage = w[s].sStage
      /\ FinishW(s).scope = w[s].scope
      /\ FinishW(s).skipped = IF w[s].adv /\ w[s].inst = doc
                 THEN w[s].skipped \ ((w[s].uploads \cap w[s].upDone) \cup {q \in w[s].deletes : w[s].inst[q] = Nil})
                 ELSE w[s].skipped
  BY DEF FinishW
<1>3b. /\ FinishW(s).baseline = [q \in Paths |-> IF q \in w[s].uploads \cap w[s].upDone THEN w[s].snap[q]
                                ELSE IF q \in w[s].deletes /\ w[s].inst[q] = Nil THEN Nil
                                ELSE w[s].baseline[q]]
      /\ FinishW(s).inst = w[s].inst
  BY DEF FinishW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(FinishW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(FinishW(s)) 
  <2>1. w[s].sStage = "none" /\ w[s].pc = "cased" BY <1>g, <1>m DEF R1
  <2>2. CASE w[s].adv /\ w[s].inst = doc
    <3>. SUFFICES ASSUME NEW p \in Paths, HeldR(FinishW(s), p), doc[p] # FinishW(s).baseline[p]
                  PROVE  p \in FinishW(s).skipped
      BY <1>1 DEF RRec
    <3>1. p \notin w[s].uploads \cap w[s].upDone
      BY <2>1, <2>2, <1>3b, <1>c DEF CasedMine
    <3>2. p \notin w[s].deletes
      BY <2>1, <2>2, <3>1, <1>3b, <1>m DEF Cased
    <3>3. FinishW(s).baseline[p] = w[s].baseline[p] BY <3>1, <3>2, <1>3b
    <3>4. Held(s, p) /\ w[s].inst[p] # w[s].baseline[p] BY <2>2, <3>3, <1>3 DEF HeldR, Held
    <3>5. p \in w[s].skipped BY <2>1, <2>2, <3>1, <3>2, <3>4, <1>c DEF CasedAdv
    <3>. QED BY <2>2, <3>1, <3>2, <3>5, <1>3
  <2>3. CASE ~(w[s].adv /\ w[s].inst = doc)
    <3>1. w[s].derived # seq BY <2>1, <2>3, <1>c DEF CasedSame
    <3>. QED BY <2>3, <3>1, <1>1, <1>3 DEF RRec
  <2>. QED BY <2>2, <2>3
<1>6. RMine(FinishW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(FinishW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(FinishW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the restart and the sync.                                            *)

LEMMA Restart_M3 ==
  ASSUME IndM3, NEW s \in Writers, Restart(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Restart_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] /\ UNCHANGED <<doc, seq>> BY DEF Restart
<1>g. On(s) BY DEF Restart
<1>2. Wr(s, RestartW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RestartW(s).pc = "idle"
      /\ RestartW(s).derived = w[s].derived
      /\ RestartW(s).memo = w[s].memo
      /\ RestartW(s).rnow = w[s].rnow
      /\ RestartW(s).sStage = IF w[s].sStage = "none" THEN "none" ELSE "saved"
      /\ RestartW(s).scope = w[s].scope
      /\ RestartW(s).skipped = w[s].skipped
  BY DEF RestartW
<1>3b. /\ RestartW(s).baseline = w[s].baseline
  BY DEF RestartW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RestartW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RestartW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(RestartW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RestartW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RestartW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA Sync_M3 ==
  ASSUME IndM3, NEW s \in Writers, Sync(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, Sync_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>s. SyncKeepsLeft /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] /\ UNCHANGED <<doc, seq>> BY DEF Sync, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF Sync
<1>2. Wr(s, SyncW(s, fail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ SyncW(s, fail).pc = w[s].pc
      /\ SyncW(s, fail).derived = IF fail # {} /\ SyncKeepsLeft THEN 0 ELSE seq
      /\ SyncW(s, fail).memo = w[s].memo
      /\ SyncW(s, fail).rnow = w[s].rnow
      /\ SyncW(s, fail).sStage = w[s].sStage
      /\ SyncW(s, fail).scope = w[s].scope
      /\ SyncW(s, fail).skipped = IF fail # {} /\ SyncKeepsLeft THEN {}
              ELSE {q \in Paths : doc[q] # SyncBl(s, fail)[q] /\ w[s].local[q] # SyncBl(s, fail)[q] /\ Held(s, q)}
  BY DEF SyncW
<1>3b. /\ SyncW(s, fail).baseline = SyncBl(s, fail)
      /\ SyncW(s, fail).uploads = w[s].uploads
      /\ SyncW(s, fail).upDone = w[s].upDone
      /\ SyncW(s, fail).deletes = w[s].deletes
      /\ SyncW(s, fail).snap = w[s].snap
      /\ SyncW(s, fail).inst = w[s].inst
      /\ SyncW(s, fail).adv = w[s].adv
  BY DEF SyncW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(SyncW(s, fail)) BY <1>1, <1>3, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(SyncW(s, fail)) 
  <3>1. ASSUME SyncW(s, fail).derived = seq' PROVE fail = {}
    BY <3>1, <1>1, <1>3, <1>s, <1>c DEF M3Seq
  <3>2. ASSUME NEW p \in Paths, fail = {},
               HeldR(SyncW(s, fail), p), doc'[p] # SyncW(s, fail).baseline[p]
        PROVE  p \in SyncW(s, fail).skipped
    <4>1. p \notin SyncOwed(s, fail) BY <3>2, <1>1, <1>3b DEF SyncBl
    <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF SyncOwed, SyncAll
    <4>3. SyncW(s, fail).baseline[p] = w[s].baseline[p] BY <4>1, <1>3b DEF SyncBl
    <4>4. Held(s, p) BY <3>2, <4>3, <1>3 DEF HeldR, Held
    <4>5. w[s].local[p] # w[s].baseline[p] BY <4>2, <4>4, <4>3, <3>2, <1>1, <1>s DEF Owed
    <4>. QED BY <4>1, <4>3, <4>4, <4>5, <3>2, <1>1, <1>3 DEF SyncBl
  <3>. QED BY <3>1, <3>2 DEF RRec
<1>6. RMine(SyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(SyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(SyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the narrow / widen verb.                                             *)

LEMMA RescopeBegin_M3 ==
  ASSUME IndM3, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RescopeBegin_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] /\ UNCHANGED <<doc, seq>> BY DEF RescopeBegin, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RescopeBegin
<1>2. Wr(s, RescopeBeginW(s, T)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RescopeBeginW(s, T).pc = w[s].pc
      /\ RescopeBeginW(s, T).derived = w[s].derived
      /\ RescopeBeginW(s, T).memo = w[s].memo
      /\ RescopeBeginW(s, T).rnow = w[s].rnow
      /\ RescopeBeginW(s, T).sStage = "saved"
      /\ RescopeBeginW(s, T).scope = w[s].scope
      /\ RescopeBeginW(s, T).skipped = w[s].skipped
  BY DEF RescopeBeginW
<1>3b. /\ RescopeBeginW(s, T).baseline = w[s].baseline
      /\ RescopeBeginW(s, T).uploads = w[s].uploads
      /\ RescopeBeginW(s, T).upDone = w[s].upDone
      /\ RescopeBeginW(s, T).deletes = w[s].deletes
      /\ RescopeBeginW(s, T).snap = w[s].snap
      /\ RescopeBeginW(s, T).inst = w[s].inst
      /\ RescopeBeginW(s, T).adv = w[s].adv
  BY DEF RescopeBeginW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RescopeBeginW(s, T)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RescopeBeginW(s, T)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(RescopeBeginW(s, T)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RescopeBeginW(s, T)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RescopeBeginW(s, T)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA RescopeFirst_M3 ==
  ASSUME IndM3, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RescopeFirst_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] /\ UNCHANGED <<doc, seq>> BY DEF RescopeFirst, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RescopeFirst
<1>2. Wr(s, RescopeFirstW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RescopeFirstW(s).pc = w[s].pc
      /\ RescopeFirstW(s).derived = w[s].derived
      /\ RescopeFirstW(s).memo = w[s].memo
      /\ RescopeFirstW(s).rnow = w[s].rnow
      /\ RescopeFirstW(s).sStage = "mid"
      /\ RescopeFirstW(s).scope = w[s].scope
      /\ RescopeFirstW(s).skipped = w[s].skipped
  BY DEF RescopeFirstW
<1>3b. /\ RescopeFirstW(s).uploads = w[s].uploads
      /\ RescopeFirstW(s).upDone = w[s].upDone
      /\ RescopeFirstW(s).deletes = w[s].deletes
      /\ RescopeFirstW(s).snap = w[s].snap
      /\ RescopeFirstW(s).inst = w[s].inst
      /\ RescopeFirstW(s).adv = w[s].adv
  BY DEF RescopeFirstW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RescopeFirstW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RescopeFirstW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(RescopeFirstW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RescopeFirstW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RescopeFirstW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA RescopeSecond_M3 ==
  ASSUME IndM3, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RescopeSecond_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] /\ UNCHANGED <<doc, seq>>
  BY DEF RescopeSecond, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RescopeSecond
<1>2. Wr(s, RescopeSecondW(s, wfail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RescopeSecondW(s, wfail).pc = w[s].pc
      /\ RescopeSecondW(s, wfail).derived = 0
      /\ RescopeSecondW(s, wfail).memo = w[s].memo
      /\ RescopeSecondW(s, wfail).rnow = w[s].rnow
      /\ RescopeSecondW(s, wfail).sStage = "none"
      /\ RescopeSecondW(s, wfail).skipped = {}
  BY DEF RescopeSecondW
<1>3b. /\ RescopeSecondW(s, wfail).uploads = w[s].uploads
      /\ RescopeSecondW(s, wfail).upDone = w[s].upDone
      /\ RescopeSecondW(s, wfail).deletes = w[s].deletes
      /\ RescopeSecondW(s, wfail).snap = w[s].snap
      /\ RescopeSecondW(s, wfail).inst = w[s].inst
      /\ RescopeSecondW(s, wfail).adv = w[s].adv
  BY DEF RescopeSecondW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RescopeSecondW(s, wfail)) BY <1>1, <1>3, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RescopeSecondW(s, wfail)) BY <1>1, <1>3, <1>c DEF RRec, M3Seq
<1>6. RMine(RescopeSecondW(s, wfail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RescopeSecondW(s, wfail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RescopeSecondW(s, wfail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: a reader's tick.                                                     *)

LEMMA RPullRead_M3 ==
  ASSUME IndM3, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RPullRead_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] /\ UNCHANGED <<doc, seq>> BY DEF RPullRead, bucket
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RPullRead
<1>2. Wr(s, RPullReadW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RPullReadW(s).pc = "pulling"
      /\ RPullReadW(s).derived = w[s].derived
      /\ RPullReadW(s).memo = w[s].memo
      /\ RPullReadW(s).rnow = seq
      /\ RPullReadW(s).sStage = w[s].sStage
      /\ RPullReadW(s).scope = w[s].scope
      /\ RPullReadW(s).skipped = w[s].skipped
  BY DEF RPullReadW
<1>3b. /\ RPullReadW(s).baseline = w[s].baseline
      /\ RPullReadW(s).uploads = w[s].uploads
      /\ RPullReadW(s).upDone = w[s].upDone
      /\ RPullReadW(s).deletes = w[s].deletes
      /\ RPullReadW(s).snap = w[s].snap
      /\ RPullReadW(s).inst = w[s].inst
      /\ RPullReadW(s).adv = w[s].adv
  BY DEF RPullReadW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RPullReadW(s)) BY <1>1, <1>3, <1>3b, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RPullReadW(s)) BY <1>1, <1>3, <1>3b, <1>c DEF RRec, Record, HeldR, Held
<1>6. RMine(RPullReadW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RPullReadW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RPullReadW(s)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

LEMMA RPullSync_M3 ==
  ASSUME IndM3, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndM3'
<1>a0. IndM2 BY DEF IndM3
<1>a. IndM2' BY <1>a0, RPullSync_M2
<1>c. M3Seq /\ Bounds /\ Record /\ CasedMine /\ CasedAdv /\ CasedSame BY DEF IndM3, M3
<1>y. /\ w \in [Writers -> Writer] /\ seq \in Nat /\ doc \in [Paths -> Opt(Handles)] /\ TypeOK
  BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>t2. w' \in [Writers -> Writer] BY <1>a DEF IndM2, IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>y
<1>f. /\ w[s].derived \in Nat /\ w[s].memo \in Nat /\ w[s].rnow \in Nat
      /\ w[s].baseline \in [Paths -> Opt(Handles)] /\ w[s].local \in [Paths -> Opt(Handles)]
      /\ w[s].scope \subseteq Paths /\ w[s].skipped \subseteq Paths
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
  BY <1>w, WriterFields
<1>m. Cased /\ Off /\ R1 BY DEF IndM3, IndM2, IndM1, M1, Rescope
<1>s. SyncKeepsLeft /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] /\ UNCHANGED <<doc, seq>> BY DEF RPullSync, bucket
<1>g. On(s) /\ w[s].pc = "pulling" BY DEF RPullSync
<1>2. Wr(s, RPullSyncW(s, fail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. /\ RPullSyncW(s, fail).pc = "idle"
      /\ RPullSyncW(s, fail).derived = IF fail # {} /\ SyncKeepsLeft THEN 0 ELSE seq
      /\ RPullSyncW(s, fail).memo = w[s].rnow
      /\ RPullSyncW(s, fail).rnow = w[s].rnow
      /\ RPullSyncW(s, fail).sStage = w[s].sStage
      /\ RPullSyncW(s, fail).scope = w[s].scope
      /\ RPullSyncW(s, fail).skipped = IF fail # {} /\ SyncKeepsLeft THEN {}
              ELSE {q \in Paths : doc[q] # SyncBl(s, fail)[q] /\ w[s].local[q] # SyncBl(s, fail)[q] /\ Held(s, q)}
  BY DEF RPullSyncW
<1>3b. /\ RPullSyncW(s, fail).baseline = SyncBl(s, fail)
      /\ RPullSyncW(s, fail).uploads = w[s].uploads
      /\ RPullSyncW(s, fail).upDone = w[s].upDone
      /\ RPullSyncW(s, fail).deletes = w[s].deletes
      /\ RPullSyncW(s, fail).snap = w[s].snap
      /\ RPullSyncW(s, fail).inst = w[s].inst
      /\ RPullSyncW(s, fail).adv = w[s].adv
  BY DEF RPullSyncW
<1>e. SeqEv BY <1>1, <1>y DEF SeqEv
<1>4. RB(RPullSyncW(s, fail)) BY <1>1, <1>3, <1>c, <1>f, <1>y DEF RB, Bounds
<1>5. RRec(RPullSyncW(s, fail)) 
  <3>1. ASSUME RPullSyncW(s, fail).derived = seq' PROVE fail = {}
    BY <3>1, <1>1, <1>3, <1>s, <1>c DEF M3Seq
  <3>2. ASSUME NEW p \in Paths, fail = {},
               HeldR(RPullSyncW(s, fail), p), doc'[p] # RPullSyncW(s, fail).baseline[p]
        PROVE  p \in RPullSyncW(s, fail).skipped
    <4>1. p \notin SyncOwed(s, fail) BY <3>2, <1>1, <1>3b DEF SyncBl
    <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF SyncOwed, SyncAll
    <4>3. RPullSyncW(s, fail).baseline[p] = w[s].baseline[p] BY <4>1, <1>3b DEF SyncBl
    <4>4. Held(s, p) BY <3>2, <4>3, <1>3 DEF HeldR, Held
    <4>5. w[s].local[p] # w[s].baseline[p] BY <4>2, <4>4, <4>3, <3>2, <1>1, <1>s DEF Owed
    <4>. QED BY <4>1, <4>3, <4>4, <4>5, <3>2, <1>1, <1>3 DEF SyncBl
  <3>. QED BY <3>1, <3>2 DEF RRec
<1>6. RMine(RPullSyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RMine, CasedMine
<1>7. RAdv(RPullSyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RAdv, CasedAdv, HeldR, Held
<1>8. RSame(RPullSyncW(s, fail)) BY <1>1, <1>g, <1>3, <1>3b, <1>c DEF RSame, CasedSame
<1>. QED BY <1>a, <1>c, <1>y, <1>t2, <1>2, <1>e, <1>4, <1>5, <1>6, <1>7, <1>8, M3Write DEF IndM3, M3

------------------------------------------------------------------------------
(* M3: the step, and the theorems.                                          *)

LEMMA Next_M3 == IndM3 /\ Next => IndM3'
<1>. SUFFICES ASSUME IndM3, Next PROVE IndM3' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM3' BY <3>1, GPut_M3
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM3' BY <3>2, GCas_M3
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM3' BY <3>3, GDelete_M3
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM3' BY <3>4, GRename_M3
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M3
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM3' BY <3>1, Checkout_M3
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM3' BY <3>2, Consume_M3
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM3' BY <3>3, Scan_M3
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM3' BY <3>4, Skip_M3
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM3' BY <3>5, PullOnly_M3
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM3' BY <3>6, Claim_M3
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM3' BY <3>7, Verify_M3
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM3' BY <3>8, Install_M3
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM3' BY <3>9, Collect_M3
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM3' BY <3>10, Finish_M3
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM3' BY <3>11, Restart_M3
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM3' BY <3>12, Sync_M3
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM3' BY <3>13, RescopeBegin_M3
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM3' BY <3>14, RescopeFirst_M3
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM3' BY <3>15, RescopeSecond_M3
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM3' BY <3>16, RPullRead_M3
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM3' BY <3>17, RPullSync_M3
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM3' BY <3>18, Edit_M3
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM3' BY <3>19, Delete_M3
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM3' BY <3>20, Upload_M3
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM3' BY <3>21, Sweep_M3
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M3
  <2>2. CASE RLoad BY <2>2, RLoad_M3
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM3' BY <2>3, Reap_M3
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M3Invariant == Spec => []IndM3
<1>1. Init => IndM3 BY Init_M3
<1>2. IndM3 /\ [Next]_vars => IndM3'
  <2>1. IndM3 /\ Next => IndM3' BY Next_M3
  <2>2. IndM3 /\ UNCHANGED vars => IndM3'
    <3>1. IndM3 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM3 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM3, IndM2, IndM1, vars
    <3>3. IndM3 /\ UNCHANGED vars => M2' BY M2Same DEF IndM3, IndM2, vars, aux, ret
    <3>4. IndM3 /\ UNCHANGED vars => M3' BY M3Same DEF IndM3, vars, bucket
    <3>. QED BY <3>1, <3>2, <3>3, <3>4 DEF IndM3, IndM2, IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

\* The cheap path skips only a writer owed nothing: an owed path is held and
\* differs from the baseline, so the record has it skipped, and the cheap
\* path re-checks every skipped path as dirty.
LEMMA ShortcutFromRecord == IndM3 => Inv_ShortcutSound
<1>. SUFFICES ASSUME IndM3, NEW s \in Writers, On(s), w[s].pc = "idle", w[s].sStage = "none", CheapPath(s),
                     NEW p \in Paths, Owed(s, p)
              PROVE  FALSE
  BY DEF Inv_ShortcutSound
<1>s. RecheckSkipped /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. w[s].derived = seq /\ \A q \in w[s].skipped : w[s].local[q] # w[s].baseline[q] BY <1>s DEF CheapPath
<1>2. Held(s, p) /\ doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p] BY <1>s DEF Owed
<1>3. p \in w[s].skipped BY <1>1, <1>2 DEF IndM3, M3, Record
<1>. QED BY <1>1, <1>2, <1>3

\* A reader that skips a tick has `memo = seq` and something derived, so by
\* the bounds its record is current; it re-checks every skipped path.
LEMMA ReaderFromRecord == IndM3 => Inv_ReaderSound
<1>. SUFFICES ASSUME IndM3, NEW s \in Readers, On(s), w[s].pc = "idle", w[s].sStage = "none", ReaderSkips(s),
                     NEW p \in Paths, Owed(s, p)
              PROVE  FALSE
  BY DEF Inv_ReaderSound
<1>0. s \in Writers BY ReadersWriters
<1>s. ReaderRechecksOwed /\ ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>y. w[s] \in Writer /\ seq \in Nat BY <1>0 DEF IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>n. w[s].derived \in Nat /\ w[s].memo \in Nat BY <1>y, WriterFields
<1>1. w[s].memo = seq /\ w[s].derived # 0 /\ \A q \in w[s].skipped : w[s].local[q] # w[s].baseline[q]
  BY <1>s DEF ReaderSkips, StillOwed
<1>2. w[s].derived = seq BY <1>0, <1>1, <1>y, <1>n DEF IndM3, M3, Bounds
<1>3. Held(s, p) /\ doc[p] # w[s].baseline[p] /\ w[s].local[p] = w[s].baseline[p] BY <1>s DEF Owed
<1>4. p \in w[s].skipped BY <1>0, <1>2, <1>3 DEF IndM3, M3, Record
<1>. QED BY <1>1, <1>3, <1>4

THEOREM ShortcutSound == Spec => []Inv_ShortcutSound
BY M3Invariant, ShortcutFromRecord, PTL

THEOREM ReaderSound == Spec => []Inv_ReaderSound
BY M3Invariant, ReaderFromRecord, PTL

------------------------------------------------------------------------------
(* M4: Inv_ReaderFetches.                                                   *)

\* The never-re-cited lemma: a step cites a fresh handle or moves a cited
\* one, and what stops being cited is logged retiring (`RetUpdate`).
NeverReCited == \A p \in Paths : doc[p] # Nil => doc[p] \notin retiring \cup aged
\* I11: a reader that loaded the document less than G ago holds handles each
\* live, not aged, and still cited or retiring.
ReaderLive == ~rlag => \A p \in Paths : rdoc[p] # Nil =>
                /\ rdoc[p] \in live /\ rdoc[p] \notin aged
                /\ (Cited(rdoc[p]) \/ rdoc[p] \in retiring)
M4 == NeverReCited /\ ReaderLive
IndM4 == IndM3 /\ M4

\* What a step may newly cite is cited already or neither retiring nor aged.
DocNew == \A k \in Paths : doc'[k] # Nil => doc'[k] \in CitedSet \/ doc'[k] \notin retiring \cup aged
\* What `live` may lose is uncited and not retiring.
LiveLoss == \A h \in live : h \notin live' => ~Cited(h) /\ h \notin retiring

------------------------------------------------------------------------------
(* What the events give M4.                                                 *)

\* Under the retire age every frame step logs exactly what stops being cited.
LEMMA RetExact ==
  ASSUME RetireAge, RetUpdate
  PROVE  retiring' = retiring \cup (CitedSet \ CitedSet')
BY DEF RetUpdate, CitedSet

LEMMA DocSameNew == doc' = doc => DocNew
BY DEF DocNew, CitedSet

LEMMA LiveGrows == live \subseteq live' => LiveLoss
BY DEF LiveLoss

LEMMA M4Write ==
  ASSUME M4, DocNew, LiveLoss,
         retiring' = retiring \cup (CitedSet \ CitedSet'),
         aged' = aged, rdoc' = rdoc, rlag' = rlag
  PROVE  M4'
<1>1. NeverReCited'
  <2>. SUFFICES ASSUME NEW p \in Paths, doc'[p] # Nil PROVE doc'[p] \notin retiring' \cup aged'
    BY DEF NeverReCited
  <2>1. doc'[p] \in CitedSet' BY DEF CitedSet
  <2>2. CASE doc'[p] \in CitedSet
    <3>1. doc'[p] \notin retiring \cup aged BY <2>2 DEF M4, NeverReCited, CitedSet
    <3>. QED BY <2>1, <3>1
  <2>3. CASE doc'[p] \notin retiring \cup aged BY <2>1, <2>3
  <2>. QED BY <2>2, <2>3 DEF DocNew
<1>2. ReaderLive'
  <2>. SUFFICES ASSUME ~rlag, NEW p \in Paths, rdoc[p] # Nil
                PROVE  /\ rdoc[p] \in live' /\ rdoc[p] \notin aged'
                       /\ ((\E k \in Paths : doc'[k] = rdoc[p]) \/ rdoc[p] \in retiring')
    BY DEF ReaderLive, Cited
  <2>0. /\ rdoc[p] \in live /\ rdoc[p] \notin aged
        /\ ((\E k \in Paths : doc[k] = rdoc[p]) \/ rdoc[p] \in retiring)
    BY DEF M4, ReaderLive, Cited
  <2>1. rdoc[p] \in live' BY <2>0 DEF LiveLoss, Cited
  <2>2. CASE \E k \in Paths : doc[k] = rdoc[p]
    <3>1. rdoc[p] \in CitedSet BY <2>2 DEF CitedSet
    <3>. QED BY <2>0, <2>1, <3>1 DEF CitedSet
  <2>. QED BY <2>0, <2>1, <2>2
<1>. QED BY <1>1, <1>2 DEF M4

\* M4 after a step that moves nothing M4 reads.
LEMMA M4Same ==
  ASSUME M4, UNCHANGED <<doc, live, retiring, aged, rdoc, rlag>>
  PROVE  M4'
BY DEF M4, NeverReCited, ReaderLive, Cited

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M4 == Init => IndM4
<1>. SUFFICES ASSUME Init PROVE IndM4 OBVIOUS
<1>1. IndM3 BY Init_M3
<1>2. NeverReCited /\ ReaderLive BY DEF Init, NeverReCited, ReaderLive
<1>. QED BY <1>1, <1>2 DEF IndM4, M4

------------------------------------------------------------------------------
(* M4: the gateway.                                                         *)

LEMMA GPut_M4 ==
  ASSUME IndM4, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, GPut_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF GPut, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF GPut, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA GCas_M4 ==
  ASSUME IndM4, NEW p \in Paths, GCas(p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, GCas_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d. DocNew 
  <2>1. gw[p] \notin retiring \cup aged /\ gw[p] # Nil BY DEF GCas, IndM4, IndM3, IndM2, M2, Flight
  <2>2. \A k \in Paths : doc'[k] = doc[k] \/ doc'[k] = gw[p]
    <3>1. doc \in [Paths -> Opt(Handles)] BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
    <3>. QED BY <3>1 DEF GCas
  <2>. QED BY <2>1, <2>2 DEF DocNew, CitedSet
<1>l0. live \subseteq live' BY DEF GCas, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA GRename_M4 ==
  ASSUME IndM4, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, GRename_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>sr. RenameAtomic BY ShippedShape DEF Shipped
<1>d. DocNew 
  <2>1. doc \in [Paths -> Opt(Handles)] BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>2. doc[p] # Nil /\ \A k \in Paths : doc'[k] = doc[k] \/ doc'[k] = Nil \/ doc'[k] = doc[p]
    BY <1>sr, <2>1 DEF GRename
  <2>. QED BY <2>2 DEF DocNew, CitedSet
<1>l0. live \subseteq live' BY DEF GRename, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA GRenameFinish_M4 ==
  ASSUME IndM4, GRenameFinish, Frame
  PROVE  IndM4'
<1>1. mv = Nil BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

LEMMA GDelete_M4 ==
  ASSUME IndM4, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, GDelete_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d. DocNew 
  <2>1. doc \in [Paths -> Opt(Handles)] BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>2. \A k \in Paths : doc'[k] = doc[k] \/ doc'[k] = Nil BY <2>1 DEF GDelete
  <2>. QED BY <2>2 DEF DocNew, CitedSet
<1>l0. live \subseteq live' BY DEF GDelete, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Sweep_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Sweep_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Sweep, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l. LiveLoss 
  <2>1. live' = live \ {h} /\ ~Cited(h) /\ h \notin retiring BY <1>s DEF Sweep
  <2>. QED BY <2>1 DEF LiveLoss
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

------------------------------------------------------------------------------
(* M4: the agent, the commit section, the sync, the rescope, the reader.    *)

LEMMA Edit_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Edit_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Edit, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Edit, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Delete_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Delete_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Delete, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Delete, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Checkout_M4 ==
  ASSUME IndM4, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Checkout_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Checkout, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Checkout, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Consume_M4 ==
  ASSUME IndM4, NEW s \in Writers, Consume(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Consume_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Consume, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Consume, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Scan_M4 ==
  ASSUME IndM4, NEW s \in Writers, Scan(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Scan_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Scan, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Scan, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Skip_M4 ==
  ASSUME IndM4, NEW s \in Writers, Skip(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Skip_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Skip, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Skip, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Upload_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Upload_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Upload, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Upload, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA PullOnly_M4 ==
  ASSUME IndM4, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, PullOnly_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF PullOnly, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF PullOnly, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Claim_M4 ==
  ASSUME IndM4, NEW s \in Writers, Claim(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Claim_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Claim, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Claim, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Verify_M4 ==
  ASSUME IndM4, NEW s \in Writers, Verify(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Verify_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Verify, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Verify, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Install_M4 ==
  ASSUME IndM4, NEW s \in Writers, Install(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Install_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d. DocNew 
  <2>1. \A k \in Paths : InstallInst(s)[k] = Nil \/ InstallInst(s)[k] = doc[k]
                         \/ (k \in InstallMine(s) \ w[s].gone /\ InstallInst(s)[k] = w[s].snap[k])
    BY DEF InstallInst
  <2>2. \A k \in InstallMine(s) \ w[s].gone : w[s].snap[k] \notin retiring \cup aged
    BY DEF IndM4, IndM3, IndM2, M2, Uploaded, Up, InstallMine, Install
  <2>3. doc' = InstallInst(s) BY DEF Install
  <2>. QED BY <2>1, <2>2, <2>3 DEF DocNew, CitedSet
<1>l0. live \subseteq live' BY DEF Install, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Collect_M4 ==
  ASSUME IndM4, NEW s \in Writers, Collect(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Collect_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Collect, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l. LiveLoss 
  <2>1. live' = live BY <1>s DEF Collect
  <2>. QED BY <2>1, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Finish_M4 ==
  ASSUME IndM4, NEW s \in Writers, Finish(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Finish_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Finish, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Finish, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Restart_M4 ==
  ASSUME IndM4, NEW s \in Writers, Restart(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Restart_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Restart, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Restart, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA Sync_M4 ==
  ASSUME IndM4, NEW s \in Writers, Sync(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Sync_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF Sync, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF Sync, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RescopeBegin_M4 ==
  ASSUME IndM4, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RescopeBegin_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RescopeBegin, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RescopeBegin, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RescopeFirst_M4 ==
  ASSUME IndM4, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RescopeFirst_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RescopeFirst, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RescopeFirst, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RescopeSecond_M4 ==
  ASSUME IndM4, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RescopeSecond_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RescopeSecond, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RescopeSecond, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RPullRead_M4 ==
  ASSUME IndM4, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RPullRead_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RPullRead, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RPullRead, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

LEMMA RPullSync_M4 ==
  ASSUME IndM4, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RPullSync_M3
<1>c. M4 BY DEF IndM4
<1>s. RetireAge BY ShippedShape DEF Shipped
<1>r. retiring' = retiring \cup (CitedSet \ CitedSet') /\ aged' = aged /\ rdoc' = rdoc /\ rlag' = rlag
  BY <1>s, RetExact DEF Frame
<1>d0. doc' = doc BY DEF RPullSync, bucket
<1>d. DocNew BY <1>d0, DocSameNew
<1>l0. live \subseteq live' BY DEF RPullSync, bucket
<1>l. LiveLoss BY <1>l0, LiveGrows
<1>. QED BY <1>a, <1>c, <1>r, <1>d, <1>l, M4Write DEF IndM4

------------------------------------------------------------------------------
(* M4: the retire age and the reader's load.                                *)

LEMMA Age_M4 ==
  ASSUME IndM4, Age, UNCHANGED anc
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Age_M3
<1>c. NeverReCited BY DEF IndM4, M4
<1>1. aged' = aged \cup retiring /\ retiring' = {} /\ rlag' = TRUE /\ doc' = doc BY DEF Age
<1>2. NeverReCited' BY <1>1, <1>c DEF NeverReCited
<1>3. ReaderLive' BY <1>1 DEF ReaderLive
<1>. QED BY <1>a, <1>2, <1>3 DEF IndM4, M4

LEMMA Reap_M4 ==
  ASSUME IndM4, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, Reap_M3
<1>1. /\ h \in aged /\ live' = live \ {h} /\ aged' = aged \ {h}
      /\ UNCHANGED <<doc, retiring, rdoc, rlag>>
  BY DEF Reap
<1>2. NeverReCited' BY <1>1 DEF IndM4, M4, NeverReCited
<1>3. ReaderLive'
  <2>. SUFFICES ASSUME ~rlag, NEW p \in Paths, rdoc[p] # Nil
                PROVE  /\ rdoc[p] \in live' /\ rdoc[p] \notin aged'
                       /\ ((\E k \in Paths : doc'[k] = rdoc[p]) \/ rdoc[p] \in retiring')
    BY <1>1 DEF ReaderLive, Cited
  <2>1. rdoc[p] \in live /\ rdoc[p] \notin aged /\ ((\E k \in Paths : doc[k] = rdoc[p]) \/ rdoc[p] \in retiring)
    BY DEF IndM4, M4, ReaderLive, Cited
  <2>. QED BY <1>1, <2>1
<1>. QED BY <1>a, <1>2, <1>3 DEF IndM4, M4

LEMMA RLoad_M4 ==
  ASSUME IndM4, RLoad, UNCHANGED anc
  PROVE  IndM4'
<1>a0. IndM3 BY DEF IndM4
<1>a. IndM3' BY <1>a0, RLoad_M3
<1>1. rdoc' = doc /\ rlag' = FALSE /\ UNCHANGED <<live, doc, retiring, aged>> BY DEF RLoad
<1>2. NeverReCited' BY <1>1 DEF IndM4, M4, NeverReCited
<1>3. ReaderLive'
  <2>. SUFFICES ASSUME NEW p \in Paths, doc[p] # Nil
                PROVE  /\ doc[p] \in live /\ doc[p] \notin aged
                       /\ ((\E k \in Paths : doc[k] = doc[p]) \/ doc[p] \in retiring)
    BY <1>1 DEF ReaderLive, Cited
  <2>1. doc[p] \in live BY DEF IndM4, IndM3, IndM2, M2, Inv_CitationsLive
  <2>2. doc[p] \notin aged BY DEF IndM4, M4, NeverReCited
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>a, <1>2, <1>3 DEF IndM4, M4

------------------------------------------------------------------------------
(* M4: the step, and the theorem.                                           *)

LEMMA Next_M4 == IndM4 /\ Next => IndM4'
<1>. SUFFICES ASSUME IndM4, Next PROVE IndM4' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM4' BY <3>1, GPut_M4
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM4' BY <3>2, GCas_M4
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM4' BY <3>3, GDelete_M4
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM4' BY <3>4, GRename_M4
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M4
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM4' BY <3>1, Checkout_M4
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM4' BY <3>2, Consume_M4
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM4' BY <3>3, Scan_M4
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM4' BY <3>4, Skip_M4
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM4' BY <3>5, PullOnly_M4
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM4' BY <3>6, Claim_M4
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM4' BY <3>7, Verify_M4
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM4' BY <3>8, Install_M4
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM4' BY <3>9, Collect_M4
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM4' BY <3>10, Finish_M4
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM4' BY <3>11, Restart_M4
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM4' BY <3>12, Sync_M4
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM4' BY <3>13, RescopeBegin_M4
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM4' BY <3>14, RescopeFirst_M4
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM4' BY <3>15, RescopeSecond_M4
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM4' BY <3>16, RPullRead_M4
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM4' BY <3>17, RPullSync_M4
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM4' BY <3>18, Edit_M4
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM4' BY <3>19, Delete_M4
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM4' BY <3>20, Upload_M4
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM4' BY <3>21, Sweep_M4
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M4
  <2>2. CASE RLoad BY <2>2, RLoad_M4
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM4' BY <2>3, Reap_M4
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M4Invariant == Spec => []IndM4
<1>1. Init => IndM4 BY Init_M4
<1>2. IndM4 /\ [Next]_vars => IndM4'
  <2>1. IndM4 /\ Next => IndM4' BY Next_M4
  <2>2. IndM4 /\ UNCHANGED vars => IndM4'
    <3>1. IndM4 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM4 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM4, IndM3, IndM2, IndM1, vars
    <3>3. IndM4 /\ UNCHANGED vars => M2' BY M2Same DEF IndM4, IndM3, IndM2, vars, aux, ret
    <3>4. IndM4 /\ UNCHANGED vars => M3' BY M3Same DEF IndM4, IndM3, vars, bucket
    <3>5. IndM4 /\ UNCHANGED vars => M4' BY M4Same DEF IndM4, vars, ret
    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5 DEF IndM4, IndM3, IndM2, IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

THEOREM ReaderFetches == Spec => []Inv_ReaderFetches
<1>1. IndM4 => Inv_ReaderFetches BY DEF IndM4, M4, ReaderLive, Inv_ReaderFetches
<1>. QED BY M4Invariant, <1>1, PTL

------------------------------------------------------------------------------
(* M5, part A: the history.                                                 *)

\* A handle not yet minted has no history.
HistNew == \A h \in Handles \ minted : base[h] = Nil /\ anc[h] = {} /\ orig[h] = Nil
\* `anc` is what `base` reaches, one level down.
HistAnc == \A h \in Handles : anc[h] = AncOf(base[h])
\* A minted handle's history is minted; an original is no copy.
HistMinted == \A h \in minted :
                /\ base[h] \in Opt(minted) /\ anc[h] \subseteq minted
                /\ orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil)
\* What a tree's baseline names is minted.
BaselineMinted == \A s \in Writers, p \in Paths : w[s].baseline[p] \in Opt(minted)
Hist == HistNew /\ HistAnc /\ HistMinted /\ BaselineMinted
M5 == Hist
IndM5 == IndM4 /\ M5

\* What a step does to the history: it keeps every minted handle's, and
\* writes a minted history only at what it mints.
HistEv ==
  /\ minted \subseteq minted'
  /\ \A m \in minted : base'[m] = base[m] /\ orig'[m] = orig[m]
  /\ \A h \in minted' \ minted : /\ base'[h] \in Opt(minted) /\ orig'[h] \in Opt(minted)
                                 /\ (orig'[h] # Nil => orig[orig'[h]] = Nil)
  /\ \A h \in Handles \ minted' : base'[h] = Nil /\ orig'[h] = Nil
\* Where a step takes a tree's new baseline from.
BlEv == \A u \in Writers, p \in Paths :
          w'[u].baseline[p] \in {w[u].baseline[p], Nil, doc[p], w[u].snap[p]}
\* The typing HistWrite reads.
HistTy ==
  /\ minted \subseteq Handles /\ minted' \subseteq Handles
  /\ base \in [Handles -> Opt(Handles)] /\ base' \in [Handles -> Opt(Handles)]
  /\ orig \in [Handles -> Opt(Handles)] /\ orig' \in [Handles -> Opt(Handles)]
  /\ anc \in [Handles -> SUBSET Handles]

------------------------------------------------------------------------------
(* What the events give the history.                                        *)

LEMMA HistSame ==
  ASSUME Hist, UNCHANGED <<minted, base, orig>>
  PROVE  HistEv
BY DEF Hist, HistNew, HistEv

LEMMA BlNone == w' = w => BlEv
BY DEF BlEv

LEMMA BlWrite ==
  ASSUME NEW t, NEW R, Wr(t, R),
         t \in Writers => \A p \in Paths : R.baseline[p] \in {w[t].baseline[p], Nil, doc[p], w[t].snap[p]}
  PROVE  BlEv
BY DEF Wr, BlEv

\* A minted handle keeps its `anc` across a step.
LEMMA AncKeep ==
  ASSUME HistTy, HistEv, AncUpdate, NEW m \in minted
  PROVE  anc'[m] = anc[m]
BY DEF HistEv, AncUpdate, HistTy

LEMMA HistWrite ==
  ASSUME Hist, HistEv, HistTy, AncUpdate, BlEv, Minted, SnapMinted
  PROVE  Hist'
<1>k. \A m \in minted : anc'[m] = anc[m] BY AncKeep
<1>1. HistNew'
  <2>. SUFFICES ASSUME NEW h \in Handles \ minted' PROVE base'[h] = Nil /\ anc'[h] = {} /\ orig'[h] = Nil
    BY DEF HistNew
  <2>1. h \notin minted BY DEF HistEv
  <2>2. base[h] = Nil /\ anc[h] = {} BY <2>1 DEF Hist, HistNew
  <2>3. base'[h] = Nil /\ orig'[h] = Nil BY DEF HistEv
  <2>4. anc'[h] = anc[h] BY <2>2, <2>3 DEF AncUpdate, HistTy
  <2>. QED BY <2>2, <2>3, <2>4
<1>2. HistAnc'
  <2>. SUFFICES ASSUME NEW h \in Handles
                PROVE  anc'[h] = IF base'[h] = Nil THEN {} ELSE {base'[h]} \cup anc'[base'[h]]
    BY DEF HistAnc, AncOf
  <2>1. CASE h \in minted
    <3>1. base'[h] = base[h] /\ anc'[h] = anc[h] BY <2>1, <1>k DEF HistEv
    <3>2. anc[h] = IF base[h] = Nil THEN {} ELSE {base[h]} \cup anc[base[h]] BY DEF Hist, HistAnc, AncOf
    <3>3. base[h] \in Opt(minted) BY <2>1 DEF Hist, HistMinted
    <3>4. base[h] # Nil => anc'[base[h]] = anc[base[h]] BY <3>3, <1>k DEF Opt
    <3>. QED BY <3>1, <3>2, <3>4
  <2>2. CASE h \notin minted
    <3>1. base[h] = Nil /\ anc[h] = {} BY <2>2 DEF Hist, HistNew
    <3>2. CASE base'[h] = Nil
      <4>1. anc'[h] = anc[h] BY <3>1, <3>2 DEF AncUpdate, HistTy
      <4>. QED BY <3>1, <3>2, <4>1
    <3>3. CASE base'[h] # Nil
      <4>1. h \in minted' BY <3>3 DEF HistEv
      <4>2. base'[h] \in minted BY <2>2, <3>3, <4>1 DEF HistEv, Opt
      <4>3. anc'[h] = {base'[h]} \cup anc[base'[h]] BY <3>1, <3>3 DEF AncUpdate, AncOf, HistTy
      <4>. QED BY <4>2, <4>3, <3>3, <1>k
    <3>. QED BY <3>2, <3>3
  <2>. QED BY <2>1, <2>2
<1>3. HistMinted'
  <2>. SUFFICES ASSUME NEW h \in minted'
                PROVE  /\ base'[h] \in Opt(minted') /\ anc'[h] \subseteq minted'
                       /\ orig'[h] \in Opt(minted') /\ (orig'[h] # Nil => orig'[orig'[h]] = Nil)
    BY DEF HistMinted
  <2>0. minted \subseteq minted' BY DEF HistEv
  <2>1. CASE h \in minted
    <3>1. base'[h] = base[h] /\ orig'[h] = orig[h] /\ anc'[h] = anc[h] BY <2>1, <1>k DEF HistEv
    <3>2. /\ base[h] \in Opt(minted) /\ anc[h] \subseteq minted
          /\ orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil)
      BY <2>1 DEF Hist, HistMinted
    <3>3. orig[h] # Nil => orig'[orig[h]] = orig[orig[h]] BY <3>2 DEF HistEv, Opt
    <3>. QED BY <2>0, <3>1, <3>2, <3>3 DEF Opt
  <2>2. CASE h \notin minted
    <3>1. /\ base'[h] \in Opt(minted) /\ orig'[h] \in Opt(minted)
          /\ (orig'[h] # Nil => orig[orig'[h]] = Nil)
      BY <2>2 DEF HistEv
    <3>2. base[h] = Nil /\ anc[h] = {} BY <2>2 DEF Hist, HistNew, HistTy
    <3>3. anc'[h] \subseteq minted
      <4>1. CASE base'[h] = Nil BY <3>2, <4>1 DEF AncUpdate, HistTy
      <4>2. CASE base'[h] # Nil
        <5>1. base'[h] \in minted BY <3>1, <4>2 DEF Opt
        <5>2. anc'[h] = {base'[h]} \cup anc[base'[h]] BY <3>2, <4>2 DEF AncUpdate, AncOf, HistTy
        <5>3. anc[base'[h]] \subseteq minted BY <5>1 DEF Hist, HistMinted
        <5>. QED BY <5>1, <5>2, <5>3
      <4>. QED BY <4>1, <4>2
    <3>4. orig'[h] # Nil => orig'[orig'[h]] = orig[orig'[h]] BY <3>1 DEF HistEv, Opt
    <3>. QED BY <2>0, <3>1, <3>3, <3>4 DEF Opt
  <2>. QED BY <2>1, <2>2
<1>4. BaselineMinted'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW p \in Paths PROVE w'[u].baseline[p] \in Opt(minted')
    BY DEF BaselineMinted
  <2>1. w'[u].baseline[p] \in {w[u].baseline[p], Nil, doc[p], w[u].snap[p]} BY DEF BlEv
  <2>2. w[u].baseline[p] \in Opt(minted) BY DEF Hist, BaselineMinted
  <2>3. doc[p] \in Opt(minted) /\ w[u].snap[p] \in Opt(minted) BY DEF Minted, SnapMinted
  <2>. QED BY <2>1, <2>2, <2>3 DEF HistEv, Opt
<1>. QED BY <1>1, <1>2, <1>3, <1>4 DEF Hist

\* THE POINT: Derives and Content between minted handles never move.
LEMMA DerivesKeep ==
  ASSUME Hist, HistEv, HistTy, AncUpdate, NEW k \in minted, NEW x \in minted
  PROVE  /\ Content(k)' = Content(k)
         /\ Derives(k, x)' = Derives(k, x)
<1>0. Nil \notin Handles BY NilHandle
<1>k. anc'[k] = anc[k] BY AncKeep
<1>a. anc[k] \subseteq minted BY DEF Hist, HistMinted
<1>c. \A m \in minted : Content(m)' = Content(m) BY DEF HistEv, Content
<1>1. Content(k)' = Content(k) BY <1>c
<1>2. Derives(k, x)' = Derives(k, x)
  <2>1. k # Nil /\ x # Nil BY <1>0 DEF HistTy
  <2>2. (\E a \in anc'[k] : a = x \/ (x # Nil /\ Content(a)' = Content(x)'))
        = (\E a \in anc[k] : a = x \/ (x # Nil /\ Content(a) = Content(x)))
    BY <1>k, <1>a, <1>c
  <2>. QED BY <2>1, <2>2, <1>c DEF Derives
<1>. QED BY <1>1, <1>2

\* The history after a step that writes none of it and no tree.
LEMMA HistAll ==
  ASSUME Hist, UNCHANGED <<minted, base, orig, anc, w>>
  PROVE  Hist'
BY DEF Hist, HistNew, HistAnc, AncOf, HistMinted, BaselineMinted

------------------------------------------------------------------------------
(* Init.                                                                    *)

LEMMA Init_M5 == Init => IndM5
<1>. SUFFICES ASSUME Init PROVE IndM5 OBVIOUS
<1>1. IndM4 BY Init_M4
<1>2. \A s \in Writers : w[s] = WriterInit BY DEF Init
<1>t. minted \subseteq Handles BY <1>1 DEF IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>b. base = [h \in Handles |-> Nil] /\ anc = [h \in Handles |-> {}] /\ orig = [h \in Handles |-> Nil] BY DEF Init
<1>3a. HistNew BY <1>b DEF HistNew
<1>3b. HistAnc BY <1>b DEF HistAnc, AncOf
<1>3c. HistMinted BY <1>b, <1>t DEF HistMinted, Opt
<1>3. HistNew /\ HistAnc /\ HistMinted BY <1>3a, <1>3b, <1>3c
<1>4. BaselineMinted BY <1>2 DEF BaselineMinted, WriterInit, Opt
<1>. QED BY <1>1, <1>3, <1>4 DEF IndM5, M5, Hist

------------------------------------------------------------------------------
(* M5: the gateway.                                                         *)

LEMMA GPut_M5 ==
  ASSUME IndM5, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY GPut_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv
  <2>. DEFINE h == <<p, nextGen>>
  <2>1. /\ minted' = minted \cup {h} /\ base' = [base EXCEPT ![h] = doc[p]] /\ orig' = orig
        /\ nextGen <= MaxMint
    BY DEF GPut, aux
  <2>2. nextGen \in Nat /\ nextGen > Seed BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>3. h \in Handles BY <2>1, <2>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed
  <2>4. h \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>. QED BY <3>1, <2>1, <2>2, MaxMintNat DEF Gen
  <2>5. doc[p] \in Opt(minted) BY <1>y DEF Minted
  <2>6. orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil) BY <2>3, <2>4, <1>c DEF Hist, HistNew, Opt
<2>q1. minted \subseteq minted' BY <2>1
<2>q2. \A x \in minted : base'[x] = base[x] /\ orig'[x] = orig[x]
  BY <2>1, <2>4, <1>ty DEF HistTy
<2>q3. \A x \in minted' \ minted : /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                                     /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
  <3>1. SUFFICES ASSUME NEW x \in minted' \ minted
                PROVE  /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                       /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
    OBVIOUS
  <3>2. x = h BY <2>1
  <3>3. base'[h] = doc[p] /\ orig'[h] = orig[h] BY <2>1, <2>3, <1>ty DEF HistTy
  <3>. QED BY <3>2, <3>3, <2>5, <2>6
<2>q4. \A x \in Handles \ minted' : base'[x] = Nil /\ orig'[x] = Nil
  <3>1. SUFFICES ASSUME NEW x \in Handles \ minted' PROVE base'[x] = Nil /\ orig'[x] = Nil
    OBVIOUS
  <3>2. x # h /\ x \notin minted BY <2>1
  <3>3. base'[x] = base[x] /\ orig'[x] = orig[x] BY <3>2, <2>1, <1>ty DEF HistTy
  <3>. QED BY <3>2, <3>3, <1>c DEF Hist, HistNew
<2>. QED BY <2>q1, <2>q2, <2>q3, <2>q4 DEF HistEv
<1>b. BlEv BY BlNone DEF GPut
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA GCas_M5 ==
  ASSUME IndM5, NEW p \in Paths, GCas(p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY GCas_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv BY <1>c, HistSame DEF GCas, bucket, aux
<1>b. BlEv BY BlNone DEF GCas
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA GRename_M5 ==
  ASSUME IndM5, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  IndM5'
<1>a. IndM4' BY GRename_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv BY <1>c, HistSame DEF GRename, bucket, aux
<1>b. BlEv BY BlNone DEF GRename
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA GRenameFinish_M5 ==
  ASSUME IndM5, GRenameFinish, Frame
  PROVE  IndM5'
<1>1. mv = Nil BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

LEMMA GDelete_M5 ==
  ASSUME IndM5, NEW p \in Paths, GDelete(p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY GDelete_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv BY <1>c, HistSame DEF GDelete, bucket, aux
<1>b. BlEv BY BlNone DEF GDelete
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Sweep_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Sweep_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>h. HistEv BY <1>c, HistSame DEF Sweep, bucket, aux
<1>b. BlEv BY BlNone DEF Sweep
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

------------------------------------------------------------------------------
(* M5: the agent, the commit section, the sync, the rescope, the reader.    *)

LEMMA Edit_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Edit_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] BY DEF Edit
<1>2. Wr(s, EditW(s, p)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. EditW(s, p).baseline = w[s].baseline BY DEF EditW
<1>4. \A p0 \in Paths : EditW(s, p).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv
  <2>. DEFINE h == <<p, nextGen>>
  <2>1. /\ minted' = minted \cup {h} /\ base' = [base EXCEPT ![h] = w[s].baseline[p]] /\ orig' = orig
        /\ nextGen <= MaxMint
    BY DEF Edit, aux
  <2>2. nextGen \in Nat /\ nextGen > Seed BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>3. h \in Handles BY <2>1, <2>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed
  <2>4. h \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>. QED BY <3>1, <2>1, <2>2, MaxMintNat DEF Gen
  <2>5. w[s].baseline[p] \in Opt(minted) BY <1>c DEF Hist, BaselineMinted
  <2>6. orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil) BY <2>3, <2>4, <1>c DEF Hist, HistNew, Opt
<2>q1. minted \subseteq minted' BY <2>1
<2>q2. \A x \in minted : base'[x] = base[x] /\ orig'[x] = orig[x]
  BY <2>1, <2>4, <1>ty DEF HistTy
<2>q3. \A x \in minted' \ minted : /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                                     /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
  <3>1. SUFFICES ASSUME NEW x \in minted' \ minted
                PROVE  /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                       /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
    OBVIOUS
  <3>2. x = h BY <2>1
  <3>3. base'[h] = w[s].baseline[p] /\ orig'[h] = orig[h] BY <2>1, <2>3, <1>ty DEF HistTy
  <3>. QED BY <3>2, <3>3, <2>5, <2>6
<2>q4. \A x \in Handles \ minted' : base'[x] = Nil /\ orig'[x] = Nil
  <3>1. SUFFICES ASSUME NEW x \in Handles \ minted' PROVE base'[x] = Nil /\ orig'[x] = Nil
    OBVIOUS
  <3>2. x # h /\ x \notin minted BY <2>1
  <3>3. base'[x] = base[x] /\ orig'[x] = orig[x] BY <3>2, <2>1, <1>ty DEF HistTy
  <3>. QED BY <3>2, <3>3, <1>c DEF Hist, HistNew
<2>. QED BY <2>q1, <2>q2, <2>q3, <2>q4 DEF HistEv
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Delete_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Delete_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] /\ UNCHANGED <<minted, base, orig, doc>> BY DEF Delete, bucket, aux
<1>2. Wr(s, DeleteW(s, p)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. DeleteW(s, p).baseline = w[s].baseline BY DEF DeleteW
<1>4. \A p0 \in Paths : DeleteW(s, p).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Checkout_M5 ==
  ASSUME IndM5, NEW s \in Writers, Checkout(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Checkout_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] /\ UNCHANGED <<minted, base, orig, doc>> BY DEF Checkout, bucket, aux
<1>2. Wr(s, CheckoutW(s, T)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. CheckoutW(s, T).baseline = CheckoutHeld(s, T) BY DEF CheckoutW
<1>4. \A p0 \in Paths : CheckoutW(s, T).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3 DEF CheckoutHeld
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Consume_M5 ==
  ASSUME IndM5, NEW s \in Writers, Consume(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Consume_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s)] /\ UNCHANGED <<minted, base, orig, doc>> BY <1>1 DEF Consume, bucket, aux
  <2>2. Wr(s, ConsumeCheapW(s)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. ConsumeCheapW(s).baseline = w[s].baseline BY DEF ConsumeCheapW
  <2>4. \A p0 \in Paths : ConsumeCheapW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
    BY <2>3
  <2>b. BlEv BY <2>2, <2>4, BlWrite
  <2>h. HistEv BY <2>1, <1>c, HistSame DEF bucket, aux
  <2>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <2>h, <2>b, HistWrite DEF IndM5, M5
<1>2. CASE ~CheapPath(s)
  <2>1. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail)] /\ UNCHANGED <<minted, base, orig, doc>> BY <1>2 DEF Consume, bucket, aux
  <2>2. Wr(s, ConsumeW(s, fail)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. ConsumeW(s, fail).baseline = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]] BY DEF ConsumeW
  <2>4. \A p0 \in Paths : ConsumeW(s, fail).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
    BY <2>3
  <2>b. BlEv BY <2>2, <2>4, BlWrite
  <2>h. HistEv BY <2>1, <1>c, HistSame DEF bucket, aux
  <2>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <2>h, <2>b, HistWrite DEF IndM5, M5
<1>. QED BY <1>1, <1>2

LEMMA Scan_M5 ==
  ASSUME IndM5, NEW s \in Writers, Scan(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Scan_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK dels \in SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] /\ UNCHANGED <<minted, base, orig, doc>> BY DEF Scan, bucket, aux
<1>2. Wr(s, ScanW(s, dels)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. ScanW(s, dels).baseline = w[s].baseline BY DEF ScanW
<1>4. \A p0 \in Paths : ScanW(s, dels).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Skip_M5 ==
  ASSUME IndM5, NEW s \in Writers, Skip(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Skip_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] /\ UNCHANGED <<minted, base, orig, doc>> BY DEF Skip, bucket, aux
<1>2. Wr(s, SkipW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. SkipW(s).baseline = w[s].baseline BY DEF SkipW
<1>4. \A p0 \in Paths : SkipW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Upload_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Upload_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. CASE w[s].snap[p] \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] /\ UNCHANGED <<minted, base, orig, doc>> BY <1>1 DEF Upload
  <2>2. Wr(s, UploadW(s, p)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. UploadW(s, p).baseline = w[s].baseline BY DEF UploadW
  <2>4. \A p0 \in Paths : UploadW(s, p).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
    BY <2>3
  <2>b. BlEv BY <2>2, <2>4, BlWrite
  <2>h. HistEv BY <2>1, <1>c, HistSame DEF bucket, aux
  <2>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <2>h, <2>b, HistWrite DEF IndM5, M5
<1>2. CASE w[s].snap[p] \in upped
  <2>. DEFINE c == <<p, MaxMint + copies + 1>>
  <2>. DEFINE h == w[s].snap[p]
  <2>1. /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] /\ doc' = doc
      /\ minted' = minted \cup {c} /\ base' = [base EXCEPT ![c] = base[h]]
      /\ orig' = [orig EXCEPT ![c] = Content(h)] /\ copies < MaxCopies
    BY <1>2 DEF Upload
  <2>2. Wr(s, UploadCopyW(s, p, c)) BY <1>y, <2>1, WriteAny DEF Wr
  <2>3. UploadCopyW(s, p, c).baseline = w[s].baseline BY DEF UploadCopyW
  <2>4. \A p0 \in Paths : UploadCopyW(s, p, c).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
    BY <2>3
  <2>b. BlEv BY <2>2, <2>4, BlWrite
<2>h. HistEv
    <3>1. /\ minted' = minted \cup {c} /\ base' = [base EXCEPT ![c] = base[h]]
          /\ orig' = [orig EXCEPT ![c] = Content(h)] /\ copies < MaxCopies
      BY <2>1
    <3>2. copies \in Nat /\ nextGen \in Nat /\ nextGen <= MaxMint + 1
      BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh, Ghosts, IndM1, IndTypeOK, TypeOK
    <3>3. c \in Handles BY <3>1, <3>2, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed
    <3>4. c \notin minted
      <4>1. \A m \in minted : Gen(m) < nextGen \/ (Gen(m) > MaxMint /\ Gen(m) <= MaxMint + copies)
        BY DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
      <4>. QED BY <4>1, <3>2, MaxMintNat DEF Gen
    <3>h0. h \in minted BY <1>2 DEF IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>h1. base[h] \in Opt(minted) /\ orig[h] \in Opt(minted) /\ (orig[h] # Nil => orig[orig[h]] = Nil)
      BY <3>h0, <1>c DEF Hist, HistMinted
    <3>5. base[h] \in Opt(minted) BY <3>h1
    <3>6. Content(h) \in Opt(minted) /\ (Content(h) # Nil => orig[Content(h)] = Nil)
      <4>0. h # Nil BY <3>h0, NilHandle, <1>ty DEF HistTy
      <4>1. CASE orig[h] # Nil BY <4>0, <4>1, <3>h1 DEF Content, Opt
      <4>2. CASE orig[h] = Nil BY <4>0, <4>2, <3>h0 DEF Content, Opt
      <4>. QED BY <4>1, <4>2
  <3>q1. minted \subseteq minted' BY <3>1
  <3>q2. \A x \in minted : base'[x] = base[x] /\ orig'[x] = orig[x]
    BY <3>1, <3>4, <1>ty DEF HistTy
  <3>q3. \A x \in minted' \ minted : /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                                       /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
    <4>1. SUFFICES ASSUME NEW x \in minted' \ minted
                  PROVE  /\ base'[x] \in Opt(minted) /\ orig'[x] \in Opt(minted)
                         /\ (orig'[x] # Nil => orig[orig'[x]] = Nil)
      OBVIOUS
    <4>2. x = c BY <3>1
    <4>3. base'[c] = base[h] /\ orig'[c] = Content(h) BY <3>1, <3>3, <1>ty DEF HistTy
    <4>. QED BY <4>2, <4>3, <3>5, <3>6
  <3>q4. \A x \in Handles \ minted' : base'[x] = Nil /\ orig'[x] = Nil
    <4>1. SUFFICES ASSUME NEW x \in Handles \ minted' PROVE base'[x] = Nil /\ orig'[x] = Nil
      OBVIOUS
    <4>2. x # c /\ x \notin minted BY <3>1
    <4>3. base'[x] = base[x] /\ orig'[x] = orig[x] BY <4>2, <3>1, <1>ty DEF HistTy
    <4>. QED BY <4>2, <4>3, <1>c DEF Hist, HistNew
  <3>. QED BY <3>q1, <3>q2, <3>q3, <3>q4 DEF HistEv
  <2>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <2>h, <2>b, HistWrite DEF IndM5, M5
<1>. QED BY <1>1, <1>2

LEMMA PullOnly_M5 ==
  ASSUME IndM5, NEW s \in Writers, PullOnly(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY PullOnly_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF PullOnly, bucket, aux
<1>2. Wr(s, PullOnlyW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. PullOnlyW(s).baseline = w[s].baseline BY DEF PullOnlyW
<1>4. \A p0 \in Paths : PullOnlyW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Claim_M5 ==
  ASSUME IndM5, NEW s \in Writers, Claim(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Claim_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Claim, bucket, aux
<1>2. Wr(s, ClaimW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. ClaimW(s).baseline = w[s].baseline BY DEF ClaimW
<1>4. \A p0 \in Paths : ClaimW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Verify_M5 ==
  ASSUME IndM5, NEW s \in Writers, Verify(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Verify_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Verify, bucket, aux
<1>2. Wr(s, VerifyW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. VerifyW(s).baseline = w[s].baseline BY DEF VerifyW
<1>4. \A p0 \in Paths : VerifyW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Install_M5 ==
  ASSUME IndM5, NEW s \in Writers, Install(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Install_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = InstallW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Install, bucket, aux
<1>2. Wr(s, InstallW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. InstallW(s).baseline = w[s].baseline BY DEF InstallW
<1>4. \A p0 \in Paths : InstallW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Collect_M5 ==
  ASSUME IndM5, NEW s \in Writers, Collect(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Collect_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Collect, bucket, aux
<1>2. Wr(s, CollectW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. CollectW(s).baseline = w[s].baseline BY DEF CollectW
<1>4. \A p0 \in Paths : CollectW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Finish_M5 ==
  ASSUME IndM5, NEW s \in Writers, Finish(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Finish_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Finish, bucket, aux
<1>2. Wr(s, FinishW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. FinishW(s).baseline = [p0 \in Paths |-> IF p0 \in w[s].uploads \cap w[s].upDone THEN w[s].snap[p0]
                         ELSE IF p0 \in w[s].deletes /\ w[s].inst[p0] = Nil THEN Nil
                         ELSE w[s].baseline[p0]] BY DEF FinishW
<1>4. \A p0 \in Paths : FinishW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Restart_M5 ==
  ASSUME IndM5, NEW s \in Writers, Restart(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Restart_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF Restart, bucket, aux
<1>2. Wr(s, RestartW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RestartW(s).baseline = w[s].baseline BY DEF RestartW
<1>4. \A p0 \in Paths : RestartW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA Sync_M5 ==
  ASSUME IndM5, NEW s \in Writers, Sync(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY Sync_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] /\ UNCHANGED <<minted, base, orig>> BY DEF Sync, bucket, aux
<1>2. Wr(s, SyncW(s, fail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. SyncW(s, fail).baseline = SyncBl(s, fail) BY DEF SyncW
<1>4. \A p0 \in Paths : SyncW(s, fail).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3 DEF SyncBl
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RescopeBegin_M5 ==
  ASSUME IndM5, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RescopeBegin_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] /\ UNCHANGED <<minted, base, orig>> BY DEF RescopeBegin, bucket, aux
<1>2. Wr(s, RescopeBeginW(s, T)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RescopeBeginW(s, T).baseline = w[s].baseline BY DEF RescopeBeginW
<1>4. \A p0 \in Paths : RescopeBeginW(s, T).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RescopeFirst_M5 ==
  ASSUME IndM5, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RescopeFirst_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF RescopeFirst, bucket, aux
<1>2. Wr(s, RescopeFirstW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RescopeFirstW(s).baseline = IF RescopeUnciteFirst
                THEN [p0 \in Paths |-> IF p0 \in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[p0]]
                ELSE w[s].baseline BY DEF RescopeFirstW
<1>4. \A p0 \in Paths : RescopeFirstW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RescopeSecond_M5 ==
  ASSUME IndM5, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RescopeSecond_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK wfail \in SUBSET {q \in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \/ ~WidenKeepsLocal} :
      w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] /\ UNCHANGED <<minted, base, orig>>
    BY DEF RescopeSecond, bucket, aux
<1>2. Wr(s, RescopeSecondW(s, wfail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RescopeSecondW(s, wfail).baseline = [p0 \in Paths |-> IF p0 \in RescopeFetch(s, wfail) THEN doc[p0] ELSE RescopeBase1(s)[p0]] BY DEF RescopeSecondW
<1>4. \A p0 \in Paths : RescopeSecondW(s, wfail).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3 DEF RescopeBase1
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RPullRead_M5 ==
  ASSUME IndM5, NEW s \in Writers, RPullRead(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RPullRead_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] /\ UNCHANGED <<minted, base, orig>> BY DEF RPullRead, bucket, aux
<1>2. Wr(s, RPullReadW(s)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RPullReadW(s).baseline = w[s].baseline BY DEF RPullReadW
<1>4. \A p0 \in Paths : RPullReadW(s).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

LEMMA RPullSync_M5 ==
  ASSUME IndM5, NEW s \in Writers, RPullSync(s), Frame
  PROVE  IndM5'
<1>a. IndM4' BY RPullSync_M4 DEF IndM5
<1>c. Hist BY DEF IndM5, M5
<1>y. /\ TypeOK /\ Ghosts /\ Minted /\ SnapMinted /\ Fresh
      /\ w \in [Writers -> Writer] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, M2
<1>ty. HistTy BY <1>a, <1>y DEF HistTy, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
<1>u. AncUpdate BY DEF Frame
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] /\ UNCHANGED <<minted, base, orig>> BY DEF RPullSync, bucket, aux
<1>2. Wr(s, RPullSyncW(s, fail)) BY <1>y, <1>1, WriteAny DEF Wr
<1>3. RPullSyncW(s, fail).baseline = SyncBl(s, fail) BY DEF RPullSyncW
<1>4. \A p0 \in Paths : RPullSyncW(s, fail).baseline[p0] \in {w[s].baseline[p0], Nil, doc[p0], w[s].snap[p0]}
  BY <1>3 DEF SyncBl
<1>b. BlEv BY <1>2, <1>4, BlWrite
<1>h. HistEv BY <1>1, <1>c, HistSame DEF bucket, aux
<1>. QED BY <1>a, <1>c, <1>y, <1>ty, <1>u, <1>h, <1>b, HistWrite DEF IndM5, M5

------------------------------------------------------------------------------
(* M5: the retire age and the reader's load.                                *)

LEMMA Age_M5 ==
  ASSUME IndM5, Age, UNCHANGED anc
  PROVE  IndM5'
<1>a. IndM4' BY Age_M4 DEF IndM5
<1>1. Hist' BY HistAll DEF IndM5, M5, Age, aux
<1>. QED BY <1>a, <1>1 DEF IndM5, M5

LEMMA Reap_M5 ==
  ASSUME IndM5, NEW s \in Writers, NEW h \in Handles, Reap(s, h), UNCHANGED anc
  PROVE  IndM5'
<1>a. IndM4' BY Reap_M4 DEF IndM5
<1>1. Hist' BY HistAll DEF IndM5, M5, Reap, aux
<1>. QED BY <1>a, <1>1 DEF IndM5, M5

LEMMA RLoad_M5 ==
  ASSUME IndM5, RLoad, UNCHANGED anc
  PROVE  IndM5'
<1>a. IndM4' BY RLoad_M4 DEF IndM5
<1>1. Hist' BY HistAll DEF IndM5, M5, RLoad, aux
<1>. QED BY <1>a, <1>1 DEF IndM5, M5

------------------------------------------------------------------------------
(* M5: the step, and the invariant.                                         *)

LEMMA Next_M5 == IndM5 /\ Next => IndM5'
<1>. SUFFICES ASSUME IndM5, Next PROVE IndM5' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE IndM5' BY <3>1, GPut_M5
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE IndM5' BY <3>2, GCas_M5
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE IndM5' BY <3>3, GDelete_M5
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE IndM5' BY <3>4, GRename_M5
    <3>5. CASE GRenameFinish BY <3>5, GRenameFinish_M5
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE IndM5' BY <3>1, Checkout_M5
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE IndM5' BY <3>2, Consume_M5
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE IndM5' BY <3>3, Scan_M5
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE IndM5' BY <3>4, Skip_M5
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE IndM5' BY <3>5, PullOnly_M5
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE IndM5' BY <3>6, Claim_M5
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE IndM5' BY <3>7, Verify_M5
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE IndM5' BY <3>8, Install_M5
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE IndM5' BY <3>9, Collect_M5
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE IndM5' BY <3>10, Finish_M5
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE IndM5' BY <3>11, Restart_M5
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE IndM5' BY <3>12, Sync_M5
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE IndM5' BY <3>13, RescopeBegin_M5
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE IndM5' BY <3>14, RescopeFirst_M5
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE IndM5' BY <3>15, RescopeSecond_M5
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE IndM5' BY <3>16, RPullRead_M5
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE IndM5' BY <3>17, RPullSync_M5
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE IndM5' BY <3>18, Edit_M5
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE IndM5' BY <3>19, Delete_M5
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE IndM5' BY <3>20, Upload_M5
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE IndM5' BY <3>21, Sweep_M5
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>. UNCHANGED anc BY <1>2
  <2>1. CASE Age BY <2>1, Age_M5
  <2>2. CASE RLoad BY <2>2, RLoad_M5
  <2>3. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE IndM5' BY <2>3, Reap_M5
  <2>. QED BY <1>2, <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA M5Invariant == Spec => []IndM5
<1>1. Init => IndM5 BY Init_M5
<1>2. IndM5 /\ [Next]_vars => IndM5'
  <2>1. IndM5 /\ Next => IndM5' BY Next_M5
  <2>2. IndM5 /\ UNCHANGED vars => IndM5'
    <3>1. IndM5 /\ UNCHANGED vars => IndTypeOK'
      BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts, Minted, vars, aux, ret
    <3>2. IndM5 /\ UNCHANGED vars => M1' BY M1Keep DEF IndM5, IndM4, IndM3, IndM2, IndM1, vars
    <3>3. IndM5 /\ UNCHANGED vars => M2' BY M2Same DEF IndM5, IndM4, IndM3, IndM2, vars, aux, ret
    <3>4. IndM5 /\ UNCHANGED vars => M3' BY M3Same DEF IndM5, IndM4, IndM3, vars, bucket
    <3>5. IndM5 /\ UNCHANGED vars => M4' BY M4Same DEF IndM5, IndM4, vars, ret
    <3>6. IndM5 /\ UNCHANGED vars => M5' BY HistAll DEF IndM5, M5, vars, bucket, aux
    <3>. QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <3>6 DEF IndM5, IndM4, IndM3, IndM2, IndM1
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>1, <1>2, PTL DEF Spec

==============================================================================
