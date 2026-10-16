------------------------------ MODULE LeanP1ProofM14 ------------------------------
(* M14 (row 15): A1DeclPriv, and AK1.  Generated: results/2026-10-15-tlaps-
   rows15-17/gen-m14.py (its docstring has the argument).                  *)
EXTENDS LeanP1ProofM13

A4Dirt(t, j) == w[t].local[j] # w[t].baseline[j] /\ w[t].local[j] # Nil /\ j \notin A1LeftSet(w[t])
A4DU == \A u, s \in Writers, q, k \in Paths :
          u # s /\ A4Dirt(u, q) =>
            /\ A4Dirt(s, k) => Content(w[s].local[k]) # Content(w[u].local[q])
            /\ k \in w[s].uploads => Content(w[s].snap[k]) # Content(w[u].local[q])
A4Gw == \A p \in Paths : gw[p] # Nil =>
          /\ orig[gw[p]] = Nil
          /\ \A h \in minted : orig[h] # gw[p]
          /\ \A s \in Writers, q \in Paths : w[s].decl[q] # gw[p]
A4DB == \A s \in Writers, q \in Paths : w[s].pc \in Bar /\ q \in w[s].deletes =>
          w[s].baseline[q] # Nil /\ w[s].local[q] # w[s].baseline[q]
A4Inv == A1DeclPriv /\ A4DU /\ A4Gw /\ A4DB
\* What the steps assume: the proved invariants of the current state, M11's of
\* the next (Next_A1 gives it).
A4Hyp == IndC /\ IndC' /\ DInv /\ A1IAK1 /\ A1IAK1' /\ A2Side /\ A3Side /\ A4Inv

------------------------------------------------------------------------------
(* Facts the steps share.                                                   *)

LEMMA A4Ty ==
  ASSUME IndC, A1IAK1, NEW t \in Writers, NEW j \in Paths
  PROVE  /\ w[t].local[j] \in Opt(minted) /\ w[t].baseline[j] \in Opt(minted) /\ w[t].snap[j] \in Opt(minted)
         /\ w[t].decl[j] \in Opt(minted) /\ doc[j] \in Opt(minted) /\ gw[j] \in Opt(minted)
         /\ w[t] \in Writer
<1>1. w[t].local[j] \in Opt(minted) /\ w[t].baseline[j] \in Opt(minted) /\ w[t].snap[j] \in Opt(minted) /\ w[t] \in Writer
  BY TreeTy DEF IndC
<1>2. w[t].decl[j] \in Opt(minted) BY DEF A1IAK1, A1DeclMinted
<1>3. doc[j] \in Opt(minted) /\ gw[j] \in Opt(minted) BY DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Minted
<1>. QED BY <1>1, <1>2, <1>3

\* The bytes of a minted handle are a minted original; Nil's are Nil.
LEMMA A4CM ==
  ASSUME IndC, NEW h \in Opt(minted)
  PROVE  /\ h # Nil => Content(h) \in minted /\ Content(h) # Nil
         /\ h = Nil => Content(h) = Nil
<1>1. CASE h = Nil BY <1>1 DEF Content
<1>2. CASE h \in minted
  <2>1. Content(h) \in minted BY <1>2, ContentMinted DEF IndC
  <2>2. Nil \notin minted BY NilHandle DEF IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>. QED BY <1>2, <2>1, <2>2
<1>. QED BY <1>1, <1>2 DEF Opt

\* A private declaration is dirt holding the declared bytes (CPub3, CPubL, DTree).
LEMMA A4PrivDirt ==
  ASSUME IndC, A1IAK1, NEW u \in Writers, NEW q \in Paths, A1Priv(u, q)
  PROVE  A4Dirt(u, q) /\ Content(w[u].local[q]) = Content(w[u].decl[q])
<1>0. A1Per(u) BY A1PerAll
<1>1. /\ (A1Decl(u, q) /\ w[u].local[q] # w[u].baseline[q] /\ q \notin A1LeftSet(w[u])) =>
           Content(w[u].local[q]) = Content(w[u].decl[q])
      /\ (A1Decl(u, q) /\ w[u].decl[q] # Nil /\ w[u].local[q] = w[u].baseline[q]) =>
           w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)
      /\ (A1Decl(u, q) /\ w[u].decl[q] # Nil /\ q \in A1LeftSet(w[u])) =>
           w[u].car.id = w[u].pend.id /\ CarForm(u, q, w[u].car)
  BY <1>0 DEF A1Per
<1>2. w[u].local[q] # w[u].baseline[q] /\ q \notin A1LeftSet(w[u]) BY <1>1 DEF A1Priv
<1>3. Content(w[u].local[q]) = Content(w[u].decl[q]) BY <1>1, <1>2 DEF A1Priv
<1>4. Content(w[u].decl[q]) # Nil BY A4Ty, A4CM DEF A1Priv
<1>5. w[u].local[q] # Nil
  <2>. SUFFICES ASSUME w[u].local[q] = Nil PROVE FALSE OBVIOUS
  <2>. QED BY <1>3, <1>4 DEF Content
<1>. QED BY <1>2, <1>3, <1>5 DEF A4Dirt

------------------------------------------------------------------------------
(* One tree s written, the document and the saves kept.  What the step must  *)
(* show for s: no new dirt (or fresh bytes), uploads from its own uploads or *)
(* dirt, no new private declaration, declarations from the tree, copies of  *)
(* no save, and a barrier's delete still dirty.                             *)

A4D(s, R) == \A q \in Paths : A4Dirt(s, q)' =>
               \/ A4Dirt(s, q) /\ (Content(w[s].local[q]))' = Content(w[s].local[q])
               \/ (Content(w[s].local[q]))' \notin minted
A4U(s, R) == \A k \in Paths : k \in R.uploads =>
               \/ k \in w[s].uploads /\ (Content(w[s].snap[k]))' = Content(w[s].snap[k])
               \/ A4Dirt(s, k) /\ (Content(w[s].snap[k]))' = Content(w[s].local[k])
\* A private declaration is an old one, its bytes kept; or (Touch) new dirt whose
\* bytes no document path and no save hold.
A4P(s, R) == \A q \in Paths : A1Priv(s, q)' =>
               \/ A1Priv(s, q) /\ (Content(w[s].decl[q]))' = Content(w[s].decl[q])
               \/ /\ A4Dirt(s, q) /\ (Content(w[s].decl[q]))' = Content(w[s].local[q])
                  /\ \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[s].local[q])
A4G(s, R) == \A q \in Paths : R.decl[q] = w[s].decl[q] \/ R.decl[q] = w[s].local[q] \/ R.decl[q] = Nil
A4O == \A h \in minted' : orig'[h] = orig[h] \/ (h \notin minted /\ \A p \in Paths : gw[p] # Nil => orig'[h] # gw[p])
A4B(s, R) == \A q \in Paths : R.pc \in Bar /\ q \in R.deletes => R.baseline[q] # Nil /\ R.local[q] # R.baseline[q]

LEMMA A4One ==
  ASSUME A4Hyp, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R], doc' = doc, gw' = gw,
         minted \subseteq minted', A3CK, A4O,
         A4D(s, R), A4U(s, R), A4P(s, R), A4G(s, R), A4B(s, R)
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>w. \A u \in Writers : w'[u] = IF u = s THEN R ELSE w[u] BY <1>0 DEF TypeOK
<1>ic. IndC /\ A1IAK1 /\ A4Inv BY DEF A4Hyp
\* contents of the other trees' handles, and the document's, are kept
<1>kd. \A j \in Paths : Content(doc[j])' = Content(doc[j])
  <2>. SUFFICES ASSUME NEW j \in Paths PROVE Content(doc[j])' = Content(doc[j]) OBVIOUS
  <2>1. PICK h4 \in Opt(minted) : h4 = doc[j] BY <1>ic, A4Ty
  <2>. QED BY <2>1 DEF A3CK
<1>k. \A t \in Writers \ {s}, j \in Paths :
        /\ Content(w[t].local[j])' = Content(w[t].local[j]) /\ Content(w[t].snap[j])' = Content(w[t].snap[j])
        /\ Content(w[t].decl[j])' = Content(w[t].decl[j]) /\ Content(doc[j])' = Content(doc[j])
  <2>. SUFFICES ASSUME NEW t \in Writers \ {s}, NEW j \in Paths
                PROVE  /\ Content(w[t].local[j])' = Content(w[t].local[j]) /\ Content(w[t].snap[j])' = Content(w[t].snap[j])
                       /\ Content(w[t].decl[j])' = Content(w[t].decl[j]) /\ Content(doc[j])' = Content(doc[j]) OBVIOUS
  <2>0. w'[t] = w[t] BY <1>w
  <2>1. PICK h1 \in Opt(minted) : h1 = w[t].local[j] BY <1>ic, A4Ty
  <2>2. PICK h2 \in Opt(minted) : h2 = w[t].snap[j] BY <1>ic, A4Ty
  <2>3. PICK h3 \in Opt(minted) : h3 = w[t].decl[j] BY <1>ic, A4Ty
  <2>5. Content(h1)' = Content(h1) BY <2>1 DEF A3CK
  <2>6. Content(h2)' = Content(h2) BY <2>2 DEF A3CK
  <2>7. Content(h3)' = Content(h3) BY <2>3 DEF A3CK
  <2>. QED BY <2>0, <2>1, <2>2, <2>3, <2>5, <2>6, <2>7, <1>kd
\* the other trees' dirt and private declarations are kept
<1>od. \A t \in Writers \ {s}, j \in Paths : (A4Dirt(t, j)' <=> A4Dirt(t, j)) /\ (A1Priv(t, j)' <=> A1Priv(t, j))
  <2>. SUFFICES ASSUME NEW t \in Writers \ {s}, NEW j \in Paths
                PROVE  (A4Dirt(t, j)' <=> A4Dirt(t, j)) /\ (A1Priv(t, j)' <=> A1Priv(t, j)) OBVIOUS
  <2>1. w'[t] = w[t] BY <1>w
  <2>2. PICK c0 : c0 = w[t].car OBVIOUS
  <2>3. PICK hp \in Opt(minted) : hp = w[t].car.pub[j] BY <1>ic DEF A1IAK1, A1CarPubMinted
  <2>4. Content(hp)' = Content(hp) BY <2>3 DEF A3CK
  <2>5. CarForm(t, j, c0)' <=> CarForm(t, j, c0) BY <2>1, <2>2, <2>3, <2>4, <1>k DEF CarForm
  <2>. QED BY <2>1, <2>2, <2>5 DEF A4Dirt, A1Priv, A1Decl
\* DU
<1>du. A4DU'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW s2 \in Writers, NEW q \in Paths, NEW k \in Paths,
                       u # s2, A4Dirt(u, q)'
                PROVE  /\ A4Dirt(s2, k)' => (Content(w[s2].local[k]) # Content(w[u].local[q]))'
                       /\ k \in w'[s2].uploads => (Content(w[s2].snap[k]) # Content(w[u].local[q]))'
    BY DEF A4DU
  \* u's dirt: the old one, its bytes kept; or fresh bytes (s's Edit)
  <2>1. \/ A4Dirt(u, q) /\ (Content(w[u].local[q]))' = Content(w[u].local[q])
        \/ u = s /\ (Content(w[u].local[q]))' \notin minted
    <3>1. CASE u = s BY <3>1, <1>w DEF A4D
    <3>2. CASE u # s BY <3>2, <1>w, <1>od, <1>k, <1>kd
    <3>. QED BY <3>1, <3>2
  \* what s2 holds dirty or uploads: old dirt or uploads' bytes, or fresh ones
  <2>2. A4Dirt(s2, k)' =>
          \/ A4Dirt(s2, k) /\ (Content(w[s2].local[k]))' = Content(w[s2].local[k])
          \/ s2 = s /\ (Content(w[s2].local[k]))' \notin minted
    <3>1. CASE s2 = s BY <3>1, <1>w DEF A4D
    <3>2. CASE s2 # s BY <3>2, <1>w, <1>od, <1>k, <1>kd
    <3>. QED BY <3>1, <3>2
  <2>3. k \in w'[s2].uploads =>
          \/ k \in w[s2].uploads /\ (Content(w[s2].snap[k]))' = Content(w[s2].snap[k])
          \/ A4Dirt(s2, k) /\ (Content(w[s2].snap[k]))' = Content(w[s2].local[k])
    <3>1. CASE s2 = s BY <3>1, <1>w DEF A4U
    <3>2. CASE s2 # s BY <3>2, <1>w, <1>k, <1>kd
    <3>. QED BY <3>1, <3>2
  \* the old bytes are minted handles' (so not fresh), and apart (old DU)
  <2>m. /\ Content(w[u].local[q]) \in minted \cup {Nil}
        /\ A4Dirt(s2, k) => Content(w[s2].local[k]) \in minted
        /\ k \in w[s2].uploads => Content(w[s2].snap[k]) \in minted
    <3>1. Content(w[u].local[q]) \in minted \cup {Nil} BY <1>ic, A4Ty, A4CM
    <3>2. A4Dirt(s2, k) => Content(w[s2].local[k]) \in minted BY <1>ic, A4Ty, A4CM DEF A4Dirt
    <3>3. k \in w[s2].uploads => w[s2].snap[k] \in minted BY TreeTy DEF A4Hyp, IndC
    <3>4. k \in w[s2].uploads => Content(w[s2].snap[k]) \in minted BY <3>3, <1>ic, A4CM DEF Opt
    <3>. QED BY <3>1, <3>2, <3>4
  <2>4. CASE A4Dirt(u, q) /\ (Content(w[u].local[q]))' = Content(w[u].local[q])
    <3>1. Content(w[u].local[q]) # Nil BY <2>4, <1>ic, A4Ty, A4CM DEF A4Dirt
    <3>2. A4Dirt(s2, k) => Content(w[s2].local[k]) # Content(w[u].local[q]) BY <2>4 DEF A4Hyp, A4Inv, A4DU
    <3>3. k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].local[q]) BY <2>4 DEF A4Hyp, A4Inv, A4DU
    <3>. QED BY <2>2, <2>3, <2>4, <2>m, <3>1, <3>2, <3>3
  <2>5. CASE u = s /\ (Content(w[u].local[q]))' \notin minted
    \* fresh bytes: s2 # s, so s2's are old minted ones
    <3>1. s2 # s BY <2>5
    <3>2. A4Dirt(s2, k)' => (Content(w[s2].local[k]))' = Content(w[s2].local[k]) /\ A4Dirt(s2, k) BY <2>2, <3>1
    <3>3. k \in w'[s2].uploads => (Content(w[s2].snap[k]))' = Content(w[s2].snap[k]) /\ k \in w[s2].uploads BY <3>1, <1>w, <1>k
    <3>. QED BY <3>2, <3>3, <2>5, <2>m
  <2>. QED BY <2>1, <2>4, <2>5
\* the saves
<1>gw. A4Gw'
  <2>. SUFFICES ASSUME NEW p \in Paths, gw'[p] # Nil
                PROVE  /\ orig'[gw'[p]] = Nil
                       /\ \A h \in minted' : orig'[h] # gw'[p]
                       /\ \A t \in Writers, q \in Paths : w'[t].decl[q] # gw'[p]
    BY DEF A4Gw
  <2>0. /\ orig[gw[p]] = Nil /\ \A h \in minted : orig[h] # gw[p]
        /\ \A t \in Writers, q \in Paths : w[t].decl[q] # gw[p]
    BY DEF A4Hyp, A4Inv, A4Gw
  <2>m. gw[p] \in minted BY <1>ic, A4Ty DEF Opt
  <2>1. orig'[gw'[p]] = Nil BY <2>0, <2>m DEF A4O
  <2>hn. \A h \in Handles \ minted : orig[h] = Nil BY DEF A4Hyp, IndC, IndM5, M5, Hist, HistNew
  <2>mh. minted' \subseteq Handles BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
  <2>2. \A h \in minted' : orig'[h] # gw'[p]
    <3>. SUFFICES ASSUME NEW h \in minted' PROVE orig'[h] # gw[p] OBVIOUS
    <3>1. orig'[h] = orig[h] \/ (h \notin minted /\ \A q \in Paths : gw[q] # Nil => orig'[h] # gw[q]) BY DEF A4O
    <3>2. CASE h \in minted BY <3>2, <3>1, <2>0
    <3>3. CASE h \notin minted BY <3>3, <3>1, <2>hn, <2>mh
    <3>. QED BY <3>2, <3>3
  \* a declaration is the old one or the tree's file; no file is a save (Flight)
  <2>f. \A t \in Writers, q \in Paths : w[t].local[q] # gw[p]
    BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, M2, Flight
  <2>3. \A t \in Writers, q \in Paths : w'[t].decl[q] # gw'[p] BY <2>0, <2>f, <1>w DEF A4G
  <2>. QED BY <2>1, <2>2, <2>3
