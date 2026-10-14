---------------------------- MODULE LeanP1ProofM6 -----------------------------
EXTENDS LeanP1ProofM5D

------------------------------------------------------------------------------
(* M6 (the plan's item 7): Prop_AgentWorkKept and Prop_ScopeRespected, the  *)
(* L-130/L-131 action properties, as LeanP1.tla states them.                *)
(*                                                                          *)
(* ScopeRespected needs nothing new: under the shipped shape a consume or a *)
(* sync takes only a held path, a widen fetches only inside its new scope,  *)
(* an unlink is of a dropped path, and a copy at upload keeps the bytes.    *)
(* AgentWorkKept needs that what nobody PUT is in no baseline, no recorded  *)
(* uncite and no copy's bytes (UI, TLC-checked first: E42): then a take is  *)
(* of a clean baseline (PUT), or adopts the same bytes; an unlink is of the *)
(* recorded uncite's bytes (PUT); a copy is made of a PUT snapshot.         *)

\* The claims' step formulas, as LeanP1.tla writes them.
AWA == \A s \in Writers, p \in Paths :
         LET h == w[s].local[p] IN
         (/\ h # Nil /\ h \notin upped
          /\ w'[s].local[p] # h
          /\ ~(w'[s].local[p] # Nil /\ ContentNext(w'[s].local[p]) = h)) => AgentStep(s)
SRA == \A s \in Writers, p \in Paths :
         (/\ w[s].st = "on"
          /\ p \notin w[s].scope \cup w'[s].scope /\ w[s].baseline[p] = Nil
          /\ w'[s].local[p] # w[s].local[p]
          /\ ~(w'[s].local[p] = Nil /\ p \in w[s].sDrop)
          /\ ~(w'[s].local[p] # Nil /\ w[s].local[p] # Nil
               /\ ContentNext(w'[s].local[p]) = Content(w[s].local[p]))) => AgentStep(s)
\* ...per tree.
AWt(t) == \A p \in Paths :
            LET h == w[t].local[p] IN
            (/\ h # Nil /\ h \notin upped
             /\ w'[t].local[p] # h
             /\ ~(w'[t].local[p] # Nil /\ ContentNext(w'[t].local[p]) = h)) => AgentStep(t)
SRt(t) == \A p \in Paths :
            (/\ w[t].st = "on"
             /\ p \notin w[t].scope \cup w'[t].scope /\ w[t].baseline[p] = Nil
             /\ w'[t].local[p] # w[t].local[p]
             /\ ~(w'[t].local[p] = Nil /\ p \in w[t].sDrop)
             /\ ~(w'[t].local[p] # Nil /\ w[t].local[p] # Nil
                  /\ ContentNext(w'[t].local[p]) = Content(w[t].local[p]))) => AgentStep(t)

\* The strengthening.  A tree's baseline, its recorded uncite and (in a
\* barrier) an upload's snapshot are PUT; a copy and its bytes are PUT.
UIR(R) == /\ \A p \in Paths : (R.baseline[p] = Nil \/ R.baseline[p] \in upped)
                               /\ (R.sHeld[p] = Nil \/ R.sHeld[p] \in upped)
          /\ R.pc \in Bar => \A p \in R.upDone : R.snap[p] \in upped
UIo == \A m \in minted : orig[m] # Nil => orig[m] \in upped /\ m \in upped
UI == (\A s \in Writers : UIR(w[s])) /\ UIo

M6Goal == UI' /\ AWA /\ SRA

------------------------------------------------------------------------------
(* Generic steps.                                                           *)

LEMMA M6Per ==
  ASSUME \A t \in Writers : AWt(t) /\ SRt(t) PROVE AWA /\ SRA
BY DEF AWA, SRA, AWt, SRt

LEMMA M6Same ==
  ASSUME NEW t, w'[t].local = w[t].local PROVE AWt(t) /\ SRt(t)
BY DEF AWt, SRt

LEMMA M6Agent ==
  ASSUME NEW t, AgentStep(t) PROVE AWt(t) /\ SRt(t)
BY DEF AWt, SRt

\* What the document cites is PUT.
LEMMA DocUpped ==
  ASSUME IndC, NEW q \in Paths, doc[q] # Nil PROVE doc[q] \in upped
<1>1. doc[q] \in live BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Inv_CitationsLive
<1>. QED BY <1>1 DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh

\* A handle nobody PUT is no copy: its bytes are its own.
LEMMA OwnBytes ==
  ASSUME IndC, UI, NEW h \in minted, h \notin upped PROVE Content(h) = h
<1>1. orig[h] = Nil BY DEF UI, UIo
<1>. QED BY <1>1 DEF Content

\* UIo across a step that writes no copy.
LEMMA UIoKeep ==
  ASSUME IndC, UI, upped \subseteq upped', orig' = orig, minted \subseteq minted',
         \A m \in minted' \ minted : orig[m] = Nil
  PROVE  UIo'
BY DEF UI, UIo

LEMMA HistNewOrig ==
  ASSUME IndC, NEW m \in Handles \ minted PROVE orig[m] = Nil
BY DEF IndC, IndM5, M5, Hist, HistNew

\* A step on tree s (as record R), with no copy and no mint.
LEMMA M6Tree ==
  ASSUME IndC, UI, NEW s \in Writers, NEW R, \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t],
         UNCHANGED <<upped, orig, minted>>,
         \A q \in Paths : \/ R.baseline[q] = Nil \/ R.baseline[q] = w[s].baseline[q] \/ R.baseline[q] = doc[q]
                          \/ q \in w[s].upDone /\ w[s].pc \in Bar /\ R.baseline[q] = w[s].snap[q],
         \A q \in Paths : R.sHeld[q] = Nil \/ R.sHeld[q] = w[s].sHeld[q] \/ R.sHeld[q] = w[s].baseline[q],
         R.pc \in Bar => (w[s].pc \in Bar /\ R.upDone \subseteq w[s].upDone /\ R.snap = w[s].snap),
         AWt(s) /\ SRt(s)
  PROVE  M6Goal
<1>a. \A t \in Writers \ {s} : AWt(t) /\ SRt(t) BY M6Same
<1>b. AWA /\ SRA BY <1>a, M6Per
<1>c. UIo' BY UIoKeep
<1>d. \A t \in Writers : UIR(w[t])'
  <2>. SUFFICES ASSUME NEW t \in Writers PROVE UIR(w[t])' OBVIOUS
  <2>1. CASE t # s BY <2>1 DEF UI, UIR
  <2>2. CASE t = s
    <3>1. \A q \in Paths : R.baseline[q] = Nil \/ R.baseline[q] \in upped
      <4>. SUFFICES ASSUME NEW q \in Paths, R.baseline[q] # Nil PROVE R.baseline[q] \in upped OBVIOUS
      <4>1. CASE R.baseline[q] = w[s].baseline[q] BY <4>1 DEF UI, UIR
      <4>2. CASE R.baseline[q] = doc[q] BY <4>2, DocUpped
      <4>3. CASE q \in w[s].upDone /\ w[s].pc \in Bar /\ R.baseline[q] = w[s].snap[q] BY <4>3 DEF UI, UIR
      <4>. QED BY <4>1, <4>2, <4>3
    <3>2. \A q \in Paths : R.sHeld[q] = Nil \/ R.sHeld[q] \in upped BY DEF UI, UIR
    <3>3. R.pc \in Bar => \A q \in R.upDone : R.snap[q] \in upped BY DEF UI, UIR
    <3>. QED BY <2>2, <3>1, <3>2, <3>3 DEF UIR
  <2>. QED BY <2>1, <2>2
<1>. QED BY <1>b, <1>c, <1>d DEF M6Goal, UI

\* A step that writes no tree, and no copy.
LEMMA M6NoTree ==
  ASSUME IndC, UI, w' = w, upped \subseteq upped', orig' = orig, minted \subseteq minted',
         \A m \in minted' \ minted : orig[m] = Nil
  PROVE  M6Goal
<1>a. \A t \in Writers : AWt(t) /\ SRt(t) BY M6Same
<1>b. UIo' BY UIoKeep
<1>c. \A t \in Writers : UIR(w[t])' BY DEF UI, UIR
<1>. QED BY <1>a, <1>b, <1>c, M6Per DEF M6Goal, UI

LEMMA TypeOKC == IndC => TypeOK
BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK

------------------------------------------------------------------------------
(* The gateway, the sweep, G, a reader's load, the reap.                    *)

LEMMA GPut_M6 ==
  ASSUME IndC, UI, NEW p \in Paths, GPut(p) PROVE M6Goal
<1>. DEFINE h == <<p, nextGen>>
<1>1. w' = w /\ upped' = upped \cup {h} /\ orig' = orig /\ minted' = minted \cup {h} BY DEF GPut
<1>2. h \notin minted => orig[h] = Nil
  <2>1. nextGen \in Nat /\ nextGen > Seed /\ nextGen <= MaxMint
    BY DEF GPut, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>2. h \in Handles BY <2>1, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed
  <2>. QED BY <2>2, HistNewOrig
<1>. QED BY <1>1, <1>2, M6NoTree

LEMMA Gw_M6 ==
  ASSUME IndC, UI, w' = w, UNCHANGED <<upped, orig, minted>> PROVE M6Goal
BY M6NoTree

LEMMA GRenameFinish_M6 == ASSUME IndC, GRenameFinish PROVE M6Goal
<1>1. mv = Nil BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>. QED BY <1>1 DEF GRenameFinish

------------------------------------------------------------------------------
(* The trees.                                                               *)

LEMMA Simple_M6 ==
  ASSUME IndC, UI, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R],
         UNCHANGED <<upped, orig, minted>>, R.local = w[s].local,
         \A q \in Paths : \/ R.baseline[q] = Nil \/ R.baseline[q] = w[s].baseline[q] \/ R.baseline[q] = doc[q]
                          \/ q \in w[s].upDone /\ w[s].pc \in Bar /\ R.baseline[q] = w[s].snap[q],
         \A q \in Paths : R.sHeld[q] = Nil \/ R.sHeld[q] = w[s].sHeld[q] \/ R.sHeld[q] = w[s].baseline[q],
         R.pc \in Bar => (w[s].pc \in Bar /\ R.upDone \subseteq w[s].upDone /\ R.snap = w[s].snap)
  PROVE  M6Goal
<1>w. \A t \in Writers : w'[t] = IF t = s THEN R ELSE w[t] BY TypeOKC, WriteAny
<1>1. AWt(s) /\ SRt(s) BY <1>w, M6Same
<1>. QED BY <1>w, <1>1, M6Tree

LEMMA Claim_M6 == ASSUME IndC, UI, NEW s \in Writers, Claim(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = ClaimW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF Claim, aux
<1>. QED BY <1>1, Simple_M6 DEF ClaimW, Claim, Bar

LEMMA Verify_M6 == ASSUME IndC, UI, NEW s \in Writers, Verify(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = VerifyW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF Verify, bucket, aux
<1>. QED BY <1>1, Simple_M6 DEF VerifyW

LEMMA Collect_M6 == ASSUME IndC, UI, NEW s \in Writers, Collect(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = CollectW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF Collect, aux
<1>. QED BY <1>1, Simple_M6 DEF CollectW

LEMMA Install_M6 == ASSUME IndC, UI, NEW s \in Writers, Install(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = InstallW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF Install, aux
<1>. QED BY <1>1, Simple_M6 DEF InstallW, Install, Bar

LEMMA Skip_M6 == ASSUME IndC, UI, NEW s \in Writers, Skip(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = SkipW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF Skip, bucket, aux
<1>. QED BY <1>1, Simple_M6 DEF SkipW, Bar

LEMMA PullOnly_M6 == ASSUME IndC, UI, NEW s \in Writers, PullOnly(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = PullOnlyW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF PullOnly, bucket, aux
<1>. QED BY <1>1, Simple_M6 DEF PullOnlyW, Bar

LEMMA Finish_M6 == ASSUME IndC, UI, NEW s \in Writers, Finish(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = FinishW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF Finish, aux
<1>2. \A q \in Paths : \/ FinishW(s).baseline[q] = Nil \/ FinishW(s).baseline[q] = w[s].baseline[q]
                       \/ q \in w[s].upDone /\ w[s].pc \in Bar /\ FinishW(s).baseline[q] = w[s].snap[q]
  BY DEF FinishW, Finish, Bar
<1>. QED BY <1>1, <1>2, Simple_M6 DEF FinishW, Bar

LEMMA Restart_M6 == ASSUME IndC, UI, NEW s \in Writers, Restart(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = RestartW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF Restart, aux
<1>. QED BY <1>1, Simple_M6 DEF RestartW, Bar

LEMMA RescopeBegin_M6 == ASSUME IndC, UI, NEW s \in Writers, RescopeBegin(s) PROVE M6Goal
<1>1. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)]
  BY DEF RescopeBegin
<1>2. UNCHANGED <<upped, orig, minted>> BY DEF RescopeBegin, bucket, aux
<1>. QED BY <1>1, <1>2, Simple_M6 DEF RescopeBeginW, RescopeBegin, Bar

LEMMA RPullRead_M6 == ASSUME IndC, UI, NEW s \in Writers, RPullRead(s) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = RPullReadW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF RPullRead, bucket, aux
<1>. QED BY <1>1, Simple_M6 DEF RPullReadW, Bar

LEMMA RescopeFirst_M6 == ASSUME IndC, UI, NEW s \in Writers, RescopeFirst(s) PROVE M6Goal
<1>sh. RescopeUnciteFirst BY ShippedShape DEF Shipped
<1>1. w' = [w EXCEPT ![s] = RescopeFirstW(s)] /\ UNCHANGED <<upped, orig, minted>> BY DEF RescopeFirst, bucket, aux
<1>2. RescopeFirstW(s).local = w[s].local BY <1>sh DEF RescopeFirstW
<1>3. \A q \in Paths : RescopeFirstW(s).baseline[q] = Nil \/ RescopeFirstW(s).baseline[q] = w[s].baseline[q]
  BY <1>sh DEF RescopeFirstW
<1>4. \A q \in Paths : RescopeFirstW(s).sHeld[q] = w[s].sHeld[q] \/ RescopeFirstW(s).sHeld[q] = w[s].baseline[q]
  BY DEF RescopeFirstW
<1>5. RescopeFirstW(s).pc = "idle" BY DEF RescopeFirstW, RescopeFirst
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5, Simple_M6 DEF Bar

\* The checkout: the tree was never on (Off), so it held nothing.
LEMMA Checkout_M6 == ASSUME IndC, UI, NEW s \in Writers, Checkout(s) PROVE M6Goal
<1>1. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>2. UNCHANGED <<upped, orig, minted>> BY DEF Checkout, bucket, aux
<1>0. w[s] = WriterInit BY DEF Checkout, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Off
<1>w. \A t \in Writers : w'[t] = IF t = s THEN CheckoutW(s, T) ELSE w[t] BY <1>1, TypeOKC, WriteAny
<1>3. AWt(s) /\ SRt(s)
  <2>1. \A q \in Paths : w[s].local[q] = Nil /\ w[s].st = "off" BY <1>0 DEF WriterInit
  <2>. QED BY <2>1 DEF AWt, SRt
<1>4. \A q \in Paths : CheckoutW(s, T).baseline[q] = Nil \/ CheckoutW(s, T).baseline[q] = doc[q]
  BY DEF CheckoutW, CheckoutHeld
<1>5. CheckoutW(s, T).pc = "idle" BY <1>0 DEF CheckoutW, WriterInit
<1>. QED BY <1>w, <1>2, <1>3, <1>4, <1>5, M6Tree DEF CheckoutW, Bar

\* A take: of a clean baseline (PUT), or of the same bytes; never of a path
\* the tree neither holds nor admits.
LEMMA TakeAW ==
  ASSUME IndC, UI, NEW s \in Writers, NEW taken, UNCHANGED orig,
         \A q \in taken : \/ w[s].local[q] = w[s].baseline[q] /\ Held(s, q)
                          \/ Held(s, q) /\ w[s].local[q] # Nil /\ doc[q] # Nil
                             /\ Content(w[s].local[q]) = Content(doc[q])
                          \/ Held(s, q) /\ w[s].local[q] = doc[q],
         \A q \in Paths : w'[s].local[q] = IF q \in taken THEN doc[q] ELSE w[s].local[q],
         w'[s].scope = w[s].scope
  PROVE  AWt(s) /\ SRt(s)
<1>y. \A q \in Paths : w[s].local[q] \in Opt(minted) BY TreeTy DEF IndC
<1>1. AWt(s)
  <2>. SUFFICES ASSUME NEW q \in Paths, w[s].local[q] # Nil, w[s].local[q] \notin upped,
                       w'[s].local[q] # w[s].local[q],
                       ~(w'[s].local[q] # Nil /\ ContentNext(w'[s].local[q]) = w[s].local[q])
                PROVE  FALSE
    BY DEF AWt
  <2>1. q \in taken BY DEF AWt
  <2>h. w[s].local[q] \in minted BY <1>y DEF Opt
  <2>2. CASE w[s].local[q] = w[s].baseline[q] BY <2>2, <2>h DEF UI, UIR
  <2>3. CASE Held(s, q) /\ w[s].local[q] # Nil /\ doc[q] # Nil /\ Content(w[s].local[q]) = Content(doc[q])
    <3>1. Content(w[s].local[q]) = w[s].local[q] BY <2>h, OwnBytes
    <3>2. w'[s].local[q] = doc[q] BY <2>1
    <3>3. ContentNext(doc[q]) = Content(doc[q]) BY DEF ContentNext, Content
    <3>. QED BY <2>3, <3>1, <3>2, <3>3
  <2>4. CASE Held(s, q) /\ w[s].local[q] = doc[q] BY <2>1, <2>4
  <2>. QED BY <2>1, <2>2, <2>3, <2>4
<1>2. SRt(s)
  <2>. SUFFICES ASSUME NEW q \in Paths, q \notin w[s].scope, w[s].baseline[q] = Nil,
                       w'[s].local[q] # w[s].local[q]
                PROVE  FALSE
    BY DEF SRt
  <2>1. ~Held(s, q) BY DEF Held
  <2>2. q \notin taken BY <2>1
  <2>. QED BY <2>2
<1>. QED BY <1>1, <1>2

LEMMA Consume_M6 == ASSUME IndC, UI, NEW s \in Writers, Consume(s) PROVE M6Goal
<1>k. UNCHANGED <<upped, orig, minted>> BY DEF Consume, bucket, aux
<1>sh. ConsumeHonorsScope /\ ContentConverges BY ShippedShape DEF Shipped
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s)] BY <1>1 DEF Consume
  <2>. QED BY <2>1, <1>k, Simple_M6 DEF ConsumeCheapW, Bar
<1>2. CASE ~CheapPath(s)
  <2>1. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail)] BY <1>2 DEF Consume
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN ConsumeW(s, fail) ELSE w[t] BY <2>1, TypeOKC, WriteAny
  <2>2. AWt(s) /\ SRt(s)
    <3>1. \A q \in ConsumeTaken(s, fail) :
             \/ w[s].local[q] = w[s].baseline[q] /\ Held(s, q)
             \/ Held(s, q) /\ w[s].local[q] # Nil /\ doc[q] # Nil /\ Content(w[s].local[q]) = Content(doc[q])
             \/ Held(s, q) /\ w[s].local[q] = doc[q]
      BY <1>sh DEF ConsumeTaken, ConsumeOwed, ConsumeConv, Owed, Converges
    <3>2. \A q \in Paths : w'[s].local[q] = IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].local[q]
      BY <2>w DEF ConsumeW
    <3>3. w'[s].scope = w[s].scope BY <2>w DEF ConsumeW
    <3>. QED BY <3>1, <3>2, <3>3, <1>k, TakeAW
  <2>3. \A q \in Paths : ConsumeW(s, fail).baseline[q] = w[s].baseline[q] \/ ConsumeW(s, fail).baseline[q] = doc[q]
    BY DEF ConsumeW
  <2>. QED BY <2>w, <1>k, <2>2, <2>3, M6Tree DEF ConsumeW, Bar
<1>. QED BY <1>1, <1>2

LEMMA Sync_M6 == ASSUME IndC, UI, NEW s \in Writers, Sync(s) PROVE M6Goal
<1>k. UNCHANGED <<upped, orig, minted>> BY DEF Sync, bucket, aux
<1>sh. ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>w. \A t \in Writers : w'[t] = IF t = s THEN SyncW(s, fail) ELSE w[t] BY <1>1, TypeOKC, WriteAny
<1>2. AWt(s) /\ SRt(s)
  <2>1. \A q \in SyncOwed(s, fail) : w[s].local[q] = w[s].baseline[q] /\ Held(s, q)
    BY <1>sh DEF SyncOwed, SyncAll, Owed
  <2>2. \A q \in Paths : w'[s].local[q] = IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]
    BY <1>w DEF SyncW
  <2>3. w'[s].scope = w[s].scope BY <1>w DEF SyncW
  <2>4. \A q \in SyncOwed(s, fail) :
          \/ w[s].local[q] = w[s].baseline[q] /\ Held(s, q)
          \/ Held(s, q) /\ w[s].local[q] # Nil /\ doc[q] # Nil /\ Content(w[s].local[q]) = Content(doc[q])
          \/ Held(s, q) /\ w[s].local[q] = doc[q]
    BY <2>1
  <2>. QED BY <2>4, <2>2, <2>3, <1>k, TakeAW
<1>3. \A q \in Paths : SyncW(s, fail).baseline[q] = w[s].baseline[q] \/ SyncW(s, fail).baseline[q] = doc[q]
  BY DEF SyncW, SyncBl
<1>4. SyncW(s, fail).pc = "idle" BY DEF SyncW, Sync
<1>. QED BY <1>w, <1>k, <1>2, <1>3, <1>4, M6Tree DEF SyncW, Bar

LEMMA RPullSync_M6 == ASSUME IndC, UI, NEW s \in Writers, RPullSync(s) PROVE M6Goal
<1>k. UNCHANGED <<upped, orig, minted>> BY DEF RPullSync, bucket, aux
<1>sh. ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>w. \A t \in Writers : w'[t] = IF t = s THEN RPullSyncW(s, fail) ELSE w[t] BY <1>1, TypeOKC, WriteAny
<1>2. AWt(s) /\ SRt(s)
  <2>1. \A q \in SyncOwed(s, fail) : w[s].local[q] = w[s].baseline[q] /\ Held(s, q)
    BY <1>sh DEF SyncOwed, SyncAll, Owed
  <2>2. \A q \in Paths : w'[s].local[q] = IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]
    BY <1>w DEF RPullSyncW
  <2>3. w'[s].scope = w[s].scope BY <1>w DEF RPullSyncW
  <2>. QED BY <2>1, <2>2, <2>3, <1>k, TakeAW
<1>3. \A q \in Paths : RPullSyncW(s, fail).baseline[q] = w[s].baseline[q] \/ RPullSyncW(s, fail).baseline[q] = doc[q]
  BY DEF RPullSyncW, SyncBl
<1>. QED BY <1>w, <1>k, <1>2, <1>3, M6Tree DEF RPullSyncW, Bar

\* The widen and the unlink.
LEMMA RescopeSecond_M6 == ASSUME IndC, UI, NEW s \in Writers, RescopeSecond(s) PROVE M6Goal
<1>k. UNCHANGED <<upped, orig, minted>> BY DEF RescopeSecond, bucket, aux
<1>sh. RescopeUnciteFirst /\ WidenKeepsLocal /\ UnlinkChecksBytes BY ShippedShape DEF Shipped
<1>1. PICK wfail \in SUBSET {p \in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>w. \A t \in Writers : w'[t] = IF t = s THEN RescopeSecondW(s, wfail) ELSE w[t] BY <1>1, TypeOKC, WriteAny
<1>y. \A q \in Paths : w[s].local[q] \in Opt(minted) BY TreeTy DEF IndC
<1>l. \A q \in Paths : w'[s].local[q] =
        IF q \in RescopeFetch(s, wfail) /\ RescopeLocal1(s)[q] = Nil THEN doc[q] ELSE RescopeLocal1(s)[q]
  BY <1>w, <1>sh DEF RescopeSecondW
<1>l1. \A q \in Paths : RescopeLocal1(s)[q] = IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]
  BY <1>sh DEF RescopeLocal1
<1>sc. w'[s].scope = w[s].sTgt BY <1>w DEF RescopeSecondW
<1>2. AWt(s)
  <2>. SUFFICES ASSUME NEW q \in Paths, w[s].local[q] # Nil, w[s].local[q] \notin upped,
                       w'[s].local[q] # w[s].local[q]
                PROVE  FALSE
    BY DEF AWt
  <2>h. w[s].local[q] \in minted BY <1>y DEF Opt
  <2>1. RescopeUnlink(s, q) BY <1>l, <1>l1
  <2>2. w[s].sHeld[q] # Nil /\ Content(w[s].local[q]) = Content(w[s].sHeld[q]) BY <2>1, <1>sh DEF RescopeUnlink
  <2>3. w[s].sHeld[q] \in upped BY <2>2 DEF UI, UIR
  <2>4. Content(w[s].local[q]) = w[s].local[q] BY <2>h, OwnBytes
  <2>m. w[s].sHeld[q] \in minted BY <2>3 DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <2>5. CASE orig[w[s].sHeld[q]] = Nil BY <2>2, <2>3, <2>4, <2>5 DEF Content
  <2>6. CASE orig[w[s].sHeld[q]] # Nil
    <3>1. orig[w[s].sHeld[q]] \in upped BY <2>6, <2>m DEF UI, UIo
    <3>. QED BY <2>2, <2>4, <2>6, <3>1 DEF Content
  <2>. QED BY <2>5, <2>6
<1>3. SRt(s)
  <2>. SUFFICES ASSUME NEW q \in Paths, q \notin w[s].scope \cup w'[s].scope,
                       w'[s].local[q] # w[s].local[q],
                       ~(w'[s].local[q] = Nil /\ q \in w[s].sDrop)
                PROVE  FALSE
    BY DEF SRt
  <2>1. q \notin RescopeFetch(s, wfail) BY <1>sc DEF RescopeFetch, RescopeFetch0, RescopeAdd
  <2>2. RescopeUnlink(s, q) BY <2>1, <1>l, <1>l1
  <2>3. w'[s].local[q] = Nil /\ q \in w[s].sDrop BY <2>1, <2>2, <1>l, <1>l1 DEF RescopeUnlink, RescopeDrop
  <2>. QED BY <2>3
<1>4. \A q \in Paths : RescopeSecondW(s, wfail).baseline[q] = w[s].baseline[q] \/ RescopeSecondW(s, wfail).baseline[q] = doc[q]
  BY <1>sh DEF RescopeSecondW, RescopeBase1
<1>5. RescopeSecondW(s, wfail).pc = "idle" BY DEF RescopeSecondW, RescopeSecond
<1>6. \A q \in Paths : RescopeSecondW(s, wfail).sHeld[q] = Nil BY DEF RescopeSecondW
<1>. QED BY <1>w, <1>k, <1>2, <1>3, <1>4, <1>5, <1>6, M6Tree DEF Bar

LEMMA Edit_M6 == ASSUME IndC, UI, NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE M6Goal
<1>. DEFINE hh == <<p, nextGen>>
<1>1. w' = [w EXCEPT ![s] = EditW(s, p)] /\ minted' = minted \cup {hh} /\ UNCHANGED <<upped, orig>>
  BY DEF Edit, aux
<1>w. \A t \in Writers : w'[t] = IF t = s THEN EditW(s, p) ELSE w[t] BY <1>1, TypeOKC, WriteAny
<1>a. AgentStep(s) BY DEF AgentStep
<1>2. \A t \in Writers : AWt(t) /\ SRt(t)
  <2>. SUFFICES ASSUME NEW t \in Writers PROVE AWt(t) /\ SRt(t) OBVIOUS
  <2>1. CASE t = s BY <2>1, <1>a, M6Agent
  <2>2. CASE t # s BY <2>2, <1>w, M6Same
  <2>. QED BY <2>1, <2>2
<1>3. hh \notin minted => orig[hh] = Nil
  <2>1. nextGen \in Nat /\ nextGen > Seed /\ nextGen <= MaxMint
    BY DEF Edit, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>2. hh \in Handles BY <2>1, MaxCopiesNat, MaxMintNat DEF Handles, Gens, Seed
  <2>. QED BY <2>2, HistNewOrig
<1>4. UIo' BY <1>1, <1>3, UIoKeep
<1>5. \A t \in Writers : UIR(w[t])' BY <1>w, <1>1 DEF UI, UIR, EditW
<1>. QED BY <1>2, <1>4, <1>5, M6Per DEF M6Goal, UI

LEMMA Delete_M6 == ASSUME IndC, UI, NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE M6Goal
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] /\ UNCHANGED <<upped, orig, minted>> BY DEF Delete, bucket, aux
<1>w. \A t \in Writers : w'[t] = IF t = s THEN DeleteW(s, p) ELSE w[t] BY <1>1, TypeOKC, WriteAny
<1>a. AgentStep(s) BY DEF AgentStep
<1>2. AWt(s) /\ SRt(s) BY <1>a, M6Agent
<1>. QED BY <1>w, <1>1, <1>2, M6Tree DEF DeleteW

\* The upload: a plain one PUTs the snapshot; a copy is of a PUT snapshot
\* and holds its bytes.
LEMMA Upload_M6 == ASSUME IndC, UI, NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE M6Goal
<1>y. /\ \A q \in Paths : w[s].local[q] \in Opt(minted) /\ w[s].snap[q] \in Opt(minted)
      /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ w[s].uploads \subseteq Paths /\ minted \subseteq Handles /\ w[s] \in Writer
  BY TreeTy DEF IndC
<1>k. w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
<1>h0. PICK h0 \in minted : h0 = w[s].snap[p] BY <1>k, <1>y
<1>1. CASE h0 \notin upped
  <2>1. w' = [w EXCEPT ![s] = UploadW(s, p)] /\ upped' = upped \cup {h0} /\ UNCHANGED <<minted, orig>>
    BY <1>1, <1>h0 DEF Upload
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN UploadW(s, p) ELSE w[t] BY <2>1, TypeOKC, WriteAny
  <2>2. \A t \in Writers : AWt(t) /\ SRt(t) BY <2>w, M6Same DEF UploadW
  <2>3. UIo' BY <2>1, UIoKeep
  <2>4. \A t \in Writers : UIR(w[t])'
    <3>. SUFFICES ASSUME NEW t \in Writers PROVE UIR(w[t])' OBVIOUS
    <3>1. CASE t # s BY <3>1, <2>w, <2>1 DEF UI, UIR
    <3>2. CASE t = s
      <4>1. \A q \in w[s].upDone \cup {p} : w[s].snap[q] \in upped' BY <2>1, <1>h0, <1>k DEF UI, UIR, Bar
      <4>. QED BY <3>2, <2>w, <2>1, <4>1 DEF UI, UIR, UploadW
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>2, <2>3, <2>4, M6Per DEF M6Goal, UI
<1>2. CASE h0 \in upped
  <2>g. copies \in Nat BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Ghosts
  <2>c. PICK c \in Handles : c = <<p, MaxMint + copies + 1>>
    BY <1>2, <1>h0, <2>g, MaxMintNat, MaxCopiesNat DEF Upload, Handles, Gens, Seed
  <2>1. /\ minted' = minted \cup {c} /\ upped' = upped \cup {c}
        /\ orig' = [orig EXCEPT ![c] = Content(h0)] /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]
    BY <1>2, <1>h0, <2>c DEF Upload
  <2>ty. orig \in [Handles -> Opt(Handles)] BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Ghosts
  <2>4. c \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ (Gen(m) > MaxMint /\ Gen(m) <= MaxMint + copies)
      BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>2. nextGen <= MaxMint + 1 /\ nextGen \in Nat
      BY DEF IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh, IndM1, IndTypeOK, TypeOK
    <3>. QED BY <3>1, <3>2, <2>c, <2>g, MaxMintNat DEF Gen
  <2>w. \A t \in Writers : w'[t] = IF t = s THEN UploadCopyW(s, p, c) ELSE w[t] BY <2>1, TypeOKC, WriteAny
  <2>lt. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)] BY <1>y, WriterFields
  <2>l. \A q \in Paths : /\ w'[s].local[q] = IF q = p /\ w[s].local[p] = h0 THEN c ELSE w[s].local[q]
                         /\ w'[s].snap[q] = IF q = p THEN c ELSE w[s].snap[q]
    BY <2>w, <2>lt, <1>h0 DEF UploadCopyW
  <2>ch. Content(h0) \in upped
    <3>1. CASE orig[h0] = Nil BY <3>1, <1>2 DEF Content
    <3>2. CASE orig[h0] # Nil BY <3>2, <1>h0 DEF UI, UIo, Content
    <3>. QED BY <3>1, <3>2
  <2>cn. Content(h0) # Nil BY <2>ch, NilHandle DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Ghosts
  <2>cc. ContentNext(c) = Content(h0) BY <2>1, <2>ty, <2>cn, NilHandle DEF ContentNext
  <2>2. AWt(s) /\ SRt(s)
    <3>1. AWt(s)
      <4>. SUFFICES ASSUME NEW q \in Paths, w[s].local[q] # Nil, w[s].local[q] \notin upped,
                           w'[s].local[q] # w[s].local[q]
                    PROVE  FALSE
        BY DEF AWt
      <4>. QED BY <2>l, <1>2, <1>h0
    <3>2. SRt(s)
      <4>. SUFFICES ASSUME NEW q \in Paths, w'[s].local[q] # w[s].local[q],
                           ~(w'[s].local[q] # Nil /\ w[s].local[q] # Nil
                             /\ ContentNext(w'[s].local[q]) = Content(w[s].local[q]))
                    PROVE  FALSE
        BY DEF SRt
      <4>1. q = p /\ w[s].local[p] = h0 /\ w'[s].local[q] = c BY <2>l
      <4>2. h0 # Nil /\ c # Nil BY <1>h0, <1>y, NilHandle
      <4>. QED BY <4>1, <4>2, <2>cc
    <3>. QED BY <3>1, <3>2
  <2>3. \A t \in Writers \ {s} : AWt(t) /\ SRt(t) BY <2>w, M6Same
  <2>5. UIo'
    <3>. SUFFICES ASSUME NEW m \in minted', orig'[m] # Nil PROVE orig'[m] \in upped' /\ m \in upped' BY DEF UIo
    <3>1. CASE m = c BY <3>1, <2>1, <2>ty, <2>ch
    <3>2. CASE m # c
      <4>1. m \in minted /\ orig'[m] = orig[m] BY <3>2, <2>1, <2>ty, MintedHandles DEF IndC
      <4>. QED BY <4>1, <2>1 DEF UI, UIo
    <3>. QED BY <3>1, <3>2
  <2>6. \A t \in Writers : UIR(w[t])'
    <3>. SUFFICES ASSUME NEW t \in Writers PROVE UIR(w[t])' OBVIOUS
    <3>1. CASE t # s BY <3>1, <2>w, <2>1 DEF UI, UIR
    <3>2. CASE t = s
      <4>1. w'[s].baseline = w[s].baseline /\ w'[s].sHeld = w[s].sHeld /\ w'[s].pc = w[s].pc
            /\ w'[s].upDone = w[s].upDone \cup {p}
        BY <2>w DEF UploadCopyW
      <4>2. \A q \in w[s].upDone \cup {p} : w'[s].snap[q] \in upped'
        <5>. SUFFICES ASSUME NEW q \in w[s].upDone \cup {p} PROVE w'[s].snap[q] \in upped' OBVIOUS
        <5>q. q \in Paths BY TreeTy, WriterFields DEF IndC
        <5>1. CASE q = p BY <5>1, <2>l, <2>1
        <5>2. CASE q # p BY <5>2, <5>q, <2>l, <2>1, <1>k DEF UI, UIR, Bar
        <5>. QED BY <5>1, <5>2
      <4>. QED BY <3>2, <4>1, <4>2, <2>1 DEF UI, UIR
    <3>. QED BY <3>1, <3>2
  <2>. QED BY <2>2, <2>3, <2>5, <2>6, M6Per DEF M6Goal, UI
<1>. QED BY <1>1, <1>2

LEMMA Scan_M6 == ASSUME IndC, UI, NEW s \in Writers, Scan(s) PROVE M6Goal
<1>1. PICK dels \in SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] BY DEF Scan
<1>2. UNCHANGED <<upped, orig, minted>> BY DEF Scan, bucket, aux
<1>w. \A t \in Writers : w'[t] = IF t = s THEN ScanW(s, dels) ELSE w[t] BY <1>1, TypeOKC, WriteAny
<1>3. AWt(s) /\ SRt(s) BY <1>w, M6Same DEF ScanW
<1>4. \A t \in Writers \ {s} : AWt(t) /\ SRt(t) BY <1>w, M6Same
<1>5. \A t \in Writers : UIR(w[t])'
  <2>. SUFFICES ASSUME NEW t \in Writers PROVE UIR(w[t])' OBVIOUS
  <2>1. CASE t # s BY <2>1, <1>w, <1>2 DEF UI, UIR
  <2>2. CASE t = s BY <2>2, <1>w, <1>2 DEF UI, UIR, ScanW
  <2>. QED BY <2>1, <2>2
<1>6. UIo' BY <1>2, UIoKeep
<1>. QED BY <1>3, <1>4, <1>5, <1>6, M6Per DEF M6Goal, UI

------------------------------------------------------------------------------
(* The step, the invariant, the claims.                                     *)

LEMMA Next_M6 == IndC /\ UI /\ Next => M6Goal
<1>. SUFFICES ASSUME IndC, UI, Next PROVE M6Goal OBVIOUS
<1>1. CASE GatewayStep \/ WriterStep
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE M6Goal BY <3>1, GPut_M6
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE M6Goal BY <3>2, Gw_M6 DEF GCas, aux
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE M6Goal BY <3>3, Gw_M6 DEF GDelete, aux
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE M6Goal BY <3>4, Gw_M6 DEF GRename, aux
    <3>5. ASSUME GRenameFinish PROVE M6Goal BY <3>5, GRenameFinish_M6
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE M6Goal BY <3>1, Checkout_M6
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE M6Goal BY <3>2, Consume_M6
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE M6Goal BY <3>3, Scan_M6
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE M6Goal BY <3>4, Skip_M6
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE M6Goal BY <3>5, PullOnly_M6
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE M6Goal BY <3>6, Claim_M6
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE M6Goal BY <3>7, Verify_M6
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE M6Goal BY <3>8, Install_M6
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE M6Goal BY <3>9, Collect_M6
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE M6Goal BY <3>10, Finish_M6
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE M6Goal BY <3>11, Restart_M6
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE M6Goal BY <3>12, Sync_M6
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE M6Goal BY <3>13, RescopeBegin_M6
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE M6Goal BY <3>14, RescopeFirst_M6
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE M6Goal BY <3>15, RescopeSecond_M6
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE M6Goal BY <3>16, RPullRead_M6
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE M6Goal BY <3>17, RPullSync_M6
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE M6Goal BY <3>18, Edit_M6
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE M6Goal BY <3>19, Delete_M6
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE M6Goal BY <3>20, Upload_M6
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE M6Goal
      BY <3>21, Gw_M6 DEF Sweep, aux
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)
  <2>1. w' = w /\ UNCHANGED <<upped, orig, minted>> BY <1>2 DEF Age, RLoad, Reap, aux
  <2>. QED BY <2>1, Gw_M6
<1>. QED BY <1>1, <1>2 DEF Next

LEMMA Stutter_M6 == IndC /\ UI /\ UNCHANGED vars => M6Goal
<1>. SUFFICES ASSUME IndC, UI, UNCHANGED vars PROVE M6Goal OBVIOUS
<1>1. w' = w /\ UNCHANGED <<upped, orig, minted>> BY DEF vars, aux
<1>. QED BY <1>1, Gw_M6

LEMMA Init_M6 == Init => UI
<1>. SUFFICES ASSUME Init PROVE UI OBVIOUS
<1>1. \A u \in Writers : w[u] = WriterInit BY DEF Init
<1>2. orig = [h \in Handles |-> Nil] BY DEF Init
<1>3. minted \subseteq Handles BY MaxMintNat, MaxCopiesNat DEF Init, Handles, Gens, Seed
<1>. QED BY <1>1, <1>2, <1>3 DEF UI, UIR, UIo, WriterInit, Bar

LEMMA UIInvariant == Spec => []UI
<1>1. IndC /\ UI /\ [Next]_vars => UI' BY Next_M6, Stutter_M6 DEF M6Goal
<1>. QED BY Init_M6, <1>1, CInvariant, PTL DEF Spec

THEOREM AgentWorkKept == Spec => Prop_AgentWorkKept
<1>1. IndC /\ UI /\ [Next]_vars => [AWA]_vars BY Next_M6, Stutter_M6 DEF M6Goal
<1>. QED BY <1>1, UIInvariant, CInvariant, PTL DEF Spec, Prop_AgentWorkKept, AWA

THEOREM ScopeRespected == Spec => Prop_ScopeRespected
<1>1. IndC /\ UI /\ [Next]_vars => [SRA]_vars BY Next_M6, Stutter_M6 DEF M6Goal
<1>. QED BY <1>1, UIInvariant, CInvariant, PTL DEF Spec, Prop_ScopeRespected, SRA
==============================================================================
