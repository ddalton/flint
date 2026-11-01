\* (S) StageAdmit's existential body for source s, with the elems' lower bound a parameter (opaque: the PICK
\* matches it with the model's bound, and BaseInclusive then rewrites the argument, not the binders under it)
ZK_AdmBody(r, s, lo) ==
  LET lineS == Lin(s) \cup {nextSeq}
      elems == {e \in lineS : lo <= e /\ e <= nextSeq}
      alOf(e) == IF e = nextSeq THEN hAl[s] ELSE sAl[s][e]
      conOf(e) == IF e = nextSeq THEN head[s] ELSE sCon[s][e]
      dHead == [x \in Blocks |->
                 LET hits == {e \in elems : x \in alOf(e)}
                 IN IF hits = {} THEN head[r][x] ELSE conOf(Max(hits))[x]]
      dAl == hAl[r] \cup {x \in Blocks : \E e \in elems : x \in alOf(e)}
  IN /\ sPres' = [q \in R |-> IF q \in AsmSet \cup {r} THEN sPres[q] \cup {nextSeq} ELSE sPres[q]]
     /\ sCon' = [q \in R |-> IF q \in AsmSet THEN [sCon[q] EXCEPT ![nextSeq] = head[q]]
                             ELSE IF q = r THEN [sCon[q] EXCEPT ![nextSeq] = dHead]
                             ELSE sCon[q]]
     /\ sAl' = [q \in R |-> IF q \in AsmSet THEN [sAl[q] EXCEPT ![nextSeq] = hAl[q]]
                            ELSE IF q = r THEN [sAl[q] EXCEPT ![nextSeq] = dAl]
                            ELSE sAl[q]]
     /\ sPar' = [q \in R |-> IF q \in AsmSet \cup {r} THEN [sPar[q] EXCEPT ![nextSeq] = hPar[q]]
                             ELSE sPar[q]]
     /\ anc' = [q \in R |-> IF q \in AsmSet \cup {r} THEN [anc[q] EXCEPT ![nextSeq] = NewAnc(q, nextSeq)]
                            ELSE anc[q]]
     /\ hPar' = [q \in R |-> IF q \in AsmSet \cup {r} THEN nextSeq ELSE hPar[q]]
     /\ hAl' = [q \in R |-> IF q \in AsmSet \cup {r} THEN {} ELSE hAl[q]]
     /\ head' = [head EXCEPT ![r] = dHead]
     /\ badAdmit' = (badAdmit \/ dHead # head[s])
     /\ badLoss' = (badLoss \/ \E x \in Blocks :
                       \/ head[s][x] >= acked[x] /\ dHead[x] < acked[x]
                       \/ dHead[x] \notin ackedEver[x] \cup {head[q][x] : q \in AsmSet})
     /\ IF Policy = "trust"
          THEN /\ tr' = [q \in R |-> IF (q \in AsmSet /\ htr[q]) \/ (q = r /\ htr[s])
                                     THEN tr[q] \cup {nextSeq} ELSE tr[q] \ {nextSeq}]
               /\ cutAck' = [q \in R |-> IF q \in AsmSet \cup {r} THEN [cutAck[q] EXCEPT ![nextSeq] = acked]
                                         ELSE cutAck[q]]
               /\ htr' = [q \in R |-> FALSE]
          ELSE UNCHANGED <<tr, htr, cutAck>>

LEMMA StageAdmitFacts ==
  ASSUME NEW r \in R, StageAdmit(r)
  PROVE  \E s \in R : AdmGuard(r, s) /\ AdmSnap(r, s) /\ AdmRest(r, s)