\* the declared bytes are private
<1>dp. A1DeclPriv'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths, A1Priv(u, q)'
                PROVE  /\ (\A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q]))'
                       /\ (\A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q]))'
                       /\ (\A s2 \in Writers \ {u}, k \in Paths :
                             k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q]))'
    BY DEF A1DeclPriv
  <2>g. \A k \in Paths : gw[k] # Nil => Content(gw[k])' = Content(gw[k])
    <3>. SUFFICES ASSUME NEW k \in Paths, gw[k] # Nil PROVE Content(gw[k])' = Content(gw[k]) OBVIOUS
    <3>1. PICK hg \in Opt(minted) : hg = gw[k] BY <1>ic, A4Ty
    <3>. QED BY <3>1 DEF A3CK
  \* (b) for every declaration: a save is an original no handle copies and no
  \* declaration names (A4Gw')
  <2>b. (\A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q]))'
    <3>. SUFFICES ASSUME NEW k \in Paths, gw'[k] # Nil
                  PROVE  Content(gw[k])' # Content(w[u].decl[q])' OBVIOUS
    <3>1. /\ orig'[gw'[k]] = Nil /\ \A h \in minted' : orig'[h] # gw'[k]
          /\ \A t \in Writers, j \in Paths : w'[t].decl[j] # gw'[k]
      BY <1>gw DEF A4Gw
    <3>2. w'[u].decl[q] \in Opt(minted') BY DEF A4Hyp, A1IAK1, A1DeclMinted
    <3>3. gw'[k] \in minted' BY <1>ic, A4Ty DEF Opt
    <3>4. Content(gw[k])' = gw'[k] BY <3>1, <3>3 DEF Content
    <3>5. CASE w'[u].decl[q] = Nil BY <3>5, <3>4, <3>3, NilHandle DEF Content, A1Priv
    <3>6. CASE w'[u].decl[q] # Nil
      <4>1. w'[u].decl[q] \in minted' BY <3>2, <3>6 DEF Opt
      <4>. QED BY <3>1, <3>4, <3>6, <4>1 DEF Content
    <3>. QED BY <3>5, <3>6
  <2>1. CASE A1Priv(u, q) /\ (Content(w[u].decl[q]))' = Content(w[u].decl[q])
    <3>o. /\ \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q])
          /\ \A s2 \in Writers \ {u}, k \in Paths : k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q])
      BY <2>1 DEF A4Hyp, A4Inv, A1DeclPriv
    <3>a. (\A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q]))' BY <2>1, <3>o, <1>k, <1>kd
    \* another tree's uploads: s's are its old uploads (old (c)) or its old dirt
    \* (A4DU against u's private dirt, A4PrivDirt)
    <3>c. (\A s2 \in Writers \ {u}, k \in Paths : k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q]))'
      <4>. SUFFICES ASSUME NEW s2 \in Writers \ {u}, NEW k \in Paths, k \in w'[s2].uploads
                    PROVE  (Content(w[s2].snap[k]) # Content(w[u].decl[q]))' OBVIOUS
      <4>p. A4Dirt(u, q) /\ Content(w[u].local[q]) = Content(w[u].decl[q]) BY <2>1, <1>ic, A4PrivDirt
      <4>1. CASE s2 = s
        <5>1. \/ k \in w[s].uploads /\ (Content(w[s].snap[k]))' = Content(w[s].snap[k])
              \/ A4Dirt(s, k) /\ (Content(w[s].snap[k]))' = Content(w[s].local[k])
          BY <4>1, <1>w DEF A4U
        <5>2. k \in w[s].uploads => Content(w[s].snap[k]) # Content(w[u].decl[q]) BY <4>1, <3>o
        <5>3. A4Dirt(s, k) => Content(w[s].local[k]) # Content(w[u].local[q]) BY <4>1, <4>p DEF A4Hyp, A4Inv, A4DU
        <5>. QED BY <4>1, <5>1, <5>2, <5>3, <4>p, <2>1, <1>w
      <4>2. CASE s2 # s
        <5>1. w'[s2] = w[s2] BY <4>2, <1>w
        <5>2. Content(w[s2].snap[k])' = Content(w[s2].snap[k]) BY <4>2, <1>k
        <5>3. k \in w[s2].uploads BY <5>1
        <5>4. Content(w[s2].snap[k]) # Content(w[u].decl[q]) BY <5>3, <3>o
        <5>. QED BY <5>2, <5>4, <2>1
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <3>a, <2>b, <3>c
  \* a new private declaration (s's touch): the tree's dirt, bytes no document has
  <2>2. CASE ~(A1Priv(u, q) /\ (Content(w[u].decl[q]))' = Content(w[u].decl[q]))
    <3>0. u = s
      <4>. SUFFICES ASSUME u # s PROVE FALSE OBVIOUS
      <4>. QED BY <2>2, <1>w, <1>od, <1>k, <1>kd
    <3>1. /\ A4Dirt(s, q) /\ (Content(w[s].decl[q]))' = Content(w[s].local[q])
          /\ \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[s].local[q])
      BY <2>2, <3>0, <1>w DEF A4P
    <3>a. (\A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q]))' BY <3>0, <3>1, <1>w, <1>k, <1>kd
    <3>c. (\A s2 \in Writers \ {u}, k \in Paths : k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q]))'
      <4>. SUFFICES ASSUME NEW s2 \in Writers \ {u}, NEW k \in Paths, k \in w'[s2].uploads
                    PROVE  (Content(w[s2].snap[k]) # Content(w[u].decl[q]))' OBVIOUS
      <4>1. w'[s2] = w[s2] /\ s2 # s BY <3>0, <1>w
      <4>3. Content(w[s2].snap[k]) # Content(w[s].local[q]) BY <4>1, <3>1 DEF A4Hyp, A4Inv, A4DU
      <4>. QED BY <4>1, <4>3, <3>0, <3>1, <1>w, <1>k, <1>kd
    <3>. QED BY <3>a, <2>b, <3>c
  <2>. QED BY <2>1, <2>2
\* a barrier's delete stays dirty
<1>db. A4DB' BY <1>w DEF A4Hyp, A4Inv, A4DB, A4B
<1>. QED BY <1>du, <1>dp, <1>gw, <1>db DEF A4Inv

------------------------------------------------------------------------------
(* The generic shapes: a step that keeps the tree's dirt, its declaration and *)
(* its uploads; and the per-path shape of a step that only shrinks dirt.     *)

\* The fields dirt reads, and those a private declaration reads.
A4Tree(R, W) == R.local = W.local /\ R.baseline = W.baseline /\ A1LeftSet(R) = A1LeftSet(W)
A4Dcl(R, W) == /\ R.decl = W.decl /\ R.dset = W.dset /\ W.moved \subseteq R.moved /\ R.pend = W.pend
               /\ R.car.id = W.car.id /\ R.car.paths = W.car.paths /\ R.car.deletes = W.car.deletes
               /\ R.car.dropped = W.car.dropped /\ R.car.pub = W.car.pub
A4Upl(R, W) == R.uploads = {} \/ (R.uploads = W.uploads /\ R.snap = W.snap)
A4Brr(R, W) == R.pc \in Bar => W.pc \in Bar /\ R.deletes = W.deletes

\* Priv reads the declaration and the carrier's CarForm fields.
LEMMA A4PrivSame ==
  ASSUME IndC, A1IAK1, NEW s \in Writers, NEW R, NEW q \in Paths, w'[s] = R, A4Dcl(R, w[s]), A3CK
  PROVE  A1Priv(s, q)' => A1Priv(s, q)
<1>1. PICK c0 : c0 = w[s].car OBVIOUS
<1>2. PICK hd \in Opt(minted) : hd = w[s].decl[q] BY A4Ty
<1>3. PICK hp \in Opt(minted) : hp = w[s].car.pub[q] BY DEF A1IAK1, A1CarPubMinted
<1>4. Content(hd)' = Content(hd) /\ Content(hp)' = Content(hp) BY <1>2, <1>3 DEF A3CK
<1>5. CarForm(s, q, R.car)' <=> CarForm(s, q, c0) BY <1>1, <1>2, <1>3, <1>4 DEF CarForm, A4Dcl
<1>. QED BY <1>1, <1>5 DEF A1Priv, A1Decl, A4Dcl

\* The pieces of a kept step, one lemma each.
LEMMA A4TreeD ==
  ASSUME IndC, A1IAK1, NEW s \in Writers, NEW R, w'[s] = R, A3CK, A4Tree(R, w[s])
  PROVE  A4D(s, R)
<1>. SUFFICES ASSUME NEW q \in Paths, A4Dirt(s, q)'
              PROVE  A4Dirt(s, q) /\ (Content(w[s].local[q]))' = Content(w[s].local[q]) BY DEF A4D
<1>1. A4Dirt(s, q) BY DEF A4Dirt, A4Tree
<1>2. PICK hl \in Opt(minted) : hl = w[s].local[q] BY A4Ty
<1>. QED BY <1>1, <1>2 DEF A3CK, A4Tree

LEMMA A4UplU ==
  ASSUME IndC, A1IAK1, NEW s \in Writers, NEW R, w'[s] = R, A3CK, A4Upl(R, w[s])
  PROVE  A4U(s, R)
<1>. SUFFICES ASSUME NEW k \in Paths, k \in R.uploads
              PROVE  k \in w[s].uploads /\ (Content(w[s].snap[k]))' = Content(w[s].snap[k]) BY DEF A4U
<1>1. R.uploads = w[s].uploads /\ R.snap = w[s].snap BY DEF A4Upl
<1>2. PICK hs \in Opt(minted) : hs = w[s].snap[k] BY A4Ty
<1>. QED BY <1>1, <1>2 DEF A3CK

LEMMA A4DclPG ==
  ASSUME IndC, A1IAK1, NEW s \in Writers, NEW R, w'[s] = R, A3CK, A4Dcl(R, w[s])
  PROVE  A4P(s, R) /\ A4G(s, R)
<1>1. A4P(s, R)
  <2>. SUFFICES ASSUME NEW q \in Paths, A1Priv(s, q)'
                PROVE  A1Priv(s, q) /\ (Content(w[s].decl[q]))' = Content(w[s].decl[q]) BY DEF A4P
  <2>1. A1Priv(s, q) BY A4PrivSame
  <2>2. PICK hd \in Opt(minted) : hd = w[s].decl[q] BY A4Ty
  <2>. QED BY <2>1, <2>2 DEF A3CK, A4Dcl
<1>2. A4G(s, R) BY DEF A4G, A4Dcl
<1>. QED BY <1>1, <1>2

LEMMA A4BrrB ==
  ASSUME A4DB, NEW s \in Writers, NEW R, R.local = w[s].local, R.baseline = w[s].baseline, A4Brr(R, w[s])
  PROVE  A4B(s, R)
BY DEF A4B, A4Brr, A4DB

\* One tree written, orig kept: A4One's A3CK and A4O.
LEMMA A4OneEq ==
  ASSUME A4Hyp, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R], doc' = doc, gw' = gw,
         minted \subseteq minted', orig' = orig,
         A4D(s, R), A4U(s, R), A4P(s, R), A4G(s, R), A4B(s, R)
  PROVE  A4Inv'
<1>1. A3CK BY A3CF
<1>2. A4O BY DEF A4O
<1>. QED BY <1>1, <1>2, A4One

LEMMA A4Keep ==
  ASSUME A4Hyp, NEW s \in Writers, NEW R, w' = [w EXCEPT ![s] = R], doc' = doc, gw' = gw,
         minted \subseteq minted', orig' = orig,
         A4Tree(R, w[s]), A4Dcl(R, w[s]), A4Upl(R, w[s]), A4Brr(R, w[s])
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>s. w'[s] = R BY <1>0 DEF TypeOK
<1>ck. A3CK BY A3CF
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>d. A4D(s, R) BY <1>s, <1>ck, <1>ic, A4TreeD
<1>u. A4U(s, R) BY <1>ck, <1>ic, <1>s, A4UplU
<1>p. A4P(s, R) /\ A4G(s, R) BY <1>s, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, R) BY <1>ic, A4BrrB DEF A4Tree
<1>. QED BY <1>d, <1>u, <1>p, <1>b, A4OneEq

\* Dirt only shrinks at q: clean, no file, the narrow's leftover, or the old dirt.
A4Shr(R, W, q) == \/ R.local[q] = R.baseline[q] \/ R.local[q] = Nil \/ q \in A1LeftSet(R)
                  \/ R.local[q] = W.local[q] /\ W.local[q] # W.baseline[q] /\ q \notin A1LeftSet(W)
LEMMA A4ShrD ==
  ASSUME IndC, A1IAK1, NEW s \in Writers, NEW R, w'[s] = R, A3CK, \A q \in Paths : A4Shr(R, w[s], q)
  PROVE  A4D(s, R)
<1>. SUFFICES ASSUME NEW q \in Paths, A4Dirt(s, q)'
              PROVE  A4Dirt(s, q) /\ (Content(w[s].local[q]))' = Content(w[s].local[q]) BY DEF A4D
<1>1. R.local[q] = w[s].local[q] /\ w[s].local[q] # w[s].baseline[q] /\ q \notin A1LeftSet(w[s])
  BY DEF A4Shr, A4Dirt
<1>2. PICK hl \in Opt(minted) : hl = w[s].local[q] BY A4Ty
<1>. QED BY <1>1, <1>2 DEF A3CK, A4Dirt

------------------------------------------------------------------------------
(* Writer steps that keep the tree, the declaration and the uploads.        *)

LEMMA Claim_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Claim(s), Frame
  PROVE  A4Inv'
<1>w. w' = [w EXCEPT ![s] = ClaimW(s)] BY DEF Claim
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Claim, bucket, aux
<1>f. /\ ClaimW(s).local = w[s].local /\ ClaimW(s).baseline = w[s].baseline
      /\ ClaimW(s).sStage = w[s].sStage /\ ClaimW(s).sDrop = w[s].sDrop /\ ClaimW(s).sKeep = w[s].sKeep /\ ClaimW(s).sHeld = w[s].sHeld
  BY DEF ClaimW
<1>g. /\ ClaimW(s).decl = w[s].decl /\ ClaimW(s).dset = w[s].dset /\ ClaimW(s).moved = w[s].moved /\ ClaimW(s).pend = w[s].pend
  BY DEF ClaimW
<1>c. /\ ClaimW(s).car.id = w[s].car.id /\ ClaimW(s).car.paths = w[s].car.paths /\ ClaimW(s).car.deletes = w[s].car.deletes
      /\ ClaimW(s).car.dropped = w[s].car.dropped /\ ClaimW(s).car.pub = w[s].car.pub
  BY DEF ClaimW, CarUncited
