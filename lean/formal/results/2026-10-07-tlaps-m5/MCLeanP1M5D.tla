----------------------------- MODULE MCLeanP1M5D ------------------------------
(***************************************************************************)
(* M5 part D: AN3's proof.  Candidate STRENGTHENINGS as state invariants   *)
(* for TLC (E37), over MCLeanP1M5C (J5C, part C's).  Which of AN3's names  *)
(* the proof can do without: the live disjunct (E1's conjecture, there for *)
(* Inv_AckedNamed in the gate worlds), the tombstone, the tree's memory,   *)
(* the later ack.                                                          *)
(***************************************************************************)
EXTENDS MCLeanP1M5C
DocAt(q, h)  == doc[q] # Nil /\ Derives(doc[q], h)
TombAt(q, h) == tomb[q] # Nil /\ Derives(tomb[q], h)
TookAt(q, h) == \E s \in Writers : Gen(h) \in took[s][q] /\ w[s].local[q] # h
UdelAt(q, h) == \E d \in udel : d[1] = q /\ Derives(d[2], h)
LaterAt(p, h) == \E b \in acked : b[1] \in Chain(p, h) /\ Later(b[2], h)
\* AN3 without `h \in live`.
A1 == \A a \in acked : LET p == a[1] h == a[2] IN
        \/ Preserved(h) \/ LaterAt(p, h)
        \/ \E q \in Chain(p, h) : DocAt(q, h) \/ TombAt(q, h) \/ TookAt(q, h) \/ UdelAt(q, h)
\* ...and without the tombstone.
A2 == \A a \in acked : LET p == a[1] h == a[2] IN
        \/ Preserved(h) \/ LaterAt(p, h)
        \/ \E q \in Chain(p, h) : DocAt(q, h) \/ TookAt(q, h) \/ UdelAt(q, h)
\* ...without the tree's memory.
A3 == \A a \in acked : LET p == a[1] h == a[2] IN
        \/ Preserved(h) \/ LaterAt(p, h)
        \/ \E q \in Chain(p, h) : DocAt(q, h) \/ TombAt(q, h) \/ UdelAt(q, h)
\* ...without the later ack.
A4 == \A a \in acked : LET p == a[1] h == a[2] IN
        \/ Preserved(h)
        \/ \E q \in Chain(p, h) : DocAt(q, h) \/ TombAt(q, h) \/ TookAt(q, h) \/ UdelAt(q, h)
\* A tombstone is an acknowledged delete, or a rename's source.
TU == \A q \in Paths : tomb[q] # Nil =>
        <<q, tomb[q]>> \in udel \/ \E r \in Paths : r # q /\ <<r, tomb[q]>> \in acked
\* An acked handle derives only from what is minted; the rename ghost is idle.
MV == mv = Nil
\* E38: AN3 FAILS (MCLeanP1M5H5): a copy's acked successor is contested
\* away; the conflict names the successor, not the copy.  AN4: a conflict
\* on the chain read by derivation, as NR5 reads it.
ConfAt(q, h) == \E c \in conflicts : c[1] = q /\ c[2] # Nil /\ Derives(c[2], h)
AN4 == \A a \in acked : AccountedChain(a[1], a[2]) \/ \E q \in Chain(a[1], a[2]) : ConfAt(q, a[2])
\* E39 (MCLeanP1M5H6): AN4 FAILS once the successor is renamed on; what
\* names the copy is its successor's ack.  AN5: AN4, or an acknowledged
\* STRICT descendant anywhere (other bytes; the rank orders it, so two acks
\* never account for each other).
Desc(h) == \E b \in acked : Derives(b[2], h) /\ Content(b[2]) # Content(h)
AN5 == \A a \in acked : AccountedChain(a[1], a[2]) \/ Desc(a[2])
                        \/ \E q \in Chain(a[1], a[2]) : ConfAt(q, a[2])
\* AN5 without `h \in live` (the proof's candidate).
B1 == \A a \in acked : LET p == a[1] h == a[2] IN
        \/ Preserved(h) \/ LaterAt(p, h) \/ Desc(h)
        \/ \E q \in Chain(p, h) : DocAt(q, h) \/ TombAt(q, h) \/ TookAt(q, h) \/ UdelAt(q, h) \/ ConfAt(q, h)
\* The proof's strengthening (E41): an acked version is cited, or an exact
\* record names it (a conflict, an acknowledged delete on its chain), or an
\* acknowledged strict descendant does, or a tree that took it on its chain
\* has moved on.  CHI: a cited version is at its own path or acked there;
\* BT: a tree's memory has its baseline; UD: an upload is dirty.
AckH == {a[2] : a \in acked}
ChainH(h) == {h[1]} \cup {q \in Paths : <<q, h>> \in acked}
KD2 == \A h \in AckH : \/ Cited(h) \/ Preserved(h) \/ Desc(h)
                       \/ \E q \in ChainH(h) : <<q, h>> \in udel \/ TookAt(q, h)
CHI == \A q \in Paths : doc[q] # Nil => doc[q][1] = q \/ <<q, doc[q]>> \in acked
BT  == \A s \in Writers, p \in Paths : w[s].baseline[p] # Nil => Gen(w[s].baseline[p]) \in took[s][p]
UD  == \A s \in Writers : w[s].pc \in Bar => \A p \in w[s].uploads :
         w[s].baseline[p] # Nil => w[s].local[p] # w[s].baseline[p]
J5D == KD2 /\ CHI /\ BT /\ UD
==============================================================================