<1>1. AssemblyQuorum = 1 /\ Policy = "trust" /\ BaseInclusive = TRUE BY Full
<1>2a. PICK s \in (IF Policy = "trust"
                      THEN {q \in AsmSet : LinCovers(q, mark[r])}
                      ELSE CoverSet(r, Max(recorded), mark[r]) \cap AsmSet) :
             LET lineS == Lin(s) \cup {nextSeq}
                 elems == {e \in lineS : (IF BaseInclusive THEN mark[r] ELSE mark[r] + 1) <= e /\ e <= nextSeq}
                 alOf(e) == IF e = nextSeq THEN hAl[s] ELSE sAl[s][e]
                 conOf(e) == IF e = nextSeq THEN head[s] ELSE sCon[s][e]
                 dHead == [x \in Blocks |->
                            LET hits == {e \in elems : x \in alOf(e)}
                            IN IF hits = {} THEN head[r][x] ELSE conOf(Max(hits))[x]]
                 dAl == hAl[r] \cup {x \in Blocks : \E e \in elems : x \in alOf(e)}
             IN /\ sPres' = [q \in R |-> IF q \in AsmSet \cup {r} THEN sPres[q] \cup {nextSeq} ELSE sPres[q]]
                /\ sCon' = [q \in R |-> IF q \in AsmSet THEN [sCon[q] EXCEPT ![nextSeq] = head[q]]
                                        ELSE IF q = r THEN [sCon[q] EXCEPT ![nextSeq] = dHead]
                                        ELSE sCon[q]]
                /\ sAl' = [q \in R |-> IF q \in AsmSet THEN [sAl[q] EXCEPT ![nextSeq] = hAl[q]]
                                       ELSE IF q = r THEN [sAl[q] EXCEPT ![nextSeq] = dAl]
                                       ELSE sAl[q]]
                /\ sPar' = [q \in R |-> IF q \in AsmSet \cup {r} THEN [sPar[q] EXCEPT ![nextSeq] = hPar[q]]
                                        ELSE sPar[q]]
                /\ anc' = [q \in R |-> IF q \in AsmSet \cup {r} THEN [anc[q] EXCEPT ![nextSeq] = NewAnc(q, nextSeq)]
                                       ELSE anc[q]]
                /\ hPar' = [q \in R |-> IF q \in AsmSet \cup {r} THEN nextSeq ELSE hPar[q]]
                /\ hAl' = [q \in R |-> IF q \in AsmSet \cup {r} THEN {} ELSE hAl[q]]
                /\ head' = [head EXCEPT ![r] = dHead]
                /\ badAdmit' = (badAdmit \/ dHead # head[s])
                /\ badLoss' = (badLoss \/ \E x \in Blocks :
                                  \/ head[s][x] >= acked[x] /\ dHead[x] < acked[x]
                                  \/ dHead[x] \notin ackedEver[x] \cup {head[q][x] : q \in AsmSet})
                /\ IF Policy = "trust"
                     THEN /\ tr' = [q \in R |-> IF (q \in AsmSet /\ htr[q]) \/ (q = r /\ htr[s])
                                                THEN tr[q] \cup {nextSeq} ELSE tr[q] \ {nextSeq}]
                          /\ cutAck' = [q \in R |-> IF q \in AsmSet \cup {r} THEN [cutAck[q] EXCEPT ![nextSeq] = acked]
                                                    ELSE cutAck[q]]
                          /\ htr' = [q \in R |-> FALSE]
                     ELSE UNCHANGED <<tr, htr, cutAck>>
  BY DEF StageAdmit