<1>t. A4Tree(ClaimW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4Dcl(ClaimW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(ClaimW(s), w[s]) BY DEF Claim, ClaimW, A4Upl
<1>3. A4Brr(ClaimW(s), w[s]) BY DEF Claim, ClaimW, A4Brr, Bar
<1>. QED BY <1>w, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

LEMMA Verify_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Verify(s), Frame
  PROVE  A4Inv'
<1>w. w' = [w EXCEPT ![s] = VerifyW(s)] BY DEF Verify
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Verify, bucket, aux
<1>f. /\ VerifyW(s).local = w[s].local /\ VerifyW(s).baseline = w[s].baseline
      /\ VerifyW(s).sStage = w[s].sStage /\ VerifyW(s).sDrop = w[s].sDrop /\ VerifyW(s).sKeep = w[s].sKeep /\ VerifyW(s).sHeld = w[s].sHeld
  BY DEF VerifyW
<1>g. /\ VerifyW(s).decl = w[s].decl /\ VerifyW(s).dset = w[s].dset /\ VerifyW(s).moved = w[s].moved /\ VerifyW(s).pend = w[s].pend
  BY DEF VerifyW
<1>c. /\ VerifyW(s).car.id = w[s].car.id /\ VerifyW(s).car.paths = w[s].car.paths /\ VerifyW(s).car.deletes = w[s].car.deletes
      /\ VerifyW(s).car.dropped = w[s].car.dropped /\ VerifyW(s).car.pub = w[s].car.pub
  BY DEF VerifyW, CarUncited
<1>t. A4Tree(VerifyW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4Dcl(VerifyW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(VerifyW(s), w[s]) BY DEF Verify, VerifyW, A4Upl
<1>3. A4Brr(VerifyW(s), w[s]) BY DEF Verify, VerifyW, A4Brr, Bar
<1>. QED BY <1>w, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

LEMMA RPullRead_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, RPullRead(s), Frame
  PROVE  A4Inv'
<1>w. w' = [w EXCEPT ![s] = RPullReadW(s)] BY DEF RPullRead
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF RPullRead, bucket, aux
<1>f. /\ RPullReadW(s).local = w[s].local /\ RPullReadW(s).baseline = w[s].baseline
      /\ RPullReadW(s).sStage = w[s].sStage /\ RPullReadW(s).sDrop = w[s].sDrop /\ RPullReadW(s).sKeep = w[s].sKeep /\ RPullReadW(s).sHeld = w[s].sHeld
  BY DEF RPullReadW
<1>g. /\ RPullReadW(s).decl = w[s].decl /\ RPullReadW(s).dset = w[s].dset /\ RPullReadW(s).moved = w[s].moved /\ RPullReadW(s).pend = w[s].pend
  BY DEF RPullReadW
<1>c. /\ RPullReadW(s).car.id = w[s].car.id /\ RPullReadW(s).car.paths = w[s].car.paths /\ RPullReadW(s).car.deletes = w[s].car.deletes
      /\ RPullReadW(s).car.dropped = w[s].car.dropped /\ RPullReadW(s).car.pub = w[s].car.pub
  BY DEF RPullReadW, CarUncited
<1>t. A4Tree(RPullReadW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4Dcl(RPullReadW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(RPullReadW(s), w[s]) BY DEF RPullRead, RPullReadW, A4Upl
<1>3. A4Brr(RPullReadW(s), w[s]) BY DEF RPullRead, RPullReadW, A4Brr, Bar
<1>. QED BY <1>w, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

LEMMA Honor_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Honor(s), Frame
  PROVE  A4Inv'
<1>w. w' = [w EXCEPT ![s] = HonorW(s)] BY DEF Honor
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Honor, bucket, aux
<1>f. /\ HonorW(s).local = w[s].local /\ HonorW(s).baseline = w[s].baseline
      /\ HonorW(s).sStage = w[s].sStage /\ HonorW(s).sDrop = w[s].sDrop /\ HonorW(s).sKeep = w[s].sKeep /\ HonorW(s).sHeld = w[s].sHeld
  BY DEF HonorW
<1>g. /\ HonorW(s).decl = w[s].decl /\ HonorW(s).dset = w[s].dset /\ HonorW(s).moved = w[s].moved /\ HonorW(s).pend = w[s].pend
  BY DEF HonorW
<1>c. /\ HonorW(s).car.id = w[s].car.id /\ HonorW(s).car.paths = w[s].car.paths /\ HonorW(s).car.deletes = w[s].car.deletes
      /\ HonorW(s).car.dropped = w[s].car.dropped /\ HonorW(s).car.pub = w[s].car.pub
  BY DEF HonorW, CarUncited
<1>t. A4Tree(HonorW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4Dcl(HonorW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(HonorW(s), w[s]) BY DEF Honor, HonorW, A4Upl
<1>3a. HonorW(s).pc = w[s].pc /\ HonorW(s).deletes = w[s].deletes BY DEF HonorW
<1>3. A4Brr(HonorW(s), w[s]) BY <1>3a DEF A4Brr
<1>. QED BY <1>w, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

LEMMA HonorReader_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, HonorReader(s), Frame
  PROVE  A4Inv'
<1>w. w' = [w EXCEPT ![s] = HonorReaderW(s)] BY DEF HonorReader
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF HonorReader, bucket, aux
<1>f. /\ HonorReaderW(s).local = w[s].local /\ HonorReaderW(s).baseline = w[s].baseline
      /\ HonorReaderW(s).sStage = w[s].sStage /\ HonorReaderW(s).sDrop = w[s].sDrop /\ HonorReaderW(s).sKeep = w[s].sKeep /\ HonorReaderW(s).sHeld = w[s].sHeld
  BY DEF HonorReaderW
<1>g. /\ HonorReaderW(s).decl = w[s].decl /\ HonorReaderW(s).dset = w[s].dset /\ HonorReaderW(s).moved = w[s].moved /\ HonorReaderW(s).pend = w[s].pend
  BY DEF HonorReaderW
<1>c. /\ HonorReaderW(s).car.id = w[s].car.id /\ HonorReaderW(s).car.paths = w[s].car.paths /\ HonorReaderW(s).car.deletes = w[s].car.deletes
      /\ HonorReaderW(s).car.dropped = w[s].car.dropped /\ HonorReaderW(s).car.pub = w[s].car.pub
  BY DEF HonorReaderW, CarUncited
<1>t. A4Tree(HonorReaderW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4Dcl(HonorReaderW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(HonorReaderW(s), w[s]) BY DEF HonorReader, HonorReaderW, A4Upl
<1>3a. HonorReaderW(s).pc = w[s].pc /\ HonorReaderW(s).deletes = w[s].deletes BY DEF HonorReaderW
<1>3. A4Brr(HonorReaderW(s), w[s]) BY <1>3a DEF A4Brr
<1>. QED BY <1>w, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

LEMMA Collect_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Collect(s), Frame
  PROVE  A4Inv'
<1>r. RetireAge BY ShippedShape DEF Shipped
<1>w. w' = [w EXCEPT ![s] = CollectW(s)] BY <1>r DEF Collect
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY <1>r DEF Collect, aux
<1>f. /\ CollectW(s).local = w[s].local /\ CollectW(s).baseline = w[s].baseline
      /\ CollectW(s).sStage = w[s].sStage /\ CollectW(s).sDrop = w[s].sDrop /\ CollectW(s).sKeep = w[s].sKeep /\ CollectW(s).sHeld = w[s].sHeld
  BY <1>r DEF CollectW
<1>g. /\ CollectW(s).decl = w[s].decl /\ CollectW(s).dset = w[s].dset /\ CollectW(s).moved = w[s].moved /\ CollectW(s).pend = w[s].pend
  BY <1>r DEF CollectW
<1>c. /\ CollectW(s).car.id = w[s].car.id /\ CollectW(s).car.paths = w[s].car.paths /\ CollectW(s).car.deletes = w[s].car.deletes
      /\ CollectW(s).car.dropped = w[s].car.dropped /\ CollectW(s).car.pub = w[s].car.pub
  BY <1>r DEF CollectW, CarUncited
<1>t. A4Tree(CollectW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4Dcl(CollectW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(CollectW(s), w[s]) BY <1>r DEF Collect, CollectW, A4Upl
<1>3. A4Brr(CollectW(s), w[s]) BY <1>r DEF Collect, CollectW, A4Brr, Bar
<1>. QED BY <1>w, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

LEMMA Skip_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Skip(s), Frame
  PROVE  A4Inv'
<1>w. w' = [w EXCEPT ![s] = SkipW(s)] BY DEF Skip
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Skip, bucket, aux
<1>f. /\ SkipW(s).local = w[s].local /\ SkipW(s).baseline = w[s].baseline
      /\ SkipW(s).sStage = w[s].sStage /\ SkipW(s).sDrop = w[s].sDrop /\ SkipW(s).sKeep = w[s].sKeep /\ SkipW(s).sHeld = w[s].sHeld
  BY DEF SkipW
<1>g. /\ SkipW(s).decl = w[s].decl /\ SkipW(s).dset = w[s].dset /\ SkipW(s).moved = w[s].moved /\ SkipW(s).pend = w[s].pend
  BY DEF SkipW
<1>c. /\ SkipW(s).car.id = w[s].car.id /\ SkipW(s).car.paths = w[s].car.paths /\ SkipW(s).car.deletes = w[s].car.deletes
      /\ SkipW(s).car.dropped = w[s].car.dropped /\ SkipW(s).car.pub = w[s].car.pub
  BY DEF SkipW, CarUncited
<1>t. A4Tree(SkipW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4Dcl(SkipW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(SkipW(s), w[s]) BY DEF Skip, SkipW, A4Upl
<1>3a. SkipW(s).pc = "idle" BY DEF SkipW
<1>3. A4Brr(SkipW(s), w[s]) BY <1>3a DEF A4Brr, Bar
<1>. QED BY <1>w, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

LEMMA PullOnly_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, PullOnly(s), Frame
  PROVE  A4Inv'
<1>w. w' = [w EXCEPT ![s] = PullOnlyW(s)] BY DEF PullOnly
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF PullOnly, bucket, aux
<1>f. /\ PullOnlyW(s).local = w[s].local /\ PullOnlyW(s).baseline = w[s].baseline
      /\ PullOnlyW(s).sStage = w[s].sStage /\ PullOnlyW(s).sDrop = w[s].sDrop /\ PullOnlyW(s).sKeep = w[s].sKeep /\ PullOnlyW(s).sHeld = w[s].sHeld
  BY DEF PullOnlyW
<1>g. /\ PullOnlyW(s).decl = w[s].decl /\ PullOnlyW(s).dset = w[s].dset /\ PullOnlyW(s).moved = w[s].moved /\ PullOnlyW(s).pend = w[s].pend
  BY DEF PullOnlyW
<1>c. /\ PullOnlyW(s).car.id = w[s].car.id /\ PullOnlyW(s).car.paths = w[s].car.paths /\ PullOnlyW(s).car.deletes = w[s].car.deletes
      /\ PullOnlyW(s).car.dropped = w[s].car.dropped /\ PullOnlyW(s).car.pub = w[s].car.pub
  BY DEF PullOnlyW, CarUncited
<1>t. A4Tree(PullOnlyW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4Dcl(PullOnlyW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(PullOnlyW(s), w[s]) BY DEF PullOnly, PullOnlyW, A4Upl
<1>3. A4Brr(PullOnlyW(s), w[s]) BY DEF PullOnly, PullOnlyW, A4Brr, Bar
<1>. QED BY <1>w, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

------------------------------------------------------------------------------
(* Fence, Restart, RescopeBegin, RetireByAck: the tree's dirt is kept.      *)

LEMMA Fence_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Fence(s), Frame
  PROVE  A4Inv'
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Fence, bucket, aux
<1>1. CASE w[s].pc = "claimed"
  <2>w. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
<2>f. /\ FenceBeforeW(s).local = w[s].local /\ FenceBeforeW(s).baseline = w[s].baseline
      /\ FenceBeforeW(s).sStage = w[s].sStage /\ FenceBeforeW(s).sDrop = w[s].sDrop /\ FenceBeforeW(s).sKeep = w[s].sKeep /\ FenceBeforeW(s).sHeld = w[s].sHeld
  BY DEF FenceBeforeW
<2>g. /\ FenceBeforeW(s).decl = w[s].decl /\ FenceBeforeW(s).dset = w[s].dset /\ FenceBeforeW(s).moved = w[s].moved /\ FenceBeforeW(s).pend = w[s].pend
  BY DEF FenceBeforeW
<2>c. /\ FenceBeforeW(s).car.id = w[s].car.id /\ FenceBeforeW(s).car.paths = w[s].car.paths /\ FenceBeforeW(s).car.deletes = w[s].car.deletes
      /\ FenceBeforeW(s).car.dropped = w[s].car.dropped /\ FenceBeforeW(s).car.pub = w[s].car.pub
  BY DEF FenceBeforeW, CarUncited
<2>t. A4Tree(FenceBeforeW(s), w[s]) BY <2>f DEF A4Tree, A1LeftSet
<2>d. A4Dcl(FenceBeforeW(s), w[s]) BY <2>g, <2>c DEF A4Dcl
  <2>2. A4Upl(FenceBeforeW(s), w[s]) BY DEF FenceBeforeW, A4Upl
  <2>3. A4Brr(FenceBeforeW(s), w[s]) BY DEF FenceBeforeW, A4Brr, Bar
  <2>. QED BY <2>w, <1>u, <2>t, <2>d, <2>2, <2>3, A4Keep
<1>2. CASE w[s].pc = "cased"
  <2>w. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
<2>f. /\ FenceAfterW(s).local = w[s].local /\ FenceAfterW(s).baseline = w[s].baseline
      /\ FenceAfterW(s).sStage = w[s].sStage /\ FenceAfterW(s).sDrop = w[s].sDrop /\ FenceAfterW(s).sKeep = w[s].sKeep /\ FenceAfterW(s).sHeld = w[s].sHeld
  BY DEF FenceAfterW
<2>g. /\ FenceAfterW(s).decl = w[s].decl /\ FenceAfterW(s).dset = w[s].dset /\ FenceAfterW(s).moved = w[s].moved /\ FenceAfterW(s).pend = w[s].pend
  BY DEF FenceAfterW
<2>c. /\ FenceAfterW(s).car.id = w[s].car.id /\ FenceAfterW(s).car.paths = w[s].car.paths /\ FenceAfterW(s).car.deletes = w[s].car.deletes
      /\ FenceAfterW(s).car.dropped = w[s].car.dropped /\ FenceAfterW(s).car.pub = w[s].car.pub
  BY DEF FenceAfterW, CarUncited
<2>t. A4Tree(FenceAfterW(s), w[s]) BY <2>f DEF A4Tree, A1LeftSet
<2>d. A4Dcl(FenceAfterW(s), w[s]) BY <2>g, <2>c DEF A4Dcl
  <2>2. A4Upl(FenceAfterW(s), w[s]) BY DEF FenceAfterW, A4Upl
  <2>3. A4Brr(FenceAfterW(s), w[s]) BY <1>2 DEF FenceAfterW, A4Brr, Bar
  <2>. QED BY <2>w, <1>u, <2>t, <2>d, <2>2, <2>3, A4Keep
<1>. QED BY <1>1, <1>2 DEF Fence

LEMMA Restart_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Restart(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>w. w' = [w EXCEPT ![s] = RestartW(s)] BY DEF Restart
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Restart
<1>t. A4Tree(RestartW(s), w[s])
  <2>0. w[s].sDrop \subseteq Paths /\ w[s].sStage \in {"none", "saved", "mid"} BY <1>0, WriterFields DEF TypeOK
  <2>1. /\ RestartW(s).sDrop = w[s].sDrop /\ RestartW(s).sHeld = w[s].sHeld /\ RestartW(s).local = w[s].local
        /\ RestartW(s).baseline = w[s].baseline /\ RestartW(s).sKeep = {}
        /\ RestartW(s).sStage = IF w[s].sStage = "none" THEN "none" ELSE "saved" BY DEF RestartW
  <2>2. w[s].sStage = "mid" => \A q \in w[s].sDrop \ w[s].sKeep : w[s].baseline[q] = Nil
    BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R4
  <2>3. \A q \in Paths : w[s].sStage = "mid" /\ q \in w[s].sKeep => w[s].baseline[q] # Nil
    BY DEF A4Hyp, A3Side, A1KeepCited
  <2>4. A1LeftSet(RestartW(s)) = A1LeftSet(w[s]) BY <2>0, <2>1, <2>2, <2>3 DEF A1LeftSet
  <2>. QED BY <2>1, <2>4 DEF A4Tree
<1>g. /\ RestartW(s).decl = w[s].decl /\ RestartW(s).dset = w[s].dset /\ RestartW(s).moved = w[s].moved /\ RestartW(s).pend = w[s].pend
  BY DEF RestartW
<1>c. /\ RestartW(s).car.id = w[s].car.id /\ RestartW(s).car.paths = w[s].car.paths /\ RestartW(s).car.deletes = w[s].car.deletes
      /\ RestartW(s).car.dropped = w[s].car.dropped /\ RestartW(s).car.pub = w[s].car.pub
  BY DEF RestartW, CarUncited
<1>d. A4Dcl(RestartW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(RestartW(s), w[s]) BY DEF RestartW, A4Upl
<1>3. A4Brr(RestartW(s), w[s]) BY DEF RestartW, A4Brr, Bar
<1>. QED BY <1>w, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

LEMMA RescopeBegin_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, RescopeBegin(s), Frame
  PROVE  A4Inv'
<1>T. PICK T \in Scopes \ {w[s].scope} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] BY DEF RescopeBegin
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF RescopeBegin, bucket
<1>t. A4Tree(RescopeBeginW(s, T), w[s])
  <2>a. w[s].sStage = "none" BY DEF RescopeBegin
  <2>b. A1LeftSet(w[s]) = {} BY <2>a DEF A1LeftSet
  <2>c. RescopeBeginW(s, T).sHeld = [p \in Paths |-> Nil] BY DEF RescopeBeginW
  <2>e2. RescopeBeginW(s, T).sDrop \subseteq Paths BY DEF RescopeBeginW, Leaving
  <2>d. A1LeftSet(RescopeBeginW(s, T)) = {} BY <2>c, <2>e2 DEF A1LeftSet
  <2>e. RescopeBeginW(s, T).local = w[s].local /\ RescopeBeginW(s, T).baseline = w[s].baseline BY DEF RescopeBeginW
  <2>. QED BY <2>b, <2>d, <2>e DEF A4Tree
<1>g. /\ RescopeBeginW(s, T).decl = w[s].decl /\ RescopeBeginW(s, T).dset = w[s].dset /\ RescopeBeginW(s, T).moved = w[s].moved /\ RescopeBeginW(s, T).pend = w[s].pend
  BY DEF RescopeBeginW
<1>c. /\ RescopeBeginW(s, T).car.id = w[s].car.id /\ RescopeBeginW(s, T).car.paths = w[s].car.paths /\ RescopeBeginW(s, T).car.deletes = w[s].car.deletes
      /\ RescopeBeginW(s, T).car.dropped = w[s].car.dropped /\ RescopeBeginW(s, T).car.pub = w[s].car.pub
  BY DEF RescopeBeginW, CarUncited
<1>d. A4Dcl(RescopeBeginW(s, T), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>2. A4Upl(RescopeBeginW(s, T), w[s]) BY DEF RescopeBeginW, A4Upl
<1>3. A4Brr(RescopeBeginW(s, T), w[s]) BY DEF RescopeBeginW, A4Brr, Bar
<1>. QED BY <1>T, <1>u, <1>t, <1>d, <1>2, <1>3, A4Keep

\* RetireByAck clears the record: nothing is declared after it.
LEMMA RetireByAck_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, RetireByAck(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>w. w' = [w EXCEPT ![s] = RetireByAckW(s)] BY DEF RetireByAck
<1>s. w'[s] = RetireByAckW(s) BY <1>0, <1>w DEF TypeOK
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF RetireByAck, bucket, aux
<1>ck. A3CK BY <1>u, A3CF
<1>f. /\ RetireByAckW(s).local = w[s].local /\ RetireByAckW(s).baseline = w[s].baseline
      /\ RetireByAckW(s).sStage = w[s].sStage /\ RetireByAckW(s).sDrop = w[s].sDrop
      /\ RetireByAckW(s).sKeep = w[s].sKeep /\ RetireByAckW(s).sHeld = w[s].sHeld
      /\ RetireByAckW(s).decl = w[s].decl /\ RetireByAckW(s).pend = NoPend
      /\ RetireByAckW(s).uploads = w[s].uploads /\ RetireByAckW(s).snap = w[s].snap
      /\ RetireByAckW(s).pc = w[s].pc /\ RetireByAckW(s).deletes = w[s].deletes
  BY DEF RetireByAckW
<1>t. A4Tree(RetireByAckW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4D(s, RetireByAckW(s)) BY <1>s, <1>t, <1>ck, <1>ic, A4TreeD
<1>up0. A4Upl(RetireByAckW(s), w[s]) BY <1>f DEF A4Upl
<1>up. A4U(s, RetireByAckW(s)) BY <1>up0, <1>ck, <1>ic, <1>s, A4UplU
<1>p. A4P(s, RetireByAckW(s))
  <2>. SUFFICES ASSUME NEW q \in Paths, A1Priv(s, q)' PROVE FALSE BY DEF A4P
  <2>. QED BY <1>s, <1>f DEF A1Priv, A1Decl, NoPend
<1>g. A4G(s, RetireByAckW(s)) BY <1>f DEF A4G
<1>b0. A4Brr(RetireByAckW(s), w[s]) BY <1>f DEF A4Brr
<1>b. A4B(s, RetireByAckW(s)) BY <1>f, <1>b0, <1>ic, A4BrrB
<1>. QED BY <1>w, <1>u, <1>d, <1>up, <1>p, <1>g, <1>b, A4OneEq

------------------------------------------------------------------------------
(* Consume, Sync, RPullSync, Checkout, Finish: dirt only shrinks.           *)

LEMMA Consume_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Consume(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>c. PICK ck \in CarryClocks(s) :
        IF CheapPath(s) THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
        ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Consume
<1>ck. A3CK BY <1>u, A3CF
<1>st. w[s].sStage = "none" BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>w. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] BY <1>1, <1>c
<2>f. /\ ConsumeCheapW(s, ck).local = w[s].local /\ ConsumeCheapW(s, ck).baseline = w[s].baseline
      /\ ConsumeCheapW(s, ck).sStage = w[s].sStage /\ ConsumeCheapW(s, ck).sDrop = w[s].sDrop /\ ConsumeCheapW(s, ck).sKeep = w[s].sKeep /\ ConsumeCheapW(s, ck).sHeld = w[s].sHeld
  BY DEF ConsumeCheapW
<2>g. /\ ConsumeCheapW(s, ck).decl = w[s].decl /\ ConsumeCheapW(s, ck).dset = w[s].dset /\ ConsumeCheapW(s, ck).moved = w[s].moved /\ ConsumeCheapW(s, ck).pend = w[s].pend
  BY DEF ConsumeCheapW
<2>c. /\ ConsumeCheapW(s, ck).car.id = w[s].car.id /\ ConsumeCheapW(s, ck).car.paths = w[s].car.paths /\ ConsumeCheapW(s, ck).car.deletes = w[s].car.deletes
      /\ ConsumeCheapW(s, ck).car.dropped = w[s].car.dropped /\ ConsumeCheapW(s, ck).car.pub = w[s].car.pub
  BY DEF ConsumeCheapW, CarUncited
<2>t. A4Tree(ConsumeCheapW(s, ck), w[s]) BY <2>f DEF A4Tree, A1LeftSet
<2>d. A4Dcl(ConsumeCheapW(s, ck), w[s]) BY <2>g, <2>c DEF A4Dcl
  <2>2. A4Upl(ConsumeCheapW(s, ck), w[s]) BY DEF ConsumeCheapW, A4Upl
  <2>3. A4Brr(ConsumeCheapW(s, ck), w[s]) BY DEF ConsumeCheapW, A4Brr, Bar
  <2>. QED BY <2>w, <1>u, <2>t, <2>d, <2>2, <2>3, A4Keep
<1>2. CASE ~CheapPath(s)
  <2>w. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] BY <1>2, <1>c
  <2>s. w'[s] = ConsumeW(s, fail, ck) BY <1>0, <2>w DEF TypeOK
<2>g. /\ ConsumeW(s, fail, ck).decl = w[s].decl /\ ConsumeW(s, fail, ck).dset = w[s].dset /\ ConsumeW(s, fail, ck).moved = w[s].moved /\ ConsumeW(s, fail, ck).pend = w[s].pend
  BY DEF ConsumeW
<2>c. /\ ConsumeW(s, fail, ck).car.id = w[s].car.id /\ ConsumeW(s, fail, ck).car.paths = w[s].car.paths /\ ConsumeW(s, fail, ck).car.deletes = w[s].car.deletes
      /\ ConsumeW(s, fail, ck).car.dropped = w[s].car.dropped /\ ConsumeW(s, fail, ck).car.pub = w[s].car.pub
  BY DEF ConsumeW, CarUncited
<2>d. A4Dcl(ConsumeW(s, fail, ck), w[s]) BY <2>g, <2>c DEF A4Dcl
  <2>l. \A q \in Paths :
          /\ ConsumeW(s, fail, ck).local[q] = IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].local[q]
          /\ ConsumeW(s, fail, ck).baseline[q] = IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]
    BY DEF ConsumeW
  <2>n. A1LeftSet(w[s]) = {} BY <1>st DEF A1LeftSet
  <2>sh. \A q \in Paths : A4Shr(ConsumeW(s, fail, ck), w[s], q)
    <3>. SUFFICES ASSUME NEW q \in Paths PROVE A4Shr(ConsumeW(s, fail, ck), w[s], q) OBVIOUS
    <3>1. CASE q \in ConsumeTaken(s, fail) BY <3>1, <2>l DEF A4Shr
    <3>2. CASE q \notin ConsumeTaken(s, fail) BY <3>2, <2>l, <2>n DEF A4Shr
    <3>. QED BY <3>1, <3>2
  <2>dd. A4D(s, ConsumeW(s, fail, ck)) BY <2>s, <2>sh, <1>ck, <1>ic, A4ShrD
  <2>up. A4U(s, ConsumeW(s, fail, ck))
    <3>1. A4Upl(ConsumeW(s, fail, ck), w[s]) BY DEF ConsumeW, A4Upl
    <3>. QED BY <3>1, <1>ck, <1>ic, <2>s, A4UplU
  <2>p. A4P(s, ConsumeW(s, fail, ck)) /\ A4G(s, ConsumeW(s, fail, ck)) BY <2>s, <2>d, <1>ck, <1>ic, A4DclPG
  <2>b. A4B(s, ConsumeW(s, fail, ck)) BY DEF ConsumeW, A4B, Bar
  <2>. QED BY <2>w, <1>u, <2>dd, <2>up, <2>p, <2>b, A4OneEq
<1>. QED BY <1>1, <1>2

LEMMA Sync_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Sync(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>w. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync
<1>s. w'[s] = SyncW(s, fail) BY <1>0, <1>w DEF TypeOK
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Sync, bucket
<1>ck. A3CK BY <1>u, A3CF
<1>g. /\ SyncW(s, fail).decl = w[s].decl /\ SyncW(s, fail).dset = w[s].dset /\ SyncW(s, fail).moved = w[s].moved /\ SyncW(s, fail).pend = w[s].pend
  BY DEF SyncW
<1>c. /\ SyncW(s, fail).car.id = w[s].car.id /\ SyncW(s, fail).car.paths = w[s].car.paths /\ SyncW(s, fail).car.deletes = w[s].car.deletes
      /\ SyncW(s, fail).car.dropped = w[s].car.dropped /\ SyncW(s, fail).car.pub = w[s].car.pub
  BY DEF SyncW, CarUncited
<1>d. A4Dcl(SyncW(s, fail), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>l. \A q \in Paths :
        /\ SyncW(s, fail).local[q] = IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]
        /\ SyncW(s, fail).baseline[q] = IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].baseline[q]
  BY DEF SyncW, SyncBl
<1>r. /\ SyncW(s, fail).sStage = w[s].sStage /\ SyncW(s, fail).sDrop = w[s].sDrop
      /\ SyncW(s, fail).sKeep = w[s].sKeep /\ SyncW(s, fail).sHeld = w[s].sHeld
  BY DEF SyncW
<1>sh. \A q \in Paths : A4Shr(SyncW(s, fail), w[s], q)
  <2>. SUFFICES ASSUME NEW q \in Paths PROVE A4Shr(SyncW(s, fail), w[s], q) OBVIOUS
  <2>1. CASE q \in SyncOwed(s, fail) BY <2>1, <1>l DEF A4Shr
  <2>2. CASE q \notin SyncOwed(s, fail)
    <3>1. SyncW(s, fail).local[q] = w[s].local[q] /\ SyncW(s, fail).baseline[q] = w[s].baseline[q] BY <2>2, <1>l
    <3>2. q \in A1LeftSet(SyncW(s, fail)) <=> q \in A1LeftSet(w[s]) BY <3>1, <1>r DEF A1LeftSet
    <3>. QED BY <3>1, <3>2 DEF A4Shr
  <2>. QED BY <2>1, <2>2
<1>dd. A4D(s, SyncW(s, fail)) BY <1>s, <1>sh, <1>ck, <1>ic, A4ShrD
<1>up. A4U(s, SyncW(s, fail))
  <2>1. A4Upl(SyncW(s, fail), w[s]) BY DEF SyncW, A4Upl
  <2>. QED BY <2>1, <1>ck, <1>ic, <1>s, A4UplU
<1>p. A4P(s, SyncW(s, fail)) /\ A4G(s, SyncW(s, fail)) BY <1>s, <1>d, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, SyncW(s, fail)) BY DEF Sync, SyncW, A4B, Bar
<1>. QED BY <1>w, <1>u, <1>dd, <1>up, <1>p, <1>b, A4OneEq

LEMMA RPullSync_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, RPullSync(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>w. PICK fail \in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync
<1>s. w'[s] = RPullSyncW(s, fail) BY <1>0, <1>w DEF TypeOK
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF RPullSync, bucket
<1>ck. A3CK BY <1>u, A3CF
<1>g. /\ RPullSyncW(s, fail).decl = w[s].decl /\ RPullSyncW(s, fail).dset = w[s].dset /\ RPullSyncW(s, fail).moved = w[s].moved /\ RPullSyncW(s, fail).pend = w[s].pend
  BY DEF RPullSyncW
<1>c. /\ RPullSyncW(s, fail).car.id = w[s].car.id /\ RPullSyncW(s, fail).car.paths = w[s].car.paths /\ RPullSyncW(s, fail).car.deletes = w[s].car.deletes
      /\ RPullSyncW(s, fail).car.dropped = w[s].car.dropped /\ RPullSyncW(s, fail).car.pub = w[s].car.pub
  BY DEF RPullSyncW, CarUncited
<1>d. A4Dcl(RPullSyncW(s, fail), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>l. \A q \in Paths :
        /\ RPullSyncW(s, fail).local[q] = IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]
        /\ RPullSyncW(s, fail).baseline[q] = IF q \in SyncOwed(s, fail) THEN doc[q] ELSE w[s].baseline[q]
  BY DEF RPullSyncW, SyncBl
<1>r. /\ RPullSyncW(s, fail).sStage = w[s].sStage /\ RPullSyncW(s, fail).sDrop = w[s].sDrop
      /\ RPullSyncW(s, fail).sKeep = w[s].sKeep /\ RPullSyncW(s, fail).sHeld = w[s].sHeld
  BY DEF RPullSyncW
<1>sh. \A q \in Paths : A4Shr(RPullSyncW(s, fail), w[s], q)
  <2>. SUFFICES ASSUME NEW q \in Paths PROVE A4Shr(RPullSyncW(s, fail), w[s], q) OBVIOUS
  <2>1. CASE q \in SyncOwed(s, fail) BY <2>1, <1>l DEF A4Shr
  <2>2. CASE q \notin SyncOwed(s, fail)
    <3>1. RPullSyncW(s, fail).local[q] = w[s].local[q] /\ RPullSyncW(s, fail).baseline[q] = w[s].baseline[q] BY <2>2, <1>l
    <3>2. q \in A1LeftSet(RPullSyncW(s, fail)) <=> q \in A1LeftSet(w[s]) BY <3>1, <1>r DEF A1LeftSet
    <3>. QED BY <3>1, <3>2 DEF A4Shr
  <2>. QED BY <2>1, <2>2
<1>dd. A4D(s, RPullSyncW(s, fail)) BY <1>s, <1>sh, <1>ck, <1>ic, A4ShrD
<1>up. A4U(s, RPullSyncW(s, fail))
  <2>1. A4Upl(RPullSyncW(s, fail), w[s]) BY DEF RPullSyncW, A4Upl
  <2>. QED BY <2>1, <1>ck, <1>ic, <1>s, A4UplU
<1>p. A4P(s, RPullSyncW(s, fail)) /\ A4G(s, RPullSyncW(s, fail)) BY <1>s, <1>d, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, RPullSyncW(s, fail)) BY DEF RPullSync, RPullSyncW, A4B, Bar
<1>. QED BY <1>w, <1>u, <1>dd, <1>up, <1>p, <1>b, A4OneEq

LEMMA Checkout_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Checkout(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>i. w[s] = WriterInit BY DEF A4Hyp, Checkout, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Off
<1>T. PICK T \in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] BY DEF Checkout
<1>s. w'[s] = CheckoutW(s, T) BY <1>0, <1>T DEF TypeOK
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Checkout, bucket, aux
<1>ck. A3CK BY <1>u, A3CF
<1>g. /\ CheckoutW(s, T).decl = w[s].decl /\ CheckoutW(s, T).dset = w[s].dset /\ CheckoutW(s, T).moved = w[s].moved /\ CheckoutW(s, T).pend = w[s].pend
  BY DEF CheckoutW
<1>c. /\ CheckoutW(s, T).car.id = w[s].car.id /\ CheckoutW(s, T).car.paths = w[s].car.paths /\ CheckoutW(s, T).car.deletes = w[s].car.deletes
      /\ CheckoutW(s, T).car.dropped = w[s].car.dropped /\ CheckoutW(s, T).car.pub = w[s].car.pub
  BY DEF CheckoutW, CarUncited
<1>d. A4Dcl(CheckoutW(s, T), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>sh. \A q \in Paths : A4Shr(CheckoutW(s, T), w[s], q) BY DEF CheckoutW, A4Shr
<1>dd. A4D(s, CheckoutW(s, T)) BY <1>s, <1>sh, <1>ck, <1>ic, A4ShrD
<1>up. A4U(s, CheckoutW(s, T))
  <2>1. A4Upl(CheckoutW(s, T), w[s]) BY DEF CheckoutW, A4Upl
  <2>. QED BY <2>1, <1>ck, <1>ic, <1>s, A4UplU
<1>p. A4P(s, CheckoutW(s, T)) /\ A4G(s, CheckoutW(s, T)) BY <1>s, <1>d, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, CheckoutW(s, T)) BY <1>i DEF CheckoutW, WriterInit, A4B, Bar
<1>. QED BY <1>T, <1>u, <1>dd, <1>up, <1>p, <1>b, A4OneEq

\* Finish: a published upload's baseline is its snap (the tree was dirty there,
\* UD), a published delete's is Nil (dirty, A4DB); no upload, not in a barrier.
LEMMA Finish_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Finish(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>w. w' = [w EXCEPT ![s] = FinishW(s)] BY DEF Finish
<1>s. w'[s] = FinishW(s) BY <1>0, <1>w DEF TypeOK
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Finish, aux
<1>ck. A3CK BY <1>u, A3CF
<1>g0. w[s].pc = "cased" BY DEF Finish
<1>n0. w[s].sStage = "none" BY <1>g0 DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1
<1>n. A1LeftSet(w[s]) = {} BY <1>n0 DEF A1LeftSet
<1>ud. \A q \in w[s].uploads : w[s].baseline[q] # Nil => w[s].local[q] # w[s].baseline[q]
  BY <1>g0 DEF A4Hyp, DInv, UD, UDR, Bar
<1>dl0. w[s].deletes \subseteq Paths BY TreeTy DEF A4Hyp, IndC
<1>dl. \A q \in w[s].deletes : w[s].local[q] # w[s].baseline[q] BY <1>g0, <1>ic, <1>dl0 DEF A4DB, Bar
<1>g. /\ FinishW(s).decl = w[s].decl /\ FinishW(s).dset = w[s].dset /\ FinishW(s).moved = w[s].moved /\ FinishW(s).pend = w[s].pend
  BY DEF FinishW
<1>c. /\ FinishW(s).car.id = w[s].car.id /\ FinishW(s).car.paths = w[s].car.paths /\ FinishW(s).car.deletes = w[s].car.deletes
      /\ FinishW(s).car.dropped = w[s].car.dropped /\ FinishW(s).car.pub = w[s].car.pub
  BY DEF FinishW, CarUncited
<1>d. A4Dcl(FinishW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>l. \A q \in Paths :
        /\ FinishW(s).local[q] = w[s].local[q]
        /\ FinishW(s).baseline[q] = IF q \in w[s].uploads \cap w[s].upDone THEN w[s].snap[q]
                                    ELSE IF q \in w[s].deletes /\ w[s].inst[q] = Nil THEN Nil
                                    ELSE w[s].baseline[q]
  BY DEF FinishW
<1>sh. \A q \in Paths : A4Shr(FinishW(s), w[s], q)
  <2>. SUFFICES ASSUME NEW q \in Paths PROVE A4Shr(FinishW(s), w[s], q) OBVIOUS
  <2>1. CASE q \in w[s].uploads \cap w[s].upDone
    <3>1. FinishW(s).local[q] = w[s].local[q] /\ FinishW(s).baseline[q] = w[s].snap[q] BY <2>1, <1>l
    <3>2. w[s].local[q] # Nil => w[s].local[q] # w[s].baseline[q] BY <2>1, <1>ud
    <3>. QED BY <3>1, <3>2, <1>n DEF A4Shr
  <2>2. CASE q \notin w[s].uploads \cap w[s].upDone /\ q \in w[s].deletes /\ w[s].inst[q] = Nil
    <3>1. FinishW(s).local[q] = w[s].local[q] /\ FinishW(s).baseline[q] = Nil BY <2>2, <1>l
    <3>. QED BY <3>1, <2>2, <1>dl, <1>n DEF A4Shr
  <2>3. CASE ~(q \in w[s].uploads \cap w[s].upDone) /\ ~(q \in w[s].deletes /\ w[s].inst[q] = Nil)
    <3>1. FinishW(s).local[q] = w[s].local[q] /\ FinishW(s).baseline[q] = w[s].baseline[q] BY <2>3, <1>l
    <3>. QED BY <3>1, <1>n DEF A4Shr
  <2>. QED BY <2>1, <2>2, <2>3
<1>dd. A4D(s, FinishW(s)) BY <1>s, <1>sh, <1>ck, <1>ic, A4ShrD
<1>up. A4U(s, FinishW(s)) BY DEF FinishW, A4U
<1>p. A4P(s, FinishW(s)) /\ A4G(s, FinishW(s)) BY <1>s, <1>d, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, FinishW(s)) BY DEF FinishW, A4B, Bar
<1>. QED BY <1>w, <1>u, <1>dd, <1>up, <1>p, <1>b, A4OneEq

------------------------------------------------------------------------------
(* Edit (fresh bytes), Delete (no file), Touch (declares its dirt).         *)

LEMMA A4TouchMem ==
  ASSUME NEW s, NEW p \in TouchDirty(s), w[s].local[p] # Nil
  PROVE  \A k \in Paths : Content(doc[k]) # Content(w[s].local[p])
<1>1. w[s].local[p] # Nil => \A k \in Paths : Content(doc[k]) # Content(w[s].local[p]) BY DEF TouchDirty
<1>. QED BY <1>1

LEMMA Edit_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, NEW p \in Paths, Edit(s, p), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>y. w[s].local \in [Paths -> Opt(Handles)] /\ minted \subseteq Handles BY <1>ic, TreeTy, WriterFields DEF IndC
<1>g. nextGen \in Nat /\ nextGen > Seed BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>h. PICK hh \in Handles : hh = <<p, nextGen>>
  BY <1>g, MaxMintNat, MaxCopiesNat DEF Edit, Handles, Gens, Seed
<1>1. /\ minted' = minted \cup {hh} /\ orig' = orig /\ w' = [w EXCEPT ![s] = EditW(s, p)]
      /\ doc' = doc /\ gw' = gw
  BY <1>h DEF Edit, aux
<1>4. hh \notin minted
  <2>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <2>2. nextGen <= MaxMint BY DEF Edit
  <2>. QED BY <2>1, <2>2, <1>g, <1>h, MaxMintNat DEF Gen
<1>6. orig[hh] = Nil BY <1>4 DEF A4Hyp, IndC, IndM5, M5, Hist, HistNew
<1>s. w'[s] = EditW(s, p) BY <1>0, <1>1 DEF TypeOK
<1>ck. A3CK BY <1>1, A3CF
<1>l. EditW(s, p).local[p] = hh /\ \A q \in Paths \ {p} : EditW(s, p).local[q] = w[s].local[q]
  BY <1>y, <1>h DEF EditW
<1>f. /\ EditW(s, p).baseline = w[s].baseline /\ EditW(s, p).sStage = w[s].sStage /\ EditW(s, p).sDrop = w[s].sDrop
      /\ EditW(s, p).sKeep = w[s].sKeep /\ EditW(s, p).sHeld = w[s].sHeld /\ EditW(s, p).pc = w[s].pc
      /\ EditW(s, p).deletes = w[s].deletes /\ EditW(s, p).uploads = w[s].uploads /\ EditW(s, p).snap = w[s].snap
  BY DEF EditW
<1>gd. /\ EditW(s, p).decl = w[s].decl /\ EditW(s, p).dset = w[s].dset /\ EditW(s, p).pend = w[s].pend
       /\ w[s].moved \subseteq EditW(s, p).moved
  BY DEF EditW
<1>c. /\ EditW(s, p).car.id = w[s].car.id /\ EditW(s, p).car.paths = w[s].car.paths /\ EditW(s, p).car.deletes = w[s].car.deletes
      /\ EditW(s, p).car.dropped = w[s].car.dropped /\ EditW(s, p).car.pub = w[s].car.pub
  BY DEF EditW
<1>d. A4Dcl(EditW(s, p), w[s]) BY <1>gd, <1>c DEF A4Dcl
\* the edited file's bytes are fresh: hh is no copy
<1>hc. Content(hh)' = hh BY <1>1, <1>6, <1>h, NilHandle DEF Content
<1>dd. A4D(s, EditW(s, p))
  <2>. SUFFICES ASSUME NEW q \in Paths, A4Dirt(s, q)'
                PROVE  \/ A4Dirt(s, q) /\ (Content(w[s].local[q]))' = Content(w[s].local[q])
                       \/ (Content(w[s].local[q]))' \notin minted
    BY DEF A4D
  <2>1. CASE q = p BY <2>1, <1>l, <1>hc, <1>4, <1>s
  <2>2. CASE q # p
    <3>1. EditW(s, p).local[q] = w[s].local[q] BY <2>2, <1>l
    <3>2. q \in A1LeftSet(EditW(s, p)) <=> q \in A1LeftSet(w[s]) BY <3>1, <1>f DEF A1LeftSet
    <3>3. A4Dirt(s, q) BY <3>1, <3>2, <1>s, <1>f DEF A4Dirt
    <3>4. PICK hl \in Opt(minted) : hl = w[s].local[q] BY <1>ic, A4Ty
    <3>. QED BY <3>1, <3>3, <3>4, <1>ck, <1>s DEF A3CK
  <2>. QED BY <2>1, <2>2
<1>up. A4U(s, EditW(s, p))
  <2>1. A4Upl(EditW(s, p), w[s]) BY <1>f DEF A4Upl
  <2>. QED BY <2>1, <1>ck, <1>ic, <1>s, A4UplU
<1>p. A4P(s, EditW(s, p)) /\ A4G(s, EditW(s, p)) BY <1>s, <1>d, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, EditW(s, p))
  <2>. SUFFICES ASSUME NEW q \in Paths, EditW(s, p).pc \in Bar, q \in EditW(s, p).deletes
                PROVE  EditW(s, p).baseline[q] # Nil /\ EditW(s, p).local[q] # EditW(s, p).baseline[q]
    BY DEF A4B
  <2>1. w[s].baseline[q] # Nil /\ w[s].local[q] # w[s].baseline[q] BY <1>f, <1>ic DEF A4DB
  <2>2. w[s].baseline[q] \in Opt(minted) BY <1>ic, A4Ty
  <2>3. CASE q = p BY <2>1, <2>2, <2>3, <1>l, <1>f, <1>4 DEF Opt
  <2>4. CASE q # p BY <2>1, <2>4, <1>l, <1>f
  <2>. QED BY <2>3, <2>4
<1>. QED BY <1>1, <1>dd, <1>up, <1>p, <1>b, A4OneEq

LEMMA Delete_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, NEW p \in Paths, Delete(s, p), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>y. w[s].local \in [Paths -> Opt(Handles)] BY <1>ic, TreeTy, WriterFields DEF IndC
<1>1. w' = [w EXCEPT ![s] = DeleteW(s, p)] /\ doc' = doc /\ gw' = gw /\ minted' = minted /\ orig' = orig
  BY DEF Delete, bucket, aux
<1>s. w'[s] = DeleteW(s, p) BY <1>0, <1>1 DEF TypeOK
<1>ck. A3CK BY <1>1, A3CF
<1>l. DeleteW(s, p).local[p] = Nil /\ \A q \in Paths \ {p} : DeleteW(s, p).local[q] = w[s].local[q]
  BY <1>y DEF DeleteW
<1>f. /\ DeleteW(s, p).baseline = w[s].baseline /\ DeleteW(s, p).sStage = w[s].sStage /\ DeleteW(s, p).sDrop = w[s].sDrop
      /\ DeleteW(s, p).sKeep = w[s].sKeep /\ DeleteW(s, p).sHeld = w[s].sHeld /\ DeleteW(s, p).pc = w[s].pc
      /\ DeleteW(s, p).deletes = w[s].deletes /\ DeleteW(s, p).uploads = w[s].uploads /\ DeleteW(s, p).snap = w[s].snap
  BY DEF DeleteW
<1>gd. /\ DeleteW(s, p).decl = w[s].decl /\ DeleteW(s, p).dset = w[s].dset /\ DeleteW(s, p).pend = w[s].pend
       /\ w[s].moved \subseteq DeleteW(s, p).moved
  BY DEF DeleteW
<1>c. /\ DeleteW(s, p).car.id = w[s].car.id /\ DeleteW(s, p).car.paths = w[s].car.paths /\ DeleteW(s, p).car.deletes = w[s].car.deletes
      /\ DeleteW(s, p).car.dropped = w[s].car.dropped /\ DeleteW(s, p).car.pub = w[s].car.pub
  BY DEF DeleteW
<1>d. A4Dcl(DeleteW(s, p), w[s]) BY <1>gd, <1>c DEF A4Dcl
<1>sh. \A q \in Paths : A4Shr(DeleteW(s, p), w[s], q)
  <2>. SUFFICES ASSUME NEW q \in Paths PROVE A4Shr(DeleteW(s, p), w[s], q) OBVIOUS
  <2>1. CASE q = p BY <2>1, <1>l DEF A4Shr
  <2>2. CASE q # p
    <3>1. DeleteW(s, p).local[q] = w[s].local[q] BY <2>2, <1>l
    <3>2. q \in A1LeftSet(DeleteW(s, p)) <=> q \in A1LeftSet(w[s]) BY <3>1, <1>f DEF A1LeftSet
    <3>. QED BY <3>1, <3>2, <1>f DEF A4Shr
  <2>. QED BY <2>1, <2>2
<1>dd. A4D(s, DeleteW(s, p)) BY <1>s, <1>sh, <1>ck, <1>ic, A4ShrD
<1>up. A4U(s, DeleteW(s, p))
  <2>1. A4Upl(DeleteW(s, p), w[s]) BY <1>f DEF A4Upl
  <2>. QED BY <2>1, <1>ck, <1>ic, <1>s, A4UplU
<1>p. A4P(s, DeleteW(s, p)) /\ A4G(s, DeleteW(s, p)) BY <1>s, <1>d, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, DeleteW(s, p))
  <2>. SUFFICES ASSUME NEW q \in Paths, DeleteW(s, p).pc \in Bar, q \in DeleteW(s, p).deletes
                PROVE  DeleteW(s, p).baseline[q] # Nil /\ DeleteW(s, p).local[q] # DeleteW(s, p).baseline[q]
    BY DEF A4B
  <2>1. w[s].baseline[q] # Nil /\ w[s].local[q] # w[s].baseline[q] BY <1>f, <1>ic DEF A4DB
  <2>. QED BY <2>1, <1>l, <1>f
<1>. QED BY <1>1, <1>dd, <1>up, <1>p, <1>b, A4OneEq

\* Touch: a declared path's bytes are its dirt, held at no document path
\* (TouchDirty); an undeclared one keeps its record.
LEMMA Touch_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Touch(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>sh. RescopeUnciteFirst /\ UnlinkChecksBytes BY ShippedShape DEF Shipped
<1>w. w' = [w EXCEPT ![s] = TouchW(s)] BY DEF Touch
<1>s. w'[s] = TouchW(s) BY <1>0, <1>w DEF TypeOK
<1>u. doc' = doc /\ gw' = gw /\ minted' = minted /\ orig' = orig BY DEF Touch, bucket, aux
<1>ck. A3CK BY <1>u, A3CF
<1>f. /\ TouchW(s).local = w[s].local /\ TouchW(s).baseline = w[s].baseline
      /\ TouchW(s).sStage = w[s].sStage /\ TouchW(s).sDrop = w[s].sDrop /\ TouchW(s).sKeep = w[s].sKeep /\ TouchW(s).sHeld = w[s].sHeld
      /\ TouchW(s).pc = w[s].pc /\ TouchW(s).deletes = w[s].deletes /\ TouchW(s).uploads = w[s].uploads /\ TouchW(s).snap = w[s].snap
      /\ TouchW(s).car = w[s].car
  BY DEF TouchW
<1>t. A4Tree(TouchW(s), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>dd. A4D(s, TouchW(s)) BY <1>s, <1>t, <1>ck, <1>ic, A4TreeD
<1>up. A4U(s, TouchW(s))
  <2>1. A4Upl(TouchW(s), w[s]) BY <1>f DEF A4Upl
  <2>. QED BY <2>1, <1>ck, <1>ic, <1>s, A4UplU
<1>dl. \A q \in Paths : TouchW(s).decl[q] = IF q \in TouchDirty(s) THEN w[s].local[q]
                                          ELSE IF TouchFresh(s) THEN Nil ELSE w[s].decl[q]
  BY DEF TouchW
<1>g. A4G(s, TouchW(s)) BY <1>dl DEF A4G
<1>p. A4P(s, TouchW(s))
  <2>. SUFFICES ASSUME NEW q \in Paths, A1Priv(s, q)'
                PROVE  \/ A1Priv(s, q) /\ (Content(w[s].decl[q]))' = Content(w[s].decl[q])
                       \/ /\ A4Dirt(s, q) /\ (Content(w[s].decl[q]))' = Content(w[s].local[q])
                          /\ \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[s].local[q])
    BY DEF A4P
  <2>0. TouchW(s).decl[q] # Nil BY <1>s DEF A1Priv
  <2>1. CASE q \in TouchDirty(s)
    <3>1. TouchW(s).decl[q] = w[s].local[q] BY <2>1, <1>dl
    <3>2. w[s].local[q] # Nil BY <2>0, <3>1
    <3>3. w[s].local[q] # w[s].baseline[q] /\ ~RescopeLeftover(s, q) BY <2>1, TouchDirtyMem
    <3>4. q \notin A1LeftSet(w[s])
        <4>. SUFFICES ASSUME q \in A1LeftSet(w[s]) PROVE FALSE OBVIOUS
        <4>a. /\ q \in w[s].sDrop /\ w[s].local[q] # Nil /\ w[s].sHeld[q] # Nil /\ w[s].local[q] = w[s].sHeld[q]
              /\ \/ w[s].sStage = "mid" /\ q \notin w[s].sKeep
                 \/ w[s].sStage = "saved" /\ w[s].baseline[q] = Nil
          BY DEF A1LeftSet
        <4>b. Content(w[s].local[q]) = Content(w[s].sHeld[q]) BY <4>a
        <4>c. RescopeLeftover(s, q) BY <4>a, <4>b, <1>sh, SMTT(300) DEF RescopeLeftover
        <4>. QED BY <4>c, <3>3
    <3>5. A4Dirt(s, q) BY <3>2, <3>3, <3>4 DEF A4Dirt
    <3>6. PICK hl \in Opt(minted) : hl = w[s].local[q] BY <1>ic, A4Ty
    <3>7. (Content(w[s].decl[q]))' = Content(w[s].local[q]) BY <3>1, <3>6, <1>ck, <1>s DEF A3CK
    <3>8. \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[s].local[q]) BY <2>1, <3>2, A4TouchMem
    <3>. QED BY <3>5, <3>7, <3>8
  <2>2. CASE q \notin TouchDirty(s)
    <3>0. ~TouchFresh(s) BY <2>0, <2>2, <1>dl
    <3>1. TouchW(s).decl[q] = w[s].decl[q] BY <2>2, <3>0, <1>dl
    <3>2. TouchW(s).pend.id = w[s].pend.id BY <3>0 DEF TouchW
    <3>3. /\ (q \in TouchW(s).dset <=> q \in w[s].dset)
          /\ (q \in TouchW(s).moved <=> q \in w[s].moved)
      BY <2>2, <3>0 DEF TouchW
    <3>4. A1Decl(s, q) BY <3>2, <3>3, <1>s DEF A1Priv, A1Decl
    <3>5. PICK c0 : c0 = w[s].car OBVIOUS
    <3>6. PICK hd \in Opt(minted) : hd = w[s].decl[q] BY <1>ic, A4Ty
    <3>7. PICK hp \in Opt(minted) : hp = w[s].car.pub[q] BY <1>ic DEF A1IAK1, A1CarPubMinted
    <3>8. Content(hd)' = Content(hd) /\ Content(hp)' = Content(hp) BY <3>6, <3>7, <1>ck DEF A3CK
    <3>9. CarForm(s, q, c0)' <=> CarForm(s, q, c0) BY <3>1, <3>5, <3>6, <3>7, <3>8, <1>s DEF CarForm
    <3>10. A1Priv(s, q) BY <3>1, <3>2, <3>4, <3>5, <3>9, <1>s, <1>f DEF A1Priv
    <3>11. (Content(w[s].decl[q]))' = Content(w[s].decl[q]) BY <3>1, <3>6, <3>8, <1>s
    <3>. QED BY <3>10, <3>11
  <2>. QED BY <2>1, <2>2
<1>b. A4B(s, TouchW(s))
  <2>1. A4Brr(TouchW(s), w[s]) BY <1>f DEF A4Brr
  <2>. QED BY <2>1, <1>f, <1>ic, A4BrrB
<1>. QED BY <1>w, <1>u, <1>dd, <1>up, <1>p, <1>g, <1>b, A4OneEq

------------------------------------------------------------------------------
(* Scan (uploads are dirt) and Upload (its copy names the same bytes).      *)

LEMMA Scan_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Scan(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>dl. PICK dels \in (IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s)) :
         w' = [w EXCEPT ![s] = ScanW(s, dels)]
  BY DEF Scan
<1>ds. dels \subseteq ScanAbsent(s) BY <1>dl
<1>s. w'[s] = ScanW(s, dels) BY <1>0, <1>dl DEF TypeOK
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF Scan, bucket, aux
<1>ck. A3CK BY <1>u, A3CF
<1>g0. w[s].pc = "consumed" BY DEF Scan
<1>n0. w[s].sStage = "none" BY <1>g0 DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1
<1>n. A1LeftSet(w[s]) = {} BY <1>n0 DEF A1LeftSet
<1>f. /\ ScanW(s, dels).local = w[s].local /\ ScanW(s, dels).baseline = w[s].baseline
      /\ ScanW(s, dels).sStage = w[s].sStage /\ ScanW(s, dels).sDrop = w[s].sDrop /\ ScanW(s, dels).sKeep = w[s].sKeep /\ ScanW(s, dels).sHeld = w[s].sHeld
  BY DEF ScanW
<1>g. /\ ScanW(s, dels).decl = w[s].decl /\ ScanW(s, dels).dset = w[s].dset /\ ScanW(s, dels).moved = w[s].moved /\ ScanW(s, dels).pend = w[s].pend
  BY DEF ScanW
<1>c. /\ ScanW(s, dels).car.id = w[s].car.id /\ ScanW(s, dels).car.paths = w[s].car.paths /\ ScanW(s, dels).car.deletes = w[s].car.deletes
      /\ ScanW(s, dels).car.dropped = w[s].car.dropped /\ ScanW(s, dels).car.pub = w[s].car.pub
  BY DEF ScanW, CarUncited
<1>t. A4Tree(ScanW(s, dels), w[s]) BY <1>f DEF A4Tree, A1LeftSet
<1>d. A4Dcl(ScanW(s, dels), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>dd. A4D(s, ScanW(s, dels)) BY <1>s, <1>t, <1>ck, <1>ic, A4TreeD
<1>r. /\ ScanW(s, dels).uploads = ScanUps(s) /\ ScanW(s, dels).snap = w[s].local
      /\ ScanW(s, dels).deletes = dels /\ ScanW(s, dels).pc = "scanned"
  BY DEF ScanW
<1>up. A4U(s, ScanW(s, dels))
  <2>. SUFFICES ASSUME NEW k \in Paths, k \in ScanW(s, dels).uploads
                PROVE  A4Dirt(s, k) /\ (Content(w[s].snap[k]))' = Content(w[s].local[k])
    BY DEF A4U
  <2>1. w[s].local[k] # w[s].baseline[k] /\ w[s].local[k] # Nil BY <1>r DEF ScanUps, ScanDirty
  <2>2. A4Dirt(s, k) BY <2>1, <1>n DEF A4Dirt
  <2>3. PICK hl \in Opt(minted) : hl = w[s].local[k] BY <1>ic, A4Ty
  <2>. QED BY <2>2, <2>3, <1>r, <1>ck, <1>s DEF A3CK
<1>p. A4P(s, ScanW(s, dels)) /\ A4G(s, ScanW(s, dels)) BY <1>s, <1>d, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, ScanW(s, dels))
  <2>. SUFFICES ASSUME NEW q \in Paths, q \in dels
                PROVE  w[s].baseline[q] # Nil /\ w[s].local[q] # w[s].baseline[q]
    BY <1>r, <1>f DEF A4B
  <2>. QED BY <1>ds DEF ScanAbsent, ScanDirty
<1>. QED BY <1>dl, <1>u, <1>dd, <1>up, <1>p, <1>b, A4OneEq

LEMMA Upload_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, NEW p \in Paths, Upload(s, p), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4Inv BY DEF A4Hyp
<1>db. A4DB BY DEF A4Hyp, A4Inv
<1>k. w[s].pc = "scanned" /\ p \in w[s].uploads BY DEF Upload
<1>uu. doc' = doc /\ gw' = gw BY DEF Upload
<1>n0. w[s].sStage = "none" BY <1>k DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Rescope, R1
<1>n. A1LeftSet(w[s]) = {} BY <1>n0 DEF A1LeftSet
<1>y. /\ \A q \in w[s].uploads : w[s].snap[q] \in minted
      /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ minted \subseteq Handles
  BY <1>ic, TreeTy, WriterFields DEF IndC
<1>1. CASE w[s].snap[p] \notin upped
  <2>w. w' = [w EXCEPT ![s] = UploadW(s, p)] /\ minted' = minted /\ orig' = orig BY <1>1 DEF Upload
<2>f. /\ UploadW(s, p).local = w[s].local /\ UploadW(s, p).baseline = w[s].baseline
      /\ UploadW(s, p).sStage = w[s].sStage /\ UploadW(s, p).sDrop = w[s].sDrop /\ UploadW(s, p).sKeep = w[s].sKeep /\ UploadW(s, p).sHeld = w[s].sHeld
  BY DEF UploadW
<2>g. /\ UploadW(s, p).decl = w[s].decl /\ UploadW(s, p).dset = w[s].dset /\ UploadW(s, p).moved = w[s].moved /\ UploadW(s, p).pend = w[s].pend
  BY DEF UploadW
<2>c. /\ UploadW(s, p).car.id = w[s].car.id /\ UploadW(s, p).car.paths = w[s].car.paths /\ UploadW(s, p).car.deletes = w[s].car.deletes
      /\ UploadW(s, p).car.dropped = w[s].car.dropped /\ UploadW(s, p).car.pub = w[s].car.pub
  BY DEF UploadW, CarUncited
<2>t. A4Tree(UploadW(s, p), w[s]) BY <2>f DEF A4Tree, A1LeftSet
<2>d. A4Dcl(UploadW(s, p), w[s]) BY <2>g, <2>c DEF A4Dcl
  <2>2. A4Upl(UploadW(s, p), w[s]) BY DEF UploadW, A4Upl
  <2>3. A4Brr(UploadW(s, p), w[s]) BY <1>k DEF UploadW, A4Brr, Bar
  <2>. QED BY <2>w, <1>uu, <2>t, <2>d, <2>2, <2>3, A4Keep
<1>2. CASE w[s].snap[p] \in upped
  <2>h0. PICK h0 \in minted : h0 = w[s].snap[p] BY <1>k, <1>y
  <2>g. copies \in Nat /\ copies < MaxCopies
    BY <1>2 DEF Upload, A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, Ghosts
  <2>c. PICK c \in Handles : c = <<p, MaxMint + copies + 1>>
    BY <2>g, MaxMintNat, MaxCopiesNat DEF Handles, Gens, Seed
  <2>1. /\ minted' = minted \cup {c} /\ orig' = [orig EXCEPT ![c] = Content(h0)]
        /\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)]
    BY <1>2, <2>h0, <2>c DEF Upload
  <2>4. c \notin minted
    <3>1. \A m \in minted : Gen(m) < nextGen \/ (Gen(m) > MaxMint /\ Gen(m) <= MaxMint + copies)
      BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>2. nextGen <= MaxMint + 1 BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
    <3>3. nextGen \in Nat BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
    <3>. QED BY <3>1, <3>2, <3>3, <2>c, <2>g, MaxMintNat DEF Gen
  <2>ty. orig \in [Handles -> Opt(Handles)] BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK, Ghosts
  <2>ck. A3CK
    <3>. SUFFICES ASSUME NEW h \in Opt(minted) PROVE Content(h)' = Content(h) BY DEF A3CK
    <3>1. h # c BY <2>4, <2>c, NilHandle DEF Opt
    <3>3. CASE h = Nil BY <3>3 DEF Content
    <3>4. CASE h # Nil
      <4>1. h \in Handles BY <3>4, <1>y DEF Opt
      <4>2. orig'[h] = orig[h] BY <4>1, <3>1, <2>1, <2>ty
      <4>. QED BY <4>2, <3>4 DEF Content
    <3>. QED BY <3>3, <3>4
  <2>hm. Content(h0) \in minted /\ Content(h0) # Nil BY <2>h0, <1>ic, <1>y, NilHandle, A4CM DEF Opt
  <2>cc. Content(c)' = Content(h0)
    <3>1. c # Nil BY <2>c, NilHandle
    <3>2. orig'[c] = Content(h0) BY <2>1, <2>ty
    <3>. QED BY <3>1, <3>2, <2>hm DEF Content
  \* the copy's bytes are no save: an original's are an original (orig Nil), the snap no save (Flight)
  <2>o. A4O
    <3>. SUFFICES ASSUME NEW h \in minted' PROVE orig'[h] = orig[h] \/ (h \notin minted /\ \A q \in Paths : gw[q] # Nil => orig'[h] # gw[q])
      BY DEF A4O
    <3>1. CASE h # c
      <4>1. h \in Handles BY <2>1, <1>y, <2>c
      <4>. QED BY <3>1, <4>1, <2>1, <2>ty
    <3>2. CASE h = c
      <4>. SUFFICES ASSUME NEW q \in Paths, gw[q] # Nil PROVE Content(h0) # gw[q] BY <3>2, <2>4, <2>1, <2>ty
      <4>1. \A m \in minted : orig[m] # gw[q] BY DEF A4Gw, A4Inv, A4Hyp
      <4>2. w[s].snap[p] # gw[q] BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, M2, Flight
      <4>3. CASE orig[h0] # Nil BY <4>3, <4>1, <2>h0 DEF Content
      <4>4. CASE orig[h0] = Nil BY <4>4, <4>2, <2>h0 DEF Content
      <4>. QED BY <4>3, <4>4
    <3>. QED BY <3>1, <3>2
  <2>s. w'[s] = UploadCopyW(s, p, c) BY <1>0, <2>1 DEF TypeOK
  <2>l. \A q \in Paths : /\ UploadCopyW(s, p, c).local[q] = IF q = p /\ w[s].local[p] = h0 THEN c ELSE w[s].local[q]
                         /\ UploadCopyW(s, p, c).snap[q] = IF q = p THEN c ELSE w[s].snap[q]
    BY <1>y, <2>h0 DEF UploadCopyW
  <2>f. /\ UploadCopyW(s, p, c).baseline = w[s].baseline /\ UploadCopyW(s, p, c).sStage = w[s].sStage
        /\ UploadCopyW(s, p, c).sDrop = w[s].sDrop /\ UploadCopyW(s, p, c).sKeep = w[s].sKeep
        /\ UploadCopyW(s, p, c).sHeld = w[s].sHeld /\ UploadCopyW(s, p, c).pc = w[s].pc
        /\ UploadCopyW(s, p, c).deletes = w[s].deletes /\ UploadCopyW(s, p, c).uploads = w[s].uploads
    BY DEF UploadCopyW
<2>g. /\ UploadCopyW(s, p, c).decl = w[s].decl /\ UploadCopyW(s, p, c).dset = w[s].dset /\ UploadCopyW(s, p, c).moved = w[s].moved /\ UploadCopyW(s, p, c).pend = w[s].pend
  BY DEF UploadCopyW
<2>c. /\ UploadCopyW(s, p, c).car.id = w[s].car.id /\ UploadCopyW(s, p, c).car.paths = w[s].car.paths /\ UploadCopyW(s, p, c).car.deletes = w[s].car.deletes
      /\ UploadCopyW(s, p, c).car.dropped = w[s].car.dropped /\ UploadCopyW(s, p, c).car.pub = w[s].car.pub
  BY DEF UploadCopyW, CarUncited
<2>d. A4Dcl(UploadCopyW(s, p, c), w[s]) BY <2>g, <2>c DEF A4Dcl
  \* the uploaded file was dirt (UD; no leftover in a barrier)
  <2>pd. w[s].local[p] = h0 => A4Dirt(s, p)
    <3>. SUFFICES ASSUME w[s].local[p] = h0 PROVE A4Dirt(s, p) OBVIOUS
    <3>1. h0 # Nil BY <2>h0, <1>y, NilHandle
    <3>2. w[s].baseline[p] # Nil => w[s].local[p] # w[s].baseline[p] BY <1>k DEF A4Hyp, DInv, UD, UDR, Bar
    <3>. QED BY <3>1, <3>2, <1>n DEF A4Dirt
  <2>dd. A4D(s, UploadCopyW(s, p, c))
    <3>. SUFFICES ASSUME NEW q \in Paths, A4Dirt(s, q)'
                  PROVE  A4Dirt(s, q) /\ (Content(w[s].local[q]))' = Content(w[s].local[q])
      BY DEF A4D
    <3>1. CASE q = p /\ w[s].local[p] = h0 BY <3>1, <2>l, <2>cc, <2>pd, <2>h0, <2>s
    <3>2. CASE ~(q = p /\ w[s].local[p] = h0)
      <4>1. UploadCopyW(s, p, c).local[q] = w[s].local[q] BY <3>2, <2>l
      <4>2. A4Dirt(s, q) BY <4>1, <2>f, <2>s, <1>n DEF A4Dirt, A1LeftSet
      <4>3. PICK hl \in Opt(minted) : hl = w[s].local[q] BY <1>ic, A4Ty
      <4>. QED BY <4>1, <4>2, <4>3, <2>ck, <2>s DEF A3CK
    <3>. QED BY <3>1, <3>2
  <2>up. A4U(s, UploadCopyW(s, p, c))
    <3>. SUFFICES ASSUME NEW k \in Paths, k \in UploadCopyW(s, p, c).uploads
                  PROVE  k \in w[s].uploads /\ (Content(w[s].snap[k]))' = Content(w[s].snap[k])
      BY DEF A4U
    <3>1. CASE k = p BY <3>1, <2>l, <2>cc, <2>h0, <1>k, <2>s
    <3>2. CASE k # p
      <4>1. PICK hs \in Opt(minted) : hs = w[s].snap[k] BY <1>ic, A4Ty
      <4>. QED BY <3>2, <4>1, <2>l, <2>f, <2>ck, <2>s DEF A3CK
    <3>. QED BY <3>1, <3>2
  <2>p. A4P(s, UploadCopyW(s, p, c)) /\ A4G(s, UploadCopyW(s, p, c)) BY <2>s, <2>d, <2>ck, <1>ic, A4DclPG
  <2>b. A4B(s, UploadCopyW(s, p, c))
    <3>. SUFFICES ASSUME NEW q \in Paths, q \in w[s].deletes
                  PROVE  w[s].baseline[q] # Nil /\ UploadCopyW(s, p, c).local[q] # w[s].baseline[q]
      BY <2>f DEF A4B
    <3>1. q # p BY <1>k DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Mine
    <3>. QED BY <3>1, <2>l, <1>k, <1>db DEF A4DB, Bar
  <2>m. minted \subseteq minted' BY <2>1
  <2>. QED BY <2>1, <1>uu, <2>m, <2>ck, <2>o, <2>dd, <2>up, <2>p, <2>b, A4One
<1>. QED BY <1>1, <1>2

------------------------------------------------------------------------------
(* The narrow's halves.  The first uncites (the baseline goes, the held     *)
(* handle is recorded): a clean file becomes the leftover, a dirty one was  *)
(* dirt.  The second unlinks the leftover and fetches the widen.            *)

LEMMA RescopeFirst_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, RescopeFirst(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>sh. RescopeUnciteFirst /\ RescopeKeepsDirty BY ShippedShape DEF Shipped
<1>w. w' = [w EXCEPT ![s] = RescopeFirstW(s)] BY DEF RescopeFirst
<1>s. w'[s] = RescopeFirstW(s) BY <1>0, <1>w DEF TypeOK
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF RescopeFirst, bucket, aux
<1>ck. A3CK BY <1>u, A3CF
<1>st. w[s].sStage = "saved" /\ w[s].pc = "idle" BY DEF RescopeFirst
<1>ty. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)]
       /\ w[s].sHeld \in [Paths -> Opt(Handles)]
  BY <1>ic, TreeTy, WriterFields DEF IndC
<1>g. /\ RescopeFirstW(s).decl = w[s].decl /\ RescopeFirstW(s).dset = w[s].dset /\ RescopeFirstW(s).moved = w[s].moved /\ RescopeFirstW(s).pend = w[s].pend
  BY DEF RescopeFirstW
<1>c. /\ RescopeFirstW(s).car.id = w[s].car.id /\ RescopeFirstW(s).car.paths = w[s].car.paths /\ RescopeFirstW(s).car.deletes = w[s].car.deletes
      /\ RescopeFirstW(s).car.dropped = w[s].car.dropped /\ RescopeFirstW(s).car.pub = w[s].car.pub
  BY DEF RescopeFirstW, CarUncited
<1>d. A4Dcl(RescopeFirstW(s), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>r. /\ RescopeFirstW(s).local = w[s].local
      /\ \A q \in Paths : RescopeFirstW(s).baseline[q] = IF q \in RescopeFirstDd(s) THEN Nil ELSE w[s].baseline[q]
      /\ RescopeFirstW(s).sStage = "mid" /\ RescopeFirstW(s).sKeep = KeepSet(s) /\ RescopeFirstW(s).sDrop = w[s].sDrop
      /\ \A q \in Paths : RescopeFirstW(s).sHeld[q] = IF q \in RescopeFirstDd(s) /\ w[s].baseline[q] # Nil
                                                     THEN w[s].baseline[q] ELSE w[s].sHeld[q]
  BY <1>sh, <1>ty DEF RescopeFirstW
<1>kp. KeepSet(s) = {q \in w[s].sDrop : w[s].baseline[q] # Nil /\ w[s].local[q] # w[s].baseline[q]}
  BY <1>sh DEF KeepSet
<1>sx. \A q \in Paths : A4Shr(RescopeFirstW(s), w[s], q)
  <2>. SUFFICES ASSUME NEW q \in Paths PROVE A4Shr(RescopeFirstW(s), w[s], q) OBVIOUS
  <2>1. CASE q \notin w[s].sDrop
    <3>1. q \notin RescopeFirstDd(s) BY <2>1 DEF RescopeFirstDd
    <3>2. q \notin A1LeftSet(w[s]) /\ q \notin A1LeftSet(RescopeFirstW(s)) BY <2>1, <1>r DEF A1LeftSet
    <3>. QED BY <3>1, <3>2, <1>r DEF A4Shr
  <2>2. CASE q \in KeepSet(s)
    <3>1. q \notin RescopeFirstDd(s) BY <2>2 DEF RescopeFirstDd
    <3>2. w[s].baseline[q] # Nil /\ w[s].local[q] # w[s].baseline[q] BY <2>2, <1>kp
    <3>3. q \notin A1LeftSet(w[s]) BY <3>2, <1>st DEF A1LeftSet
    <3>. QED BY <3>1, <3>2, <3>3, <1>r DEF A4Shr
  <2>3. CASE q \in RescopeFirstDd(s) /\ w[s].baseline[q] # Nil
    <3>0. q \in w[s].sDrop /\ q \notin KeepSet(s) BY <2>3 DEF RescopeFirstDd
    <3>1. RescopeFirstW(s).baseline[q] = Nil /\ RescopeFirstW(s).sHeld[q] = w[s].baseline[q] BY <2>3, <1>r
    <3>2. q \notin A1LeftSet(w[s]) BY <2>3, <1>st DEF A1LeftSet
    <3>3. CASE w[s].local[q] = w[s].baseline[q]
      <4>1. q \in A1LeftSet(RescopeFirstW(s)) BY <3>0, <3>1, <3>3, <2>3, <1>r DEF A1LeftSet
      <4>. QED BY <4>1 DEF A4Shr
    <3>4. CASE w[s].local[q] # w[s].baseline[q] BY <3>4, <3>2, <1>r DEF A4Shr
    <3>. QED BY <3>3, <3>4
  <2>4. CASE q \in RescopeFirstDd(s) /\ w[s].baseline[q] = Nil
    <3>0. q \in w[s].sDrop /\ q \notin KeepSet(s) BY <2>4 DEF RescopeFirstDd
    <3>1. RescopeFirstW(s).baseline[q] = Nil /\ RescopeFirstW(s).sHeld[q] = w[s].sHeld[q] BY <2>4, <1>r
    <3>2. q \in A1LeftSet(RescopeFirstW(s)) <=> q \in A1LeftSet(w[s]) BY <3>0, <3>1, <2>4, <1>r, <1>st DEF A1LeftSet
    <3>. QED BY <3>1, <3>2, <2>4, <1>r DEF A4Shr
  <2>0. q \in w[s].sDrop /\ q \notin KeepSet(s) => q \in RescopeFirstDd(s) BY DEF RescopeFirstDd
  <2>. QED BY <2>0, <2>1, <2>2, <2>3, <2>4
<1>dd. A4D(s, RescopeFirstW(s)) BY <1>s, <1>sx, <1>ck, <1>ic, A4ShrD
<1>up. A4U(s, RescopeFirstW(s))
  <2>1. A4Upl(RescopeFirstW(s), w[s]) BY DEF RescopeFirstW, A4Upl
  <2>. QED BY <2>1, <1>ck, <1>ic, <1>s, A4UplU
<1>p. A4P(s, RescopeFirstW(s)) /\ A4G(s, RescopeFirstW(s)) BY <1>s, <1>d, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, RescopeFirstW(s)) BY <1>st DEF RescopeFirstW, A4B, Bar
<1>. QED BY <1>w, <1>u, <1>dd, <1>up, <1>p, <1>b, A4OneEq

LEMMA RescopeSecond_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, RescopeSecond(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4DB BY DEF A4Hyp, A4Inv
<1>sh. RescopeUnciteFirst /\ WidenKeepsLocal /\ UnlinkChecksBytes BY ShippedShape DEF Shipped
<1>w. PICK wfail \in SUBSET {p \in RescopeFetch0(s) : RescopeLocal1(s)[p] = Nil \/ ~WidenKeepsLocal} :
        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)]
  BY DEF RescopeSecond
<1>s. w'[s] = RescopeSecondW(s, wfail) BY <1>0, <1>w DEF TypeOK
<1>u. doc' = doc /\ gw' = gw /\ minted \subseteq minted' /\ orig' = orig BY DEF RescopeSecond, bucket
<1>ck. A3CK BY <1>u, A3CF
<1>st. w[s].sStage = "mid" /\ w[s].pc = "idle" BY DEF RescopeSecond
<1>ty. w[s].local \in [Paths -> Opt(Handles)] /\ w[s].baseline \in [Paths -> Opt(Handles)] BY <1>ic, TreeTy, WriterFields DEF IndC
<1>g. /\ RescopeSecondW(s, wfail).decl = w[s].decl /\ RescopeSecondW(s, wfail).dset = w[s].dset /\ RescopeSecondW(s, wfail).moved = w[s].moved /\ RescopeSecondW(s, wfail).pend = w[s].pend
  BY DEF RescopeSecondW
<1>c. /\ RescopeSecondW(s, wfail).car.id = w[s].car.id /\ RescopeSecondW(s, wfail).car.paths = w[s].car.paths /\ RescopeSecondW(s, wfail).car.deletes = w[s].car.deletes
      /\ RescopeSecondW(s, wfail).car.dropped = w[s].car.dropped /\ RescopeSecondW(s, wfail).car.pub = w[s].car.pub
  BY DEF RescopeSecondW, CarUncited
<1>d. A4Dcl(RescopeSecondW(s, wfail), w[s]) BY <1>g, <1>c DEF A4Dcl
<1>l1. \A q \in Paths : /\ RescopeLocal1(s)[q] = IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]
                        /\ RescopeBase1(s)[q] = w[s].baseline[q]
  BY <1>sh, <1>ty DEF RescopeLocal1, RescopeBase1
<1>r. \A q \in Paths :
        /\ RescopeSecondW(s, wfail).local[q] = IF q \in RescopeFetch(s, wfail) /\ RescopeLocal1(s)[q] = Nil
                                               THEN doc[q] ELSE RescopeLocal1(s)[q]
        /\ RescopeSecondW(s, wfail).baseline[q] = IF q \in RescopeFetch(s, wfail) THEN doc[q] ELSE RescopeBase1(s)[q]
  BY <1>sh DEF RescopeSecondW
\* the leftover is unlinked
<1>lu. \A q \in A1LeftSet(w[s]) : RescopeUnlink(s, q) BY <1>st, <1>sh DEF A1LeftSet, RescopeUnlink, RescopeDrop
<1>fa. \A q \in Paths : q \in RescopeFetch(s, wfail) => w[s].baseline[q] = Nil
  BY <1>l1 DEF RescopeFetch, RescopeFetch0, RescopeAdd
<1>sx. \A q \in Paths : A4Shr(RescopeSecondW(s, wfail), w[s], q)
  <2>. SUFFICES ASSUME NEW q \in Paths PROVE A4Shr(RescopeSecondW(s, wfail), w[s], q) OBVIOUS
  <2>1. CASE RescopeUnlink(s, q) BY <2>1, <1>l1, <1>r DEF A4Shr
  <2>2. CASE ~RescopeUnlink(s, q)
    <3>1. q \notin A1LeftSet(w[s]) BY <2>2, <1>lu
    <3>2. RescopeLocal1(s)[q] = w[s].local[q] BY <2>2, <1>l1
    <3>3. CASE q \in RescopeFetch(s, wfail) /\ w[s].local[q] = Nil BY <3>2, <3>3, <1>r DEF A4Shr
    <3>4. CASE q \in RescopeFetch(s, wfail) /\ w[s].local[q] # Nil
      <4>1. w[s].baseline[q] = Nil BY <3>4, <1>fa
      <4>. QED BY <3>1, <3>2, <3>4, <4>1, <1>r DEF A4Shr
    <3>5. CASE q \notin RescopeFetch(s, wfail) BY <3>1, <3>2, <3>5, <1>r, <1>l1 DEF A4Shr
    <3>. QED BY <3>3, <3>4, <3>5
  <2>. QED BY <2>1, <2>2
<1>dd. A4D(s, RescopeSecondW(s, wfail)) BY <1>s, <1>sx, <1>ck, <1>ic, A4ShrD
<1>up. A4U(s, RescopeSecondW(s, wfail))
  <2>1. A4Upl(RescopeSecondW(s, wfail), w[s]) BY DEF RescopeSecondW, A4Upl
  <2>. QED BY <2>1, <1>ck, <1>ic, <1>s, A4UplU
<1>p. A4P(s, RescopeSecondW(s, wfail)) /\ A4G(s, RescopeSecondW(s, wfail)) BY <1>s, <1>d, <1>ck, <1>ic, A4DclPG
<1>b. A4B(s, RescopeSecondW(s, wfail)) BY <1>st DEF RescopeSecondW, A4B, Bar
<1>. QED BY <1>w, <1>u, <1>dd, <1>up, <1>p, <1>b, A4OneEq

------------------------------------------------------------------------------
(* Every tree keeps the fields the facts read (Depose: two trees).          *)

LEMMA A4All ==
  ASSUME A4Hyp, doc' = doc, gw' = gw, minted' = minted, orig' = orig,
         \A u \in Writers : /\ A4Tree(w'[u], w[u]) /\ A4Dcl(w'[u], w[u])
                            /\ w'[u].uploads = w[u].uploads /\ w'[u].snap = w[u].snap
                            /\ A4Brr(w'[u], w[u])
  PROVE  A4Inv'
<1>ck. A3CK BY A3CF
<1>ic. IndC /\ A1IAK1 /\ A4Inv BY DEF A4Hyp
<1>G. \A t \in Writers, j \in Paths :
        /\ A4Dirt(t, j)' <=> A4Dirt(t, j)
        /\ Content(w[t].local[j])' = Content(w[t].local[j]) /\ Content(w[t].snap[j])' = Content(w[t].snap[j])
        /\ Content(w[t].decl[j])' = Content(w[t].decl[j]) /\ w'[t].uploads = w[t].uploads
        /\ A1Priv(t, j)' => A1Priv(t, j)
  <2>. SUFFICES ASSUME NEW t \in Writers, NEW j \in Paths
                PROVE  /\ A4Dirt(t, j)' <=> A4Dirt(t, j)
                       /\ Content(w[t].local[j])' = Content(w[t].local[j]) /\ Content(w[t].snap[j])' = Content(w[t].snap[j])
                       /\ Content(w[t].decl[j])' = Content(w[t].decl[j]) /\ w'[t].uploads = w[t].uploads
                       /\ A1Priv(t, j)' => A1Priv(t, j)
    OBVIOUS
  <2>0. A4Tree(w'[t], w[t]) /\ A4Dcl(w'[t], w[t]) /\ w'[t].uploads = w[t].uploads /\ w'[t].snap = w[t].snap OBVIOUS
  <2>1. A4Dirt(t, j)' <=> A4Dirt(t, j) BY <2>0 DEF A4Dirt, A4Tree
  <2>2. PICK h1 \in Opt(minted) : h1 = w[t].local[j] BY <1>ic, A4Ty
  <2>3. PICK h2 \in Opt(minted) : h2 = w[t].snap[j] BY <1>ic, A4Ty
  <2>4. PICK h3 \in Opt(minted) : h3 = w[t].decl[j] BY <1>ic, A4Ty
  <2>5. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3) BY <2>2, <2>3, <2>4, <1>ck DEF A3CK
  <2>6. A1Priv(t, j)' => A1Priv(t, j) BY <2>0, <1>ck, <1>ic, A4PrivSame
  <2>. QED BY <2>0, <2>1, <2>2, <2>3, <2>4, <2>5, <2>6 DEF A4Tree, A4Dcl
<1>du. A4DU'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW s2 \in Writers, NEW q \in Paths, NEW k \in Paths,
                       u # s2, A4Dirt(u, q)'
                PROVE  /\ A4Dirt(s2, k)' => (Content(w[s2].local[k]) # Content(w[u].local[q]))'
                       /\ k \in w'[s2].uploads => (Content(w[s2].snap[k]) # Content(w[u].local[q]))'
    BY DEF A4DU
  <2>. QED BY <1>G DEF A4Hyp, A4Inv, A4DU
<1>gw. A4Gw' BY <1>G DEF A4Hyp, A4Inv, A4Gw, A4Dcl
<1>db. A4DB' BY DEF A4Hyp, A4Inv, A4DB, A4Brr, A4Tree
<1>dp. A1DeclPriv'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths, A1Priv(u, q)'
                PROVE  /\ (\A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q]))'
                       /\ (\A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q]))'
                       /\ (\A s2 \in Writers \ {u}, k \in Paths :
                             k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q]))'
    BY DEF A1DeclPriv
  <2>1. A1Priv(u, q) BY <1>G
  <2>2. /\ \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q])
        /\ \A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q])
        /\ \A s2 \in Writers \ {u}, k \in Paths : k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q])
    BY <2>1 DEF A4Hyp, A4Inv, A1DeclPriv
  <2>3. \A k \in Paths : Content(doc[k])' = Content(doc[k]) /\ Content(gw[k])' = Content(gw[k])
    <3>. SUFFICES ASSUME NEW k \in Paths PROVE Content(doc[k])' = Content(doc[k]) /\ Content(gw[k])' = Content(gw[k]) OBVIOUS
    <3>1. PICK hd \in Opt(minted) : hd = doc[k] BY <1>ic, A4Ty
    <3>2. PICK hg \in Opt(minted) : hg = gw[k] BY <1>ic, A4Ty
    <3>. QED BY <3>1, <3>2, <1>ck DEF A3CK
  <2>. QED BY <2>2, <2>3, <1>G
