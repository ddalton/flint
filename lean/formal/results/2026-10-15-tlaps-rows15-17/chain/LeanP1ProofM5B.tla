--------------------------- MODULE LeanP1ProofM5B ----------------------------
EXTENDS LeanP1ProofM5A

------------------------------------------------------------------------------
(* M5, part B: Prop_NoSilentRevert.  A step replaces a published version  *)
(* with another at only two places: the gateway's CAS (GCas), which lands  *)
(* only over the version the save read -- so the new one derives from it  *)
(* through `anc` (HistAnc) -- and a commit (Install), which publishes over *)
(* a version not its own baseline only by recording it (CommitSurfaces-   *)
(* Foreign).  No other M5 claim is used: part A and the typing suffice.   *)

NoRev == \A p \in Paths : ~SilentRevert(p)

\* A step that, at every path, keeps the version, clears it, or fills an
\* empty path, reverts nothing.
LEMMA RevClear ==
  (\A p \in Paths : doc'[p] = doc[p] \/ doc'[p] = Nil \/ doc[p] = Nil) => NoRev
  BY DEF NoRev, SilentRevert

LEMMA GCas_Rev ==
  ASSUME IndM5, NEW p0 \in Paths, GCas(p0)
  PROVE  NoRev
<1>. DEFINE h == gw[p0]
<1>y. /\ doc \in [Paths -> Opt(Handles)] /\ gw \in [Paths -> Opt(Handles)] /\ minted \subseteq Handles
      /\ Minted
  BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>1. /\ h # Nil
      /\ doc' = IF doc[p0] = base[h] THEN [doc EXCEPT ![p0] = h] ELSE doc
  BY ShippedShape DEF GCas, Shipped
<1>2. h \in minted /\ h \in Handles BY <1>1, <1>y DEF Minted, Opt
<1>3. SUFFICES ASSUME NEW p \in Paths, SilentRevert(p) PROVE FALSE BY DEF NoRev
<1>4. /\ doc[p] # Nil /\ doc'[p] # doc[p]
      /\ ~Supersedes(doc'[p], doc[p], p)
  BY <1>3 DEF SilentRevert
<1>5. p = p0 /\ doc[p0] = base[h] /\ doc'[p] = h BY <1>1, <1>4, <1>y
<1>6. anc[h] = {base[h]} \cup anc[base[h]]
  BY <1>2, <1>4, <1>5 DEF IndM5, M5, Hist, HistAnc, AncOf
<1>7. Derives(h, doc[p]) BY <1>1, <1>5, <1>6 DEF Derives
<1>. QED BY <1>4, <1>5, <1>7 DEF Supersedes

LEMMA Install_Rev ==
  ASSUME IndM5, NEW s \in Writers, Install(s)
  PROVE  NoRev
<1>1. /\ w[s].pc = "claimed" /\ doc' = InstallInst(s)
      /\ {<<q, doc[q]>> : q \in InstallContested(s)} \subseteq conflicts'
  BY DEF Install
<1>2. SUFFICES ASSUME NEW p \in Paths, SilentRevert(p) PROVE FALSE BY DEF NoRev
<1>3. /\ doc[p] # Nil /\ doc'[p] # Nil /\ doc'[p] # doc[p]
      /\ ~\E c \in conflicts' : c[2] = doc[p]
      /\ ~\E t \in Writers : w[t].pc = "claimed"
                             /\ (w[t].baseline[p] = doc[p] \/ Gen(doc[p]) \in took[t][p])
  BY <1>2 DEF SilentRevert
<1>4. p \in InstallMine(s) /\ p \notin w[s].gone BY <1>1, <1>3 DEF InstallInst
<1>5. CASE Foreign(s, p)
  <2>1. p \in InstallContested(s) BY <1>3, <1>4, <1>5, ShippedShape DEF InstallContested, Shipped
  <2>2. <<p, doc[p]>> \in conflicts' BY <1>1, <2>1
  <2>. QED BY <1>3, <2>2
<1>6. CASE ~Foreign(s, p)
  <2>1. w[s].baseline[p] = doc[p] BY <1>6 DEF Foreign
  <2>. QED BY <1>1, <1>3, <2>1
<1>. QED BY <1>5, <1>6

LEMMA Next_Rev == IndM5 /\ Next => NoRev
<1>. SUFFICES ASSUME IndM5, Next PROVE NoRev OBVIOUS
<1>y. doc \in [Paths -> Opt(Handles)] BY DEF IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>1. CASE GatewayStep
  <2>1. ASSUME NEW p \in Paths, GPut(p) PROVE NoRev BY <2>1, RevClear DEF GPut
  <2>2. ASSUME NEW p \in Paths, GCas(p) PROVE NoRev BY <2>2, GCas_Rev
  <2>3. ASSUME NEW p \in Paths, GDelete(p) PROVE NoRev BY <2>3, <1>y, RevClear DEF GDelete
  <2>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE NoRev
    BY <2>4, <1>y, RevClear, ShippedShape DEF GRename, Shipped
  <2>5. CASE GRenameFinish BY <2>5, <1>y, RevClear DEF GRenameFinish
  <2>. QED BY <1>1, <2>1, <2>2, <2>3, <2>4, <2>5 DEF GatewayStep
<1>2. CASE WriterStep
  <2>1. ASSUME NEW s \in Writers, Checkout(s) PROVE NoRev BY <2>1, RevClear DEF Checkout, bucket
  <2>2. ASSUME NEW s \in Writers, Consume(s) PROVE NoRev BY <2>2, RevClear DEF Consume, bucket
  <2>3. ASSUME NEW s \in Writers, Scan(s) PROVE NoRev BY <2>3, RevClear DEF Scan, bucket
  <2>4. ASSUME NEW s \in Writers, Skip(s) PROVE NoRev BY <2>4, RevClear DEF Skip, bucket
  <2>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE NoRev BY <2>5, RevClear DEF PullOnly, bucket
  <2>6. ASSUME NEW s \in Writers, Claim(s) PROVE NoRev BY <2>6, RevClear DEF Claim, bucket
  <2>7. ASSUME NEW s \in Writers, Verify(s) PROVE NoRev BY <2>7, RevClear DEF Verify, bucket
  <2>8. ASSUME NEW s \in Writers, Collect(s) PROVE NoRev BY <2>8, RevClear DEF Collect, bucket
  <2>9. ASSUME NEW s \in Writers, Finish(s) PROVE NoRev BY <2>9, RevClear DEF Finish, bucket
  <2>10. ASSUME NEW s \in Writers, Restart(s) PROVE NoRev BY <2>10, RevClear DEF Restart, bucket
  <2>11. ASSUME NEW s \in Writers, Sync(s) PROVE NoRev BY <2>11, RevClear DEF Sync, bucket
  <2>12. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE NoRev BY <2>12, RevClear DEF RescopeBegin, bucket
  <2>13. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE NoRev BY <2>13, RevClear DEF RescopeFirst, bucket
  <2>14. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE NoRev BY <2>14, RevClear DEF RescopeSecond, bucket
  <2>15. ASSUME NEW s \in Writers, RPullRead(s) PROVE NoRev BY <2>15, RevClear DEF RPullRead, bucket
  <2>16. ASSUME NEW s \in Writers, RPullSync(s) PROVE NoRev BY <2>16, RevClear DEF RPullSync, bucket
  <2>17. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE NoRev BY <2>17, RevClear DEF Edit, bucket
  <2>18. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE NoRev BY <2>18, RevClear DEF Delete, bucket
  <2>19. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE NoRev BY <2>19, RevClear DEF Upload, bucket
  <2>20. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE NoRev BY <2>20, RevClear DEF Sweep, bucket
  <2>21. ASSUME NEW s \in Writers, Install(s) PROVE NoRev BY <2>21, Install_Rev
  <2>22. ASSUME NEW s \in Writers, Touch(s) PROVE NoRev BY <2>22, RevClear DEF Touch, bucket
  <2>23. ASSUME NEW s \in Writers, Honor(s) PROVE NoRev BY <2>23, RevClear DEF Honor, bucket
  <2>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE NoRev BY <2>24, RevClear DEF HonorReader, bucket
  <2>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE NoRev BY <2>25, RevClear DEF RetireByAck, bucket
  <2>26. ASSUME NEW s \in Writers, Depose(s) PROVE NoRev BY <2>26, RevClear DEF Depose
  <2>27. ASSUME NEW s \in Writers, Fence(s) PROVE NoRev BY <2>27, RevClear DEF Fence, bucket
  <2>. QED BY <1>2, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13, <2>14, <2>15, <2>16, <2>17, <2>18, <2>19, <2>20, <2>21,
              <2>22, <2>23, <2>24, <2>25, <2>26, <2>27 DEF WriterStep
<1>3. CASE Age BY <1>3, RevClear DEF Age
<1>4. CASE RLoad BY <1>4, RevClear DEF RLoad
<1>5. ASSUME NEW s \in Writers, NEW h \in Handles, Reap(s, h) PROVE NoRev BY <1>5, RevClear DEF Reap
<1>. QED BY <1>1, <1>2, <1>3, <1>4, <1>5 DEF Next

THEOREM NoSilentRevert == Spec => Prop_NoSilentRevert
<1>1. IndM5 /\ [Next]_vars => [NoRev]_vars
  <2>1. IndM5 /\ Next => NoRev BY Next_Rev
  <2>. QED BY <2>1
<1>. QED BY M5Invariant, <1>1, PTL DEF Spec, Prop_NoSilentRevert, NoRev

==============================================================================