<1>2. s \in {q \in AsmSet : LinCovers(q, mark[r])} /\
        LET lineS == Lin(s) \cup {nextSeq}
            elems == {e \in lineS : mark[r] <= e /\ e <= nextSeq}
            alOf(e) == IF e = nextSeq THEN hAl[s] ELSE sAl[s][e]
            conOf(e) == IF e = nextSeq THEN head[s] ELSE sCon[s][e]
            dHead == [x \in Blocks |->
                       LET hits == {e \in elems : x \in alOf(e)}
                       IN IF hits = {} THEN head[r][x] ELSE conOf(Max(hits))[x]]
            dAl == hAl[r] \cup {x \in Blocks : \E e \in elems : x \in alOf(e)}
        IN /\ sPres' = [q \in R |-> IF q \in AsmSet \cup {r} THEN sPres[q] \cup {nextSeq} ELSE sPres[q]]
           /\ sCon' = [q \in R |-> IF q \in AsmSet THEN [sCon[q] EXCEPT ![nextSeq] = head[q]]
                                   ELSE IF q = r THEN [sCon[q] EXCEPT ![nextSeq] = dHead]
                                   ELSE sCon[q]]
           /\ sAl' = [q \in R |-> IF q \in AsmSet THEN [sAl[q] EXCEPT ![nextSeq] = hAl[q]]
                                  ELSE IF q = r THEN [sAl[q] EXCEPT ![nextSeq] = dAl]
                                  ELSE sAl[q]]
           /\ sPar' = [q \in R |-> IF q \in AsmSet \cup {r} THEN [sPar[q] EXCEPT ![nextSeq] = hPar[q]]
                                   ELSE sPar[q]]
           /\ anc' = [q \in R |-> IF q \in AsmSet \cup {r} THEN [anc[q] EXCEPT ![nextSeq] = NewAnc(q, nextSeq)]
                                  ELSE anc[q]]
           /\ hPar' = [q \in R |-> IF q \in AsmSet \cup {r} THEN nextSeq ELSE hPar[q]]
           /\ hAl' = [q \in R |-> IF q \in AsmSet \cup {r} THEN {} ELSE hAl[q]]
           /\ head' = [head EXCEPT ![r] = dHead]
           /\ badAdmit' = (badAdmit \/ dHead # head[s])
           /\ badLoss' = (badLoss \/ \E x \in Blocks :
                             \/ head[s][x] >= acked[x] /\ dHead[x] < acked[x]
                             \/ dHead[x] \notin ackedEver[x] \cup {head[q][x] : q \in AsmSet})
           /\ tr' = [q \in R |-> IF (q \in AsmSet /\ htr[q]) \/ (q = r /\ htr[s])
                                 THEN tr[q] \cup {nextSeq} ELSE tr[q] \ {nextSeq}]
           /\ cutAck' = [q \in R |-> IF q \in AsmSet \cup {r} THEN [cutAck[q] EXCEPT ![nextSeq] = acked]
                                     ELSE cutAck[q]]
           /\ htr' = [q \in R |-> FALSE]
  <2>1. ZK_AdmBody(r, s, IF BaseInclusive THEN mark[r] ELSE mark[r] + 1) BY <1>2a DEF ZK_AdmBody
  <2>2. (IF BaseInclusive THEN mark[r] ELSE mark[r] + 1) = mark[r] BY <1>1
  <2>3. ZK_AdmBody(r, s, mark[r]) BY <2>1, <2>2
  <2>4. s \in {q \in AsmSet : LinCovers(q, mark[r])} BY <1>1, <1>2a
  <2>. QED BY <1>1, <2>3, <2>4 DEF ZK_AdmBody
<1>3. s \in R BY <1>2, AsmSetSub
<1>4. AdmGuard(r, s) BY <1>1, <1>2, Full DEF StageAdmit, AdmGuard, AdmT, AdmK
<1>5. AdmHead(r, s) = [x \in Blocks |->
                         LET hits == {e \in {e \in Lin(s) \cup {nextSeq} : mark[r] <= e /\ e <= nextSeq} :
                                        x \in (IF e = nextSeq THEN hAl[s] ELSE sAl[s][e])}
                         IN IF hits = {} THEN head[r][x]
                            ELSE (IF Max(hits) = nextSeq THEN head[s] ELSE sCon[s][Max(hits)])[x]]
  BY DEF AdmHead, AdmHits, AdmElems, AdmAl, AdmCon, AdmK
<1>6. AdmAlD(r, s) = hAl[r] \cup {x \in Blocks : \E e \in {e \in Lin(s) \cup {nextSeq} : mark[r] <= e /\ e <= nextSeq} :
                                     x \in (IF e = nextSeq THEN hAl[s] ELSE sAl[s][e])}
  BY DEF AdmAlD, AdmElems, AdmAl, AdmK
<1>7. AdmSnap(r, s) BY <1>2, <1>5, <1>6 DEF AdmSnap, AdmT, AdmK
<1>8. AdmRest(r, s) BY <1>1, <1>2, <1>5, Full DEF StageAdmit, AdmRest, AdmT, AdmM, AdmK, tieVars
<1>. QED BY <1>3, <1>4, <1>7, <1>8