<1>. QED BY <1>du, <1>gw, <1>db, <1>dp DEF A4Inv

LEMMA Depose_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Depose(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>1. /\ holder \in Writers /\ holder # s /\ w[s].pc = "scanned"
      /\ w' = [w EXCEPT ![s] = DeposeW(s), ![holder] = DeposedW(holder)]
      /\ doc' = doc /\ gw' = gw /\ minted' = minted /\ orig' = orig
  BY DEF Depose, aux
<1>w. \A u \in Writers : w'[u] = IF u = holder THEN DeposedW(holder) ELSE IF u = s THEN DeposeW(s) ELSE w[u]
  BY <1>0, <1>1 DEF TypeOK
<1>2. \A u \in Writers : /\ A4Tree(w'[u], w[u]) /\ A4Dcl(w'[u], w[u])
                         /\ w'[u].uploads = w[u].uploads /\ w'[u].snap = w[u].snap
                         /\ A4Brr(w'[u], w[u])
  <2>. SUFFICES ASSUME NEW u \in Writers
                PROVE  /\ A4Tree(w'[u], w[u]) /\ A4Dcl(w'[u], w[u])
                       /\ w'[u].uploads = w[u].uploads /\ w'[u].snap = w[u].snap
                       /\ A4Brr(w'[u], w[u])
    OBVIOUS
  <2>1. CASE u = holder
    <3>1. /\ DeposedW(holder).local = w[holder].local /\ DeposedW(holder).baseline = w[holder].baseline
          /\ DeposedW(holder).sStage = w[holder].sStage /\ DeposedW(holder).sDrop = w[holder].sDrop
          /\ DeposedW(holder).sKeep = w[holder].sKeep /\ DeposedW(holder).sHeld = w[holder].sHeld
          /\ DeposedW(holder).decl = w[holder].decl /\ DeposedW(holder).dset = w[holder].dset
          /\ DeposedW(holder).moved = w[holder].moved /\ DeposedW(holder).pend = w[holder].pend
          /\ DeposedW(holder).car = w[holder].car /\ DeposedW(holder).uploads = w[holder].uploads
          /\ DeposedW(holder).snap = w[holder].snap /\ DeposedW(holder).pc = w[holder].pc
          /\ DeposedW(holder).deletes = w[holder].deletes
      BY DEF DeposedW
    <3>2. w'[u] = DeposedW(holder) BY <2>1, <1>w
    <3>. QED BY <2>1, <3>1, <3>2 DEF A4Tree, A1LeftSet, A4Dcl, A4Brr
  <2>2. CASE u = s
    <3>1. /\ DeposeW(s).local = w[s].local /\ DeposeW(s).baseline = w[s].baseline
          /\ DeposeW(s).sStage = w[s].sStage /\ DeposeW(s).sDrop = w[s].sDrop
          /\ DeposeW(s).sKeep = w[s].sKeep /\ DeposeW(s).sHeld = w[s].sHeld
          /\ DeposeW(s).decl = w[s].decl /\ DeposeW(s).dset = w[s].dset
          /\ DeposeW(s).moved = w[s].moved /\ DeposeW(s).pend = w[s].pend
          /\ DeposeW(s).car = w[s].car /\ DeposeW(s).uploads = w[s].uploads
          /\ DeposeW(s).snap = w[s].snap /\ DeposeW(s).deletes = w[s].deletes
      BY DEF DeposeW
    <3>2. w'[u] = DeposeW(s) BY <2>2, <1>1, <1>w
    <3>. QED BY <2>2, <3>1, <3>2, <1>1 DEF A4Tree, A1LeftSet, A4Dcl, A4Brr, Bar
  <2>3. CASE u # holder /\ u # s
    <3>1. w'[u] = w[u] BY <2>3, <1>w
    <3>. QED BY <3>1 DEF A4Tree, A4Dcl, A4Brr
  <2>. QED BY <2>1, <2>2, <2>3
<1>. QED BY <1>1, <1>2, A4All

------------------------------------------------------------------------------
(* Install: the document takes the barrier's result.  Another tree's private *)
(* bytes are in no upload of s (DeclPriv (c)); s's own declared files end in *)
(* carrier form (Cased', a withheld one in the carrier's dropped set) unless *)
(* the install changed nothing.                                             *)

LEMMA Install_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, Install(s), Frame
  PROVE  A4Inv'
<1>0. TypeOK BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK
<1>ic. IndC /\ A1IAK1 /\ A4Inv BY DEF A4Hyp
<1>1. /\ w' = [w EXCEPT ![s] = InstallW(s)] /\ doc' = InstallInst(s)
      /\ gw' = gw /\ minted' = minted /\ orig' = orig
  BY DEF Install, aux
<1>w. \A u \in Writers : w'[u] = IF u = s THEN InstallW(s) ELSE w[u] BY <1>0, <1>1 DEF TypeOK
<1>ck. A3CK BY <1>1, A3CF
<1>g0. w[s].pc = "claimed" BY DEF Install
<1>z. w[s].cId = w[s].pend.id BY <1>g0 DEF A4Hyp, A1IAK1, A1Frozen
<1>fx. /\ InstallW(s).local = w[s].local /\ InstallW(s).baseline = w[s].baseline
       /\ InstallW(s).sStage = w[s].sStage /\ InstallW(s).sDrop = w[s].sDrop
       /\ InstallW(s).sKeep = w[s].sKeep /\ InstallW(s).sHeld = w[s].sHeld
       /\ InstallW(s).decl = w[s].decl /\ InstallW(s).dset = w[s].dset
       /\ InstallW(s).moved = w[s].moved /\ InstallW(s).pend = w[s].pend
       /\ InstallW(s).uploads = w[s].uploads /\ InstallW(s).snap = w[s].snap
       /\ InstallW(s).deletes = w[s].deletes /\ InstallW(s).gone = w[s].gone
       /\ InstallW(s).cId = w[s].cId /\ InstallW(s).pc = "cased"
  BY DEF InstallW
<1>fc. InstallW(s).car = IF Carries(s) /\ ~InstallNothing(s) THEN InstallCar(s) ELSE w[s].car BY DEF InstallW
<1>tp. w[s].gone \subseteq w[s].uploads BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, M1, Mine
<1>icar. /\ InstallCar(s).id = w[s].cId
         /\ w[s].gone \subseteq InstallCar(s).dropped /\ w[s].uploads \subseteq InstallCar(s).paths
  BY DEF InstallCar, InstallCar0
<1>ii. \A k \in Paths : \/ InstallInst(s)[k] = doc[k] \/ InstallInst(s)[k] = Nil
                        \/ k \in w[s].uploads /\ InstallInst(s)[k] = w[s].snap[k]
  BY DEF InstallInst, InstallMine
\* s declares nothing private after a carrying install that changed the document
<1>np. Carries(s) /\ ~InstallNothing(s) => \A q \in Paths : ~A1Priv(s, q)'
  <2>. SUFFICES ASSUME Carries(s), ~InstallNothing(s), NEW q \in Paths, A1Priv(s, q)' PROVE FALSE OBVIOUS
  <2>1. w'[s] = InstallW(s) BY <1>w
  <2>2. A1Decl(s, q)' /\ w'[s].decl[q] # Nil BY DEF A1Priv
  <2>3. w'[s].pc = "cased" /\ w'[s].cId # 0 BY <2>1, <1>fx DEF Carries
  <2>4. \/ w'[s].car.id = w'[s].pend.id /\ CarForm(s, q, w[s].car)'
        \/ q \in w'[s].gone /\ q \notin w'[s].upDone
    BY <2>2, <2>3 DEF A4Hyp, A1IAK1, A1Cased, Carries
  <2>5. PICK c1 : c1 = InstallCar(s) OBVIOUS
  <2>6. w'[s].car = c1 BY <2>1, <2>5, <1>fc
  <2>7. CASE q \in w'[s].gone
    <3>1. q \in c1.paths /\ q \in c1.dropped BY <2>7, <2>1, <2>5, <1>fx, <1>tp, <1>icar
    <3>2. CarForm(s, q, c1)' BY <3>1 DEF CarForm
    <3>3. c1.id = w'[s].pend.id BY <2>5, <2>1, <1>icar, <1>z, <1>fx
    <3>. QED BY <3>2, <3>3, <2>6 DEF A1Priv
  <2>8. CASE q \notin w'[s].gone BY <2>4, <2>8 DEF A1Priv
  <2>. QED BY <2>7, <2>8
\* s's private declarations: none, or (nothing installed) the old ones
<1>sp. \A q \in Paths : A1Priv(s, q)' => InstallNothing(s) /\ A1Priv(s, q)
  <2>. SUFFICES ASSUME NEW q \in Paths, A1Priv(s, q)' PROVE InstallNothing(s) /\ A1Priv(s, q) OBVIOUS
  <2>1. Carries(s)
    <3>1. w'[s].pend.id # 0 BY DEF A1Priv, A1Decl
    <3>. QED BY <3>1, <1>w, <1>fx, <1>z DEF Carries
  <2>2. InstallNothing(s) BY <2>1, <1>np
  <2>3. A4Dcl(InstallW(s), w[s]) BY <2>2, <1>fx, <1>fc DEF A4Dcl
  <2>. QED BY <2>2, <2>3, <1>w, <1>ck, <1>ic, A4PrivSame
<1>G. \A t \in Writers, j \in Paths :
        /\ A4Dirt(t, j)' <=> A4Dirt(t, j)
        /\ Content(w[t].local[j])' = Content(w[t].local[j]) /\ Content(w[t].snap[j])' = Content(w[t].snap[j])
        /\ Content(w[t].decl[j])' = Content(w[t].decl[j]) /\ w'[t].uploads = w[t].uploads
        /\ w'[t].decl = w[t].decl /\ (w'[t].pc \in Bar => w[t].pc \in Bar)
        /\ w'[t].deletes = w[t].deletes /\ w'[t].local = w[t].local /\ w'[t].baseline = w[t].baseline
  <2>. SUFFICES ASSUME NEW t \in Writers, NEW j \in Paths
                PROVE  /\ A4Dirt(t, j)' <=> A4Dirt(t, j)
                       /\ Content(w[t].local[j])' = Content(w[t].local[j]) /\ Content(w[t].snap[j])' = Content(w[t].snap[j])
                       /\ Content(w[t].decl[j])' = Content(w[t].decl[j]) /\ w'[t].uploads = w[t].uploads
                       /\ w'[t].decl = w[t].decl /\ (w'[t].pc \in Bar => w[t].pc \in Bar)
                       /\ w'[t].deletes = w[t].deletes /\ w'[t].local = w[t].local /\ w'[t].baseline = w[t].baseline
    OBVIOUS
  <2>0. /\ w'[t].local = w[t].local /\ w'[t].baseline = w[t].baseline /\ w'[t].sStage = w[t].sStage
        /\ w'[t].sDrop = w[t].sDrop /\ w'[t].sKeep = w[t].sKeep /\ w'[t].sHeld = w[t].sHeld
        /\ w'[t].decl = w[t].decl /\ w'[t].uploads = w[t].uploads /\ w'[t].snap = w[t].snap
        /\ w'[t].deletes = w[t].deletes /\ (w'[t].pc \in Bar => w[t].pc \in Bar)
    BY <1>w, <1>fx, <1>g0 DEF Bar
  <2>1. A4Dirt(t, j)' <=> A4Dirt(t, j) BY <2>0 DEF A4Dirt, A1LeftSet
  <2>2. PICK h1 \in Opt(minted) : h1 = w[t].local[j] BY <1>ic, A4Ty
  <2>3. PICK h2 \in Opt(minted) : h2 = w[t].snap[j] BY <1>ic, A4Ty
  <2>4. PICK h3 \in Opt(minted) : h3 = w[t].decl[j] BY <1>ic, A4Ty
  <2>5. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3) BY <2>2, <2>3, <2>4, <1>ck DEF A3CK
  <2>. QED BY <2>0, <2>1, <2>2, <2>3, <2>4, <2>5
<1>op. \A t \in Writers \ {s}, j \in Paths : A1Priv(t, j)' => A1Priv(t, j)
  <2>. SUFFICES ASSUME NEW t \in Writers \ {s}, NEW j \in Paths, A1Priv(t, j)' PROVE A1Priv(t, j) OBVIOUS
  <2>1. w'[t] = w[t] BY <1>w
  <2>2. A4Dcl(w[t], w[t]) BY DEF A4Dcl
  <2>. QED BY <2>1, <2>2, <1>ck, <1>ic, A4PrivSame
<1>du. A4DU'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW s2 \in Writers, NEW q \in Paths, NEW k \in Paths,
                       u # s2, A4Dirt(u, q)'
                PROVE  /\ A4Dirt(s2, k)' => (Content(w[s2].local[k]) # Content(w[u].local[q]))'
                       /\ k \in w'[s2].uploads => (Content(w[s2].snap[k]) # Content(w[u].local[q]))'
    BY DEF A4DU
  <2>. QED BY <1>G DEF A4Hyp, A4Inv, A4DU
<1>gw. A4Gw' BY <1>1, <1>G DEF A4Hyp, A4Inv, A4Gw
<1>db. A4DB' BY <1>G DEF A4Hyp, A4Inv, A4DB
<1>dp. A1DeclPriv'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths, A1Priv(u, q)'
                PROVE  /\ (\A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q]))'
                       /\ (\A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q]))'
                       /\ (\A s2 \in Writers \ {u}, k \in Paths :
                             k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q]))'
    BY DEF A1DeclPriv
  <2>1. A1Priv(u, q) /\ (u = s => InstallNothing(s)) BY <1>sp, <1>op
  <2>2. /\ \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q])
        /\ \A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q])
        /\ \A s2 \in Writers \ {u}, k \in Paths : k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q])
    BY <2>1 DEF A4Hyp, A4Inv, A1DeclPriv
  <2>3. \A k \in Paths : Content(gw[k])' = Content(gw[k]) /\ Content(w[s].snap[k])' = Content(w[s].snap[k])
    <3>. SUFFICES ASSUME NEW k \in Paths
                  PROVE  Content(gw[k])' = Content(gw[k]) /\ Content(w[s].snap[k])' = Content(w[s].snap[k])
      OBVIOUS
    <3>2. PICK hg \in Opt(minted) : hg = gw[k] BY <1>ic, A4Ty
    <3>3. PICK hs \in Opt(minted) : hs = w[s].snap[k] BY <1>ic, A4Ty
    <3>4. Content(hg)' = Content(hg) /\ Content(hs)' = Content(hs) BY <3>2, <3>3, <1>ck DEF A3CK
    <3>. QED BY <3>2, <3>3, <3>4, <1>1, <1>G
  <2>4. Content(w[u].decl[q])' = Content(w[u].decl[q]) BY <1>G
  <2>a. (\A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q]))'
    <3>. SUFFICES ASSUME NEW k \in Paths, doc'[k] # Nil
                  PROVE  Content(doc[k])' # Content(w[u].decl[q])' OBVIOUS
    <3>0. PICK hd \in Opt(minted) : hd = doc[k] BY <1>ic, A4Ty
    <3>00. Content(hd)' = Content(hd) BY <3>0, <1>ck DEF A3CK
    <3>1. CASE u = s
      <4>1. doc' = doc BY <3>1, <2>1, <1>1 DEF InstallNothing
      <4>. QED BY <4>1, <3>0, <3>00, <2>2, <2>4
    <3>2. CASE u # s
      <4>1. CASE doc'[k] = doc[k] BY <4>1, <3>0, <3>00, <2>2, <2>4
      <4>2. CASE doc'[k] # doc[k]
        <5>1. k \in w[s].uploads /\ doc'[k] = w[s].snap[k] BY <4>2, <1>ii, <1>1
        <5>2. Content(w[s].snap[k]) # Content(w[u].decl[q]) BY <5>1, <3>2, <2>2
        <5>3. PICK hs \in Opt(minted) : hs = w[s].snap[k] BY <1>ic, A4Ty
        <5>4. Content(hs)' = Content(hs) BY <5>3, <1>ck DEF A3CK
        <5>. QED BY <5>1, <5>2, <5>3, <5>4, <2>4
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <3>1, <3>2
  <2>b. (\A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q]))' BY <2>2, <2>3, <2>4, <1>1
  <2>c. (\A s2 \in Writers \ {u}, k \in Paths : k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q]))'
    BY <2>2, <2>4, <1>G
  <2>. QED BY <2>a, <2>b, <2>c
<1>. QED BY <1>du, <1>gw, <1>db, <1>dp DEF A4Inv

------------------------------------------------------------------------------
(* The gateway and the quiet steps: no tree written.  A document path takes *)
(* a document path's bytes or a save's; a save is kept, cleared or fresh.  *)

LEMMA A4Gate ==
  ASSUME A4Hyp, w' = w, orig' = orig, minted \subseteq minted',
         \A k \in Paths : gw'[k] = Nil \/ gw'[k] = gw[k] \/ gw'[k] \in Handles \ minted,
         \A k \in Paths : \/ doc'[k] = Nil \/ \E j \in Paths : doc'[k] = doc[j]
                          \/ \E j \in Paths : gw[j] # Nil /\ doc'[k] = gw[j]
  PROVE  A4Inv'
<1>ck. A3CK BY A3CF
<1>ic. IndC /\ A1IAK1 /\ A4Inv BY DEF A4Hyp
<1>hn. \A h \in Handles \ minted : orig[h] = Nil BY DEF A4Hyp, IndC, IndM5, M5, Hist, HistNew
<1>hm. \A h \in minted : orig[h] \in Opt(minted) BY DEF A4Hyp, IndC, IndM5, M5, Hist, HistMinted
<1>mh. minted' \subseteq Handles BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>nh. Nil \notin Handles BY NilHandle
<1>G. \A t \in Writers, j \in Paths :
        /\ A4Dirt(t, j)' <=> A4Dirt(t, j)
        /\ Content(w[t].local[j])' = Content(w[t].local[j]) /\ Content(w[t].snap[j])' = Content(w[t].snap[j])
        /\ Content(w[t].decl[j])' = Content(w[t].decl[j])
        /\ A1Priv(t, j)' => A1Priv(t, j)
  <2>. SUFFICES ASSUME NEW t \in Writers, NEW j \in Paths
                PROVE  /\ A4Dirt(t, j)' <=> A4Dirt(t, j)
                       /\ Content(w[t].local[j])' = Content(w[t].local[j]) /\ Content(w[t].snap[j])' = Content(w[t].snap[j])
                       /\ Content(w[t].decl[j])' = Content(w[t].decl[j])
                       /\ A1Priv(t, j)' => A1Priv(t, j)
    OBVIOUS
  <2>1. A4Dirt(t, j)' <=> A4Dirt(t, j) BY DEF A4Dirt, A1LeftSet
  <2>2. PICK h1 \in Opt(minted) : h1 = w[t].local[j] BY <1>ic, A4Ty
  <2>3. PICK h2 \in Opt(minted) : h2 = w[t].snap[j] BY <1>ic, A4Ty
  <2>4. PICK h3 \in Opt(minted) : h3 = w[t].decl[j] BY <1>ic, A4Ty
  <2>5. Content(h1)' = Content(h1) /\ Content(h2)' = Content(h2) /\ Content(h3)' = Content(h3) BY <2>2, <2>3, <2>4, <1>ck DEF A3CK
  <2>6. A4Dcl(w[t], w[t]) BY DEF A4Dcl
  <2>7. A1Priv(t, j)' => A1Priv(t, j) BY <2>6, <1>ck, <1>ic, A4PrivSame
  <2>. QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>7
\* a document path's or save's old bytes, kept
<1>du. A4DU'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW s2 \in Writers, NEW q \in Paths, NEW k \in Paths,
                       u # s2, A4Dirt(u, q)'
                PROVE  /\ A4Dirt(s2, k)' => (Content(w[s2].local[k]) # Content(w[u].local[q]))'
                       /\ k \in w'[s2].uploads => (Content(w[s2].snap[k]) # Content(w[u].local[q]))'
    BY DEF A4DU
  <2>. QED BY <1>G DEF A4Hyp, A4Inv, A4DU
<1>db. A4DB' BY DEF A4Hyp, A4Inv, A4DB
<1>gw. A4Gw'
  <2>. SUFFICES ASSUME NEW p \in Paths, gw'[p] # Nil
                PROVE  /\ orig'[gw'[p]] = Nil
                       /\ \A h \in minted' : orig'[h] # gw'[p]
                       /\ \A t \in Writers, q \in Paths : w'[t].decl[q] # gw'[p]
    BY DEF A4Gw
  <2>d. \A t \in Writers, q \in Paths : w[t].decl[q] \in Opt(minted) BY <1>ic, A4Ty
  <2>1. CASE gw'[p] = gw[p]
    <3>0. /\ orig[gw[p]] = Nil /\ \A h \in minted : orig[h] # gw[p]
          /\ \A t \in Writers, q \in Paths : w[t].decl[q] # gw[p]
      BY <2>1 DEF A4Hyp, A4Inv, A4Gw
    <3>1. \A h \in minted' : orig'[h] # gw'[p]
      <4>. SUFFICES ASSUME NEW h \in minted' PROVE orig[h] # gw[p] BY <2>1
      <4>1. CASE h \in minted BY <4>1, <3>0
      <4>2. CASE h \notin minted BY <4>2, <1>mh, <1>hn, <2>1
      <4>. QED BY <4>1, <4>2
    <3>. QED BY <3>0, <3>1, <2>1
  <2>2. CASE gw'[p] # gw[p]
    <3>0. gw'[p] \in Handles \ minted BY <2>2
    <3>1. orig[gw'[p]] = Nil BY <3>0, <1>hn
    <3>2. \A h \in minted' : orig[h] # gw'[p]
      <4>. SUFFICES ASSUME NEW h \in minted' PROVE orig[h] # gw'[p] OBVIOUS
      <4>1. CASE h \in minted BY <4>1, <1>hm, <3>0, <1>nh DEF Opt
      <4>2. CASE h \notin minted BY <4>2, <1>mh, <1>hn, <3>0
      <4>. QED BY <4>1, <4>2
    <3>3. \A t \in Writers, q \in Paths : w[t].decl[q] # gw'[p] BY <2>d, <3>0, <1>nh DEF Opt
    <3>. QED BY <3>1, <3>2, <3>3
  <2>. QED BY <2>1, <2>2
<1>dp. A1DeclPriv'
  <2>. SUFFICES ASSUME NEW u \in Writers, NEW q \in Paths, A1Priv(u, q)'
                PROVE  /\ (\A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q]))'
                       /\ (\A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q]))'
                       /\ (\A s2 \in Writers \ {u}, k \in Paths :
                             k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q]))'
    BY DEF A1DeclPriv
  <2>1. A1Priv(u, q) BY <1>G
  <2>2. /\ \A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q])
        /\ \A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q])
        /\ \A s2 \in Writers \ {u}, k \in Paths : k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q])
    BY <2>1 DEF A4Hyp, A4Inv, A1DeclPriv
  <2>3. Content(w[u].decl[q])' = Content(w[u].decl[q]) BY <1>G
  <2>4. w[u].decl[q] \in minted BY <2>1, <1>ic, A4Ty DEF A1Priv, Opt
  <2>5. Content(w[u].decl[q]) \in minted BY <2>4, <1>ic, A4CM DEF Opt
  \* the old document's and saves' bytes, kept
  <2>6. \A j \in Paths : (doc[j] # Nil => Content(doc[j]) # Content(w[u].decl[q]))
                         /\ (gw[j] # Nil => Content(gw[j]) # Content(w[u].decl[q])) BY <2>2
  <2>a. (\A k \in Paths : doc[k] = Nil \/ Content(doc[k]) # Content(w[u].decl[q]))'
    <3>. SUFFICES ASSUME NEW k \in Paths, doc'[k] # Nil
                  PROVE  Content(doc[k])' # Content(w[u].decl[q])' OBVIOUS
    <3>1. CASE \E j \in Paths : doc'[k] = doc[j]
      <4>1. PICK j \in Paths : doc'[k] = doc[j] BY <3>1
      <4>2. PICK hd \in Opt(minted) : hd = doc[j] BY <1>ic, A4Ty
      <4>3. Content(hd)' = Content(hd) BY <4>2, <1>ck DEF A3CK
      <4>. QED BY <4>1, <4>2, <4>3, <2>6, <2>3
    <3>2. CASE \E j \in Paths : gw[j] # Nil /\ doc'[k] = gw[j]
      <4>1. PICK j \in Paths : gw[j] # Nil /\ doc'[k] = gw[j] BY <3>2
      <4>2. PICK hg \in Opt(minted) : hg = gw[j] BY <1>ic, A4Ty
      <4>3. Content(hg)' = Content(hg) BY <4>2, <1>ck DEF A3CK
      <4>. QED BY <4>1, <4>2, <4>3, <2>6, <2>3
    <3>. QED BY <3>1, <3>2
  <2>b. (\A k \in Paths : gw[k] = Nil \/ Content(gw[k]) # Content(w[u].decl[q]))'
    <3>. SUFFICES ASSUME NEW k \in Paths, gw'[k] # Nil
                  PROVE  Content(gw[k])' # Content(w[u].decl[q])' OBVIOUS
    <3>1. CASE gw'[k] = gw[k]
      <4>2. PICK hg \in Opt(minted) : hg = gw[k] BY <1>ic, A4Ty
      <4>3. Content(hg)' = Content(hg) BY <4>2, <1>ck DEF A3CK
      <4>. QED BY <3>1, <4>2, <4>3, <2>6, <2>3
    <3>2. CASE gw'[k] # gw[k]
      <4>1. gw'[k] \in Handles \ minted BY <3>2
      <4>2. Content(gw[k])' = gw'[k] BY <4>1, <1>hn DEF Content
      <4>. QED BY <4>1, <4>2, <2>5, <2>3
    <3>. QED BY <3>1, <3>2
  <2>c. (\A s2 \in Writers \ {u}, k \in Paths : k \in w[s2].uploads => Content(w[s2].snap[k]) # Content(w[u].decl[q]))'
    BY <2>2, <2>3, <1>G
  <2>. QED BY <2>a, <2>b, <2>c
<1>. QED BY <1>du, <1>db, <1>gw, <1>dp DEF A4Inv

LEMMA A4Quiet ==
  ASSUME A4Hyp, w' = w, orig' = orig, minted' = minted, doc' = doc, gw' = gw
  PROVE  A4Inv'
BY A4Gate

LEMMA GPut_A4 ==
  ASSUME A4Hyp, NEW p \in Paths, GPut(p), Frame
  PROVE  A4Inv'
<1>g. nextGen \in Nat /\ nextGen > Seed BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>h. PICK h \in Handles : h = <<p, nextGen>> BY <1>g, MaxMintNat, MaxCopiesNat DEF GPut, Handles, Gens, Seed
<1>4. h \notin minted
  <2>1. \A m \in minted : Gen(m) < nextGen \/ Gen(m) > MaxMint BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, M2, Fresh
  <2>2. nextGen <= MaxMint BY DEF GPut
  <2>. QED BY <2>1, <2>2, <1>g, <1>h, MaxMintNat DEF Gen
<1>1. /\ w' = w /\ orig' = orig /\ minted' = minted \cup {h} /\ doc' = doc
      /\ gw' = [gw EXCEPT ![p] = h]
  BY <1>h DEF GPut
<1>ty. gw \in [Paths -> Opt(Handles)] BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>2. \A k \in Paths : gw'[k] = Nil \/ gw'[k] = gw[k] \/ gw'[k] \in Handles \ minted BY <1>1, <1>ty, <1>4
<1>3. \A k \in Paths : \/ doc'[k] = Nil \/ \E j \in Paths : doc'[k] = doc[j]
                       \/ \E j \in Paths : gw[j] # Nil /\ doc'[k] = gw[j]
  BY <1>1
<1>. QED BY <1>1, <1>2, <1>3, A4Gate

LEMMA GCas_A4 ==
  ASSUME A4Hyp, NEW p \in Paths, GCas(p), Frame
  PROVE  A4Inv'
<1>ty. gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)]
  BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>1. /\ w' = w /\ orig' = orig /\ minted' = minted /\ gw[p] # Nil
      /\ gw' = [gw EXCEPT ![p] = Nil]
      /\ \/ doc' = doc
         \/ doc' = [doc EXCEPT ![p] = gw[p]]
  BY DEF GCas, aux
<1>2. \A k \in Paths : gw'[k] = Nil \/ gw'[k] = gw[k] \/ gw'[k] \in Handles \ minted BY <1>1, <1>ty
<1>3. \A k \in Paths : \/ doc'[k] = Nil \/ \E j \in Paths : doc'[k] = doc[j]
                       \/ \E j \in Paths : gw[j] # Nil /\ doc'[k] = gw[j]
  BY <1>1, <1>ty
<1>. QED BY <1>1, <1>2, <1>3, A4Gate

LEMMA GDelete_A4 ==
  ASSUME A4Hyp, NEW p \in Paths, GDelete(p), Frame
  PROVE  A4Inv'
<1>ty. doc \in [Paths -> Opt(Handles)] BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>1. w' = w /\ orig' = orig /\ minted' = minted /\ gw' = gw /\ doc' = [doc EXCEPT ![p] = Nil] BY DEF GDelete, aux
<1>3. \A k \in Paths : \/ doc'[k] = Nil \/ \E j \in Paths : doc'[k] = doc[j]
                       \/ \E j \in Paths : gw[j] # Nil /\ doc'[k] = gw[j]
  BY <1>1, <1>ty
<1>. QED BY <1>1, <1>3, A4Gate

LEMMA GRename_A4 ==
  ASSUME A4Hyp, NEW p \in Paths, NEW q \in Paths, GRename(p, q), Frame
  PROVE  A4Inv'
<1>ty. doc \in [Paths -> Opt(Handles)] BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>ra. RenameAtomic BY ShippedShape DEF Shipped
<1>1. w' = w /\ orig' = orig /\ minted' = minted /\ gw' = gw /\ doc' = [doc EXCEPT ![q] = doc[p], ![p] = Nil]
  BY <1>ra DEF GRename, aux
<1>3. \A k \in Paths : \/ doc'[k] = Nil \/ \E j \in Paths : doc'[k] = doc[j]
                       \/ \E j \in Paths : gw[j] # Nil /\ doc'[k] = gw[j]
  BY <1>1, <1>ty
<1>. QED BY <1>1, <1>3, A4Gate

LEMMA GRenameFinish_A4 ==
  ASSUME A4Hyp, GRenameFinish, Frame
  PROVE  A4Inv'
<1>ty. doc \in [Paths -> Opt(Handles)] BY DEF A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>0. mv # Nil /\ mv[1] \in Paths BY DEF GRenameFinish, A4Hyp, IndC, IndM5, IndM4, IndM3, IndM2, IndM1, IndTypeOK, TypeOK
<1>1. /\ w' = w /\ orig' = orig /\ minted' = minted /\ gw' = gw
      /\ doc' = [doc EXCEPT ![mv[1]] = IF doc[mv[1]] = mv[2] THEN Nil ELSE @]
  BY DEF GRenameFinish, aux
<1>3. \A k \in Paths : \/ doc'[k] = Nil \/ \E j \in Paths : doc'[k] = doc[j]
                       \/ \E j \in Paths : gw[j] # Nil /\ doc'[k] = gw[j]
  BY <1>0, <1>1, <1>ty
<1>. QED BY <1>1, <1>3, A4Gate

LEMMA Sweep_A4 ==
  ASSUME A4Hyp, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  A4Inv'
<1>1. w' = w /\ orig' = orig /\ minted' = minted /\ doc' = doc /\ gw' = gw BY DEF Sweep, aux
<1>. QED BY <1>1, A4Quiet

------------------------------------------------------------------------------
(* Init, the step, and the theorem.                                          *)

LEMMA Init_A4 == Init => A4Inv
<1>. SUFFICES ASSUME Init PROVE A4Inv OBVIOUS
<1>0. \A u \in Writers : w[u] = WriterInit BY DEF Init
<1>1. \A u \in Writers, q \in Paths : w[u].decl[q] = Nil /\ w[u].local[q] = Nil /\ w[u].baseline[q] = Nil
  BY <1>0 DEF WriterInit
<1>2. \A u \in Writers : w[u].uploads = {} /\ w[u].deletes = {} BY <1>0 DEF WriterInit
<1>3. \A p \in Paths : gw[p] = Nil BY DEF Init
<1>4. A1DeclPriv BY <1>1 DEF A1DeclPriv, A1Priv
<1>5. A4DU BY <1>1 DEF A4DU, A4Dirt
<1>6. A4Gw BY <1>3 DEF A4Gw
<1>7. A4DB BY <1>2 DEF A4DB
<1>. QED BY <1>4, <1>5, <1>6, <1>7 DEF A4Inv

LEMMA Next_A4 == A4Hyp /\ Next => A4Inv'
<1>. SUFFICES ASSUME A4Hyp, Next PROVE A4Inv' OBVIOUS
<1>1. CASE (GatewayStep \/ WriterStep) /\ Frame
  <2>. Frame BY <1>1
  <2>1. CASE GatewayStep
    <3>1. ASSUME NEW p \in Paths, GPut(p) PROVE A4Inv' BY <3>1, GPut_A4
    <3>2. ASSUME NEW p \in Paths, GCas(p) PROVE A4Inv' BY <3>2, GCas_A4
    <3>3. ASSUME NEW p \in Paths, GDelete(p) PROVE A4Inv' BY <3>3, GDelete_A4
    <3>4. ASSUME NEW p \in Paths, NEW q \in Paths, GRename(p, q) PROVE A4Inv' BY <3>4, GRename_A4
    <3>5. ASSUME GRenameFinish PROVE A4Inv' BY <3>5, GRenameFinish_A4
    <3>. QED BY <2>1, <3>1, <3>2, <3>3, <3>4, <3>5 DEF GatewayStep
  <2>2. CASE WriterStep
    <3>1. ASSUME NEW s \in Writers, Checkout(s) PROVE A4Inv' BY <3>1, Checkout_A4
    <3>2. ASSUME NEW s \in Writers, Consume(s) PROVE A4Inv' BY <3>2, Consume_A4
    <3>3. ASSUME NEW s \in Writers, Scan(s) PROVE A4Inv' BY <3>3, Scan_A4
    <3>4. ASSUME NEW s \in Writers, Skip(s) PROVE A4Inv' BY <3>4, Skip_A4
    <3>5. ASSUME NEW s \in Writers, PullOnly(s) PROVE A4Inv' BY <3>5, PullOnly_A4
    <3>6. ASSUME NEW s \in Writers, Claim(s) PROVE A4Inv' BY <3>6, Claim_A4
    <3>7. ASSUME NEW s \in Writers, Verify(s) PROVE A4Inv' BY <3>7, Verify_A4
    <3>8. ASSUME NEW s \in Writers, Install(s) PROVE A4Inv' BY <3>8, Install_A4
    <3>9. ASSUME NEW s \in Writers, Collect(s) PROVE A4Inv' BY <3>9, Collect_A4
    <3>10. ASSUME NEW s \in Writers, Finish(s) PROVE A4Inv' BY <3>10, Finish_A4
    <3>11. ASSUME NEW s \in Writers, Restart(s) PROVE A4Inv' BY <3>11, Restart_A4
    <3>12. ASSUME NEW s \in Writers, Sync(s) PROVE A4Inv' BY <3>12, Sync_A4
    <3>13. ASSUME NEW s \in Writers, RescopeBegin(s) PROVE A4Inv' BY <3>13, RescopeBegin_A4
    <3>14. ASSUME NEW s \in Writers, RescopeFirst(s) PROVE A4Inv' BY <3>14, RescopeFirst_A4
    <3>15. ASSUME NEW s \in Writers, RescopeSecond(s) PROVE A4Inv' BY <3>15, RescopeSecond_A4
    <3>16. ASSUME NEW s \in Writers, RPullRead(s) PROVE A4Inv' BY <3>16, RPullRead_A4
    <3>17. ASSUME NEW s \in Writers, RPullSync(s) PROVE A4Inv' BY <3>17, RPullSync_A4
    <3>18. ASSUME NEW s \in Writers, NEW p \in Paths, Edit(s, p) PROVE A4Inv' BY <3>18, Edit_A4
    <3>19. ASSUME NEW s \in Writers, NEW p \in Paths, Delete(s, p) PROVE A4Inv' BY <3>19, Delete_A4
    <3>20. ASSUME NEW s \in Writers, NEW p \in Paths, Upload(s, p) PROVE A4Inv' BY <3>20, Upload_A4
    <3>21. ASSUME NEW s \in Writers, NEW h \in Handles, Sweep(s, h) PROVE A4Inv' BY <3>21, Sweep_A4
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE A4Inv' BY <3>22, Touch_A4
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE A4Inv' BY <3>23, Honor_A4
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE A4Inv' BY <3>24, HonorReader_A4
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE A4Inv' BY <3>25, RetireByAck_A4
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE A4Inv' BY <3>26, Depose_A4
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE A4Inv' BY <3>27, Fence_A4
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
         DEF WriterStep
  <2>. QED BY <1>1, <2>1, <2>2
<1>2. CASE (Age \/ RLoad \/ \E s \in Writers, h \in Handles : Reap(s, h)) /\ UNCHANGED anc
  <2>1. w' = w /\ orig' = orig /\ minted' = minted /\ doc' = doc /\ gw' = gw BY <1>2 DEF Age, RLoad, Reap, aux
  <2>. QED BY <2>1, A4Quiet
<1>. QED BY <1>1, <1>2 DEF Next, Frame

LEMMA A4Step == A4Hyp /\ [Next]_vars => A4Inv'
<1>. SUFFICES ASSUME A4Hyp, [Next]_vars PROVE A4Inv' OBVIOUS
<1>1. CASE Next BY <1>1, Next_A4
<1>2. CASE UNCHANGED vars
  <2>1. w' = w /\ orig' = orig /\ minted' = minted /\ doc' = doc /\ gw' = gw BY <1>2 DEF vars, bucket, aux
  <2>. QED BY <2>1, A4Quiet
<1>. QED BY <1>1, <1>2

\* M11's invariant and this one, jointly: A1IAK1's step needs A1DeclPriv (in
\* A1Side), and this one's needs A1IAK1'.
LEMMA A4JStep == IndC /\ IndC' /\ DInv /\ A2Side /\ A3Side /\ A1IAK1 /\ A4Inv /\ [Next]_vars
                   => A1IAK1' /\ A4Inv'
<1>. SUFFICES ASSUME IndC, IndC', DInv, A2Side, A3Side, A1IAK1, A4Inv, [Next]_vars
              PROVE  A1IAK1' /\ A4Inv' OBVIOUS
<1>1. A1Side BY A2ToA1 DEF A1Side, A4Inv, A3Side
<1>2. A1IAK1' BY <1>1, A1Step
<1>3. A4Hyp BY <1>2 DEF A4Hyp
<1>. QED BY <1>2, <1>3, A4Step

THEOREM A4Joint == Spec => [](A1IAK1 /\ A4Inv)
<1>1. Init => A1IAK1 /\ A4Inv BY Init_A1, Init_A4
<1>. QED BY <1>1, A4JStep, CInvariant, DInvariant, A2Inv, A3Inv, PTL DEF Spec

\* THE RESULT (row 15, AK1 part 2): the acknowledgement names what it carried.
THEOREM AK1Holds == Spec => AK1
<1>1. Spec => []A1DeclPriv BY A4Joint, PTL DEF A4Inv
<1>. QED BY <1>1, AK1FromPriv, PTL

==============================================================================
