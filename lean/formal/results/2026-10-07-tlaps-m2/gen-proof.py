#!/usr/bin/env python3
"""Emit lean/formal/LeanP1Proof.tla: M0 + M1 (results/2026-10-07-tlaps-m1/gen-proof.py)
followed by M2 -- Inv_CitationsLive and Inv_OneName over IndM2 = IndM1 + the
freshness / save-in-flight / upload conjuncts (results/2026-10-07-tlaps-m2/
MCLeanP1M2.tla, TLC-checked; NOTES.txt).  Every step states its events in
four normalised facts (DocEv, GwEv, RetEv, TreeEv) and one lemma per
conjunct consumes them.
Run from lean/formal:  python3 results/2026-10-07-tlaps-m2/gen-proof.py LeanP1Proof.tla"""
import sys, runpy, os
OUT = sys.argv[1]
HERE = os.path.dirname(os.path.abspath(__file__))
m1 = runpy.run_path(os.path.join(HERE, '..', '2026-10-07-tlaps-m1', 'gen-proof.py'), run_name='m1')
END = m1['END']
PRE = m1['M0TEXT'] + m1['M1']
PRE = PRE.replace(
    "   once (`<1>3`) and discharges each lemma's hypothesis from them.        *)\n",
    "   once (`<1>3`) and discharges each lemma's hypothesis from them.\n"
    "   M2 (after M1): `Inv_CitationsLive` and `Inv_OneName` over `IndM2` --\n"
    "   IndM1 and the plan's I2-I5 as the proof needs them (results/\n"
    "   2026-10-07-tlaps-m2/NOTES.txt): a handle never PUT is private to its\n"
    "   minter; an upload is uncited from its PUT to the CAS; a verified one\n"
    "   is live; a save in flight is live, uncited and in no tree.  Each step\n"
    "   states its events in four normalised facts (what the document, the\n"
    "   saves in flight, the retired set and the written tree may now hold)\n"
    "   and one lemma per conjunct consumes them.                              *)\n")
assert 'M2 (after M1)' in PRE

M2 = r'''
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

'''

# ---------------------------------------------------------------------------
# Step lemmas.

HEAD_COMMON = '''<1>a. IndM1 /\\ IndTypeOK /\\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\\ Holder /\\ Mine /\\ Ups /\\ Rescope /\\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\\ Fresh /\\ SnapMinted /\\ Flight /\\ Private /\\ Pending /\\ Uploaded /\\ Verified /\\ Unverified
      /\\ Inv_CitationsLive /\\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\\ gw \\in [Paths -> Opt(Handles)] /\\ doc \\in [Paths -> Opt(Handles)] /\\ nextGen \\in Nat
      /\\ holder \\in Writers \\cup {"none"} /\\ live \\subseteq Handles /\\ minted \\subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\\ SweepUnderLease /\\ GatewaySweepGrace /\\ CommitVerifiesUploads /\\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
'''
FRAME_EV = '''<1>r. RetEv /\\ aged' = aged BY <1>s, RetFacts DEF Frame
'''

def nostep(name, sig, binders, unch_defs, frame='Frame', ev=FRAME_EV):
    """A step that writes no tree: M2' from the events with t = "none"."""
    return f'''LEMMA {name}_M2 ==
  ASSUME IndM2{binders}, {sig}, {frame}
  PROVE  IndM2'
{HEAD_COMMON}<1>t. IndM1' BY <1>a, {name}_M1
'''

FIELDS2 = ['pc', 'uploads', 'upDone', 'gone', 'verified', 'local', 'snap']

def reads2(rec, changed):
    out = []
    for f in FIELDS2:
        out.append(f'{rec}.{f} = {changed.get(f, "w[s]." + f)}')
    return out

def tree_step(name, sig, binders, rec, recdef, restate, guards, guard_defs, changed,
              L=1, pre='', reads_by=None, ev_by=None, nc='{}',
              fresh='keep', flight_by=None, private_by=None, pending_by=None, uploaded_by=None,
              verified_by=None, unverified_by=None, cl_by=None, on_by=None, head=True, pick_restate=None):
    """A writer step lemma body at level L (<L>1..) after the common facts."""
    I = '  ' * (L - 1)
    def S(n): return f'<{L}>{n}'
    t = ''
    if head:
        t += f'''LEMMA {name}_M2 ==
  ASSUME IndM2, NEW s \\in Writers{binders}, {sig}, Frame
  PROVE  IndM2'
{HEAD_COMMON}{FRAME_EV}<1>t. IndM1' BY <1>a, {name}_M1
<1>t2. w' \\in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \\in Writer BY <1>a DEF TypeOK
<1>f. /\\ w[s].local \\in [Paths -> Opt(Handles)] /\\ w[s].snap \\in [Paths -> Opt(Handles)]
      /\\ w[s].uploads \\subseteq Paths /\\ w[s].upDone \\subseteq Paths /\\ w[s].deletes \\subseteq Paths /\\ w[s].gone \\subseteq Paths
      /\\ w[s].pc \\in {{"idle", "consumed", "scanned", "claimed", "cased", "pulling"}} /\\ w[s].verified \\in BOOLEAN
  BY <1>w, WriterFields
'''
    t += pre
    t += f'{I}{S(1)}. {restate}\n'
    t += f'{I}{S("g")}. {guards} BY DEF {guard_defs}\n'
    t += f'{I}{S(2)}. Wr(s, {rec}) BY <1>a, {S(1)}, WriteAny DEF Wr\n'
    rd = reads2(rec, changed)
    t += f'{I}{S(3)}. /\\ ' + f'\n{I}      /\\ '.join(rd) + f'\n{I}  BY {reads_by or "DEF " + recdef}\n'
    # Fresh' first: a mint's freshness feeds the events.
    if fresh == 'keep':
        t += f'{I}{S(4)}. Fresh\' BY <1>c, {S(1)}, <1>r, FreshKeep\n'
    else:
        t += fresh
    t += f'{I}{S("e")}. /\\ DocEv({nc}) /\\ GwEv /\\ TreeEv(s, {rec}) /\\ LiveEv\n'
    t += f'{I}      /\\ minted \\subseteq minted\' /\\ upped \\subseteq upped\'\n'
    t += f'{I}  BY {ev_by or (S(1) + ", " + S(3) + ", <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt")}\n'
    t += f'{I}{S("5a")}. \\A q \\in Paths : {rec}.snap[q] \\in Opt(minted\') BY {S(1)}, {S(3)}, <1>c, <1>d, {S("e")}, <1>f DEF SnapMinted, Minted, Opt\n'
    t += f'{I}{S(5)}. SnapMinted\' BY <1>c, {S(2)}, {S("e")}, {S("5a")}, SnapMintedWrite\n'
    t += f'{I}{S("6a")}. \\A p \\in Paths : gw\'[p] # Nil => gw\'[p] = gw[p] /\\ gw[p] \\notin {nc} BY {flight_by or S(1)}\n'
    t += f'{I}{S(6)}. Flight\' BY <1>c, <1>d, {S("6a")}, {S("e")}, <1>r, {S(2)}, FlightKeep\n'
    t += f'{I}{S("7a")}. \\A pp \\in Paths : ({rec}.local[pp] # Nil /\\ {rec}.local[pp] \\notin upped\') =>\n'
    t += f'{I}          \\/ {rec}.local[pp] = w[s].local[pp]\n'
    t += f'{I}          \\/ ({rec}.local[pp][1] = pp /\\ {rec}.local[pp] \\notin minted)\n'
    t += f'{I}  BY {private_by or S(3)}\n'
    t += f'{I}{S(7)}. Private\' BY <1>c, <1>d, {S(2)}, {S("e")}, {S("7a")}, PrivateWrite\n'
    t += f'{I}{S("8a")}. {rec}.pc \\in {{"scanned", "claimed"}} =>\n'
    t += f'{I}          \\A pp \\in {rec}.uploads \\ {rec}.upDone :\n'
    t += f'{I}            /\\ {rec}.snap[pp] # Nil\n'
    t += f'{I}            /\\ ({rec}.snap[pp] \\notin upped\' =>\n'
    t += f'{I}                  \\/ ({rec}.snap[pp] = w[s].snap[pp] /\\ w[s].pc \\in {{"scanned", "claimed"}} /\\ pp \\in w[s].uploads \\ w[s].upDone)\n'
    t += f'{I}                  \\/ {rec}.snap[pp] = w[s].local[pp]\n'
    t += f'{I}                  \\/ ({rec}.snap[pp][1] = pp /\\ {rec}.snap[pp] \\notin minted))\n'
    t += f'{I}  BY {pending_by or (S(3) + ", " + S("g") + ", <1>c DEF Pending")}\n'
    t += f'{I}{S(8)}. Pending\' BY <1>c, <1>d, <1>t2, {S(2)}, {S("e")}, {S("8a")}, PendingWrite\n'
    t += f'{I}{S("9a")}. \\A u \\in Writers, pp \\in Paths : (u # s /\\ w[u].pc \\in {{"scanned", "claimed"}} /\\ pp \\in w[u].upDone) => w[u].snap[pp] \\notin {nc}\n'
    t += f'{I}  {"OBVIOUS" if nc == "{}" else "BY " + uploaded_by[0]}\n'
    t += f'{I}{S("9b")}. {rec}.pc \\in {{"scanned", "claimed"}} =>\n'
    t += f'{I}          \\A pp \\in {rec}.upDone : {rec}.snap[pp][1] = pp /\\\n'
    t += f'{I}            \\/ ({rec}.snap[pp] = w[s].snap[pp] /\\ w[s].pc \\in {{"scanned", "claimed"}} /\\ pp \\in w[s].upDone /\\ w[s].snap[pp] \\notin {nc})\n'
    t += f'{I}            \\/ ({rec}.snap[pp] = w[s].snap[pp] /\\ w[s].pc \\in {{"scanned", "claimed"}} /\\ pp \\in w[s].uploads \\ w[s].upDone\n'
    t += f'{I}                /\\ w[s].snap[pp] \\notin upped /\\ w[s].snap[pp] \\in upped\' /\\ w[s].snap[pp] \\notin {nc})\n'
    t += f'{I}            \\/ ({rec}.snap[pp] # Nil /\\ {rec}.snap[pp] \\notin minted /\\ {rec}.snap[pp] \\in upped\' /\\ {rec}.snap[pp] \\notin {nc}\n'
    t += f'{I}                /\\ \\A k \\in Paths : gw\'[k] # {rec}.snap[pp])\n'
    t += f'{I}  BY {uploaded_by[1] if uploaded_by else (S(3) + ", " + S("g") + ", <1>c DEF Uploaded")}\n'
    t += f'{I}{S(9)}. Uploaded\' BY <1>c, <1>d, <1>t2, {S(2)}, {S("e")}, <1>r, {S("9a")}, {S("9b")}, UploadedWrite\n'
    t += f'{I}{S("10a")}. ({rec}.pc = "claimed" /\\ {rec}.verified) => \\A pp \\in ({rec}.uploads \\cap {rec}.upDone) \\ {rec}.gone : {rec}.snap[pp] \\in live\'\n'
    t += f'{I}  BY {verified_by or (S(1) + ", " + S(3) + ", " + S("g") + ", <1>c DEF Verified")}\n'
    t += f'{I}{S(10)}. Verified\' BY <1>c, <1>m, {S(2)}, {S("e")}, {S("10a")}, VerifiedWrite\n'
    t += f'{I}{S("11a")}. {rec}.pc = "scanned" => ~{rec}.verified BY {unverified_by or (S(3) + ", " + S("g") + ", <1>c DEF Unverified")}\n'
    t += f'{I}{S(11)}. Unverified\' BY <1>c, {S(2)}, {S("11a")}, UnverifiedWrite\n'
    t += f'{I}{S("12a")}. \\A h \\in {nc} : h \\in live\' {"OBVIOUS" if nc == "{}" else "BY " + cl_by}\n'
    t += f'{I}{S(12)}. Inv_CitationsLive\' BY <1>c, {S("e")}, {S("12a")}, CLWrite\n'
    t += f'{I}{S(13)}. Inv_OneName\' BY {on_by or (S(1) + ", <1>c DEF Inv_OneName")}\n'
    t += f'{I}{S("")}. QED BY <1>t, {S(4)}, {S(5)}, {S(6)}, {S(7)}, {S(8)}, {S(9)}, {S(10)}, {S(11)}, {S(12)}, {S(13)} DEF IndM2, M2\n'
    return t

# The restatements name every variable M2 reads.
UNCH_M2 = "UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>"

def W(name, *a, **k):
    return tree_step(name, *a, **k) + '\n'

BODY = ''
BODY += '------------------------------------------------------------------------------\n(* The gateway.                                                             *)\n\n'
BODY += r'''LEMMA GPut_M2 ==
  ASSUME IndM2, NEW p \in Paths, GPut(p), Frame
  PROVE  IndM2'
''' + HEAD_COMMON + FRAME_EV + r'''<1>t. IndM1' BY <1>a, GPut_M1
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
''' + HEAD_COMMON + FRAME_EV + r'''<1>t. IndM1' BY <1>a, GCas_M1
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
''' + HEAD_COMMON + FRAME_EV + r'''<1>t. IndM1' BY <1>a, GRename_M1
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
''' + HEAD_COMMON + FRAME_EV + r'''<1>t. IndM1' BY <1>a, GDelete_M1
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

'''
# Sweep, Reap: live shrinks; Age, RLoad: nothing M2 reads but retiring/aged.
BODY += r'''LEMMA Sweep_M2 ==
  ASSUME IndM2, NEW s \in Writers, NEW h \in Handles, Sweep(s, h), Frame
  PROVE  IndM2'
''' + HEAD_COMMON + FRAME_EV + r'''<1>t. IndM1' BY <1>a, Sweep_M1
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
''' + HEAD_COMMON + r'''<1>t. IndM1' BY <1>a, Reap_M1
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
''' + HEAD_COMMON + r'''<1>t. IndM1' BY <1>a, Age_M1
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

'''
# ---------------------------------------------------------------------------
# The writer steps.
UNCH7 = "UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>"
DOCLOCAL = "<1>3, <1>e, <1>c, <1>f, <1>0 DEF Inv_CitationsLive, Fresh"   # a doc handle is PUT: the un-PUT case is vacuous

BODY += '------------------------------------------------------------------------------\n(* The agent.                                                               *)\n\n'
BODY += W('Edit', 'Edit(s, p)', ', NEW p \\in Paths', 'EditW(s, p)', 'EditW',
          "/\\ nextGen <= MaxMint /\\ minted' = minted \\cup {<<p, nextGen>>} /\\ nextGen' = nextGen + 1\n"
          "      /\\ w' = [w EXCEPT ![s] = EditW(s, p)] /\\ UNCHANGED <<live, doc, gw, copies, upped>>\n  BY DEF Edit, aux",
          'On(s)', 'Edit',
          {'local': '[w[s].local EXCEPT ![p] = <<p, nextGen>>]'},
          fresh="<1>4. Fresh' /\\ <<p, nextGen>> \\notin minted BY <1>c, <1>0, <1>1, <1>r, FreshMint DEF Fresh\n",
          ev_by="<1>1, <1>3, <1>4, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt",
          private_by="<1>3, <1>4, <1>f")
BODY += W('Delete', 'Delete(s, p)', ', NEW p \\in Paths', 'DeleteW(s, p)', 'DeleteW',
          f"w' = [w EXCEPT ![s] = DeleteW(s, p)] /\\ {UNCH7} BY DEF Delete, bucket, aux",
          'On(s)', 'Delete', {'local': '[w[s].local EXCEPT ![p] = Nil]'},
          private_by="<1>3, <1>f")
BODY += W('Checkout', 'Checkout(s)', '', 'CheckoutW(s, T)', 'CheckoutW',
          f"PICK T \\in Scopes : w' = [w EXCEPT ![s] = CheckoutW(s, T)] /\\ {UNCH7} BY DEF Checkout, bucket, aux",
          'w[s].st = "off"', 'Checkout', {'local': 'CheckoutHeld(s, T)'},
          ev_by="<1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CheckoutHeld, CitedSet",
          private_by="<1>3, <1>e, <1>c, <1>f, <1>0 DEF CheckoutHeld, Inv_CitationsLive, Fresh")

# Consume: two trees.
BODY += f'''LEMMA Consume_M2 ==
  ASSUME IndM2, NEW s \\in Writers, Consume(s), Frame
  PROVE  IndM2'
{HEAD_COMMON}{FRAME_EV}<1>t. IndM1' BY <1>a, Consume_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \\in Writer BY <1>a DEF TypeOK
<1>f. /\\ w[s].local \\in [Paths -> Opt(Handles)] /\\ w[s].snap \\in [Paths -> Opt(Handles)]
      /\\ w[s].uploads \\subseteq Paths /\\ w[s].upDone \\subseteq Paths /\\ w[s].deletes \\subseteq Paths /\\ w[s].gone \\subseteq Paths
      /\\ w[s].pc \\in {{"idle", "consumed", "scanned", "claimed", "cased", "pulling"}} /\\ w[s].verified \\in BOOLEAN
  BY <1>w, WriterFields
<1>1. CASE CheapPath(s)
'''
BODY += tree_step('Consume', 'Consume(s)', '', 'ConsumeCheapW(s)', 'ConsumeCheapW',
                  f"w' = [w EXCEPT ![s] = ConsumeCheapW(s)] /\\ {UNCH7} BY <1>1 DEF Consume",
                  'On(s) /\\ w[s].pc = "idle"', 'Consume', {'pc': '"consumed"'}, L=2, head=False)
BODY += '<1>2. CASE ~CheapPath(s)\n'
BODY += tree_step('Consume', 'Consume(s)', '', 'ConsumeW(s, fail)', 'ConsumeW',
                  f"PICK fail \\in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail)] /\\ {UNCH7} BY <1>2 DEF Consume",
                  'On(s) /\\ w[s].pc = "idle"', 'Consume',
                  {'pc': '"consumed"', 'local': '[q \\in Paths |-> IF q \\in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].local[q]]'},
                  L=2, head=False,
                  ev_by="<2>1, <2>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet",
                  private_by="<2>3, <2>e, <1>c, <1>f, <1>0 DEF Inv_CitationsLive, Fresh")
BODY += '<1>. QED BY <1>1, <1>2\n\n'

BODY += W('Scan', 'Scan(s)', '', 'ScanW(s, dels)', 'ScanW',
          f"PICK dels \\in SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] /\\ {UNCH7} BY DEF Scan, bucket, aux",
          'On(s) /\\ w[s].pc = "consumed"', 'Scan',
          {'pc': '"scanned"', 'uploads': 'ScanUps(s)', 'upDone': '{}', 'gone': '{}', 'verified': 'FALSE', 'snap': 'w[s].local'},
          pending_by="<1>3 DEF ScanUps, ScanDirty", uploaded_by=None, unverified_by="<1>3")
BODY += W('Skip', 'Skip(s)', '', 'SkipW(s)', 'SkipW',
          f"w' = [w EXCEPT ![s] = SkipW(s)] /\\ {UNCH7} BY DEF Skip, bucket, aux",
          'On(s) /\\ w[s].pc = "consumed"', 'Skip', {'pc': '"idle"'})

# Upload: two trees.
BODY += f'''LEMMA Upload_M2 ==
  ASSUME IndM2, NEW s \\in Writers, NEW p \\in Paths, Upload(s, p), Frame
  PROVE  IndM2'
{HEAD_COMMON}{FRAME_EV}<1>t. IndM1' BY <1>a, Upload_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \\in Writer BY <1>a DEF TypeOK
<1>f. /\\ w[s].local \\in [Paths -> Opt(Handles)] /\\ w[s].snap \\in [Paths -> Opt(Handles)]
      /\\ w[s].uploads \\subseteq Paths /\\ w[s].upDone \\subseteq Paths /\\ w[s].deletes \\subseteq Paths /\\ w[s].gone \\subseteq Paths
      /\\ w[s].pc \\in {{"idle", "consumed", "scanned", "claimed", "cased", "pulling"}} /\\ w[s].verified \\in BOOLEAN
  BY <1>w, WriterFields
<1>g. On(s) /\\ w[s].pc = "scanned" /\\ p \\in w[s].uploads \\ w[s].upDone BY DEF Upload
<1>h. w[s].snap[p] # Nil /\\ w[s].snap[p] \\in minted BY <1>g, <1>c DEF Pending, SnapMinted, Opt
<1>1. CASE w[s].snap[p] \\notin upped
'''
BODY += tree_step('Upload', 'Upload(s, p)', ', NEW p \\in Paths', 'UploadW(s, p)', 'UploadW',
                  "/\\ live' = live \\cup {w[s].snap[p]} /\\ upped' = upped \\cup {w[s].snap[p]}\n"
                  "        /\\ w' = [w EXCEPT ![s] = UploadW(s, p)] /\\ UNCHANGED <<minted, doc, gw, nextGen, copies>>\n    BY <1>1 DEF Upload",
                  'On(s) /\\ w[s].pc = "scanned" /\\ p \\in w[s].uploads \\ w[s].upDone', 'Upload',
                  {'upDone': 'w[s].upDone \\cup {p}'}, L=2, head=False,
                  fresh="  <2>4. Fresh' BY <1>c, <1>h, <2>1, <1>r, FreshPut\n",
                  pending_by="<2>3, <1>g, <1>c DEF Pending",
                  uploaded_by=(None, "<2>1, <2>3, <1>1, <1>g, <1>c DEF Uploaded, Pending"))
BODY += '<1>2. CASE w[s].snap[p] \\in upped\n  <2>. DEFINE c == <<p, MaxMint + copies + 1>>\n'
BODY += tree_step('Upload', 'Upload(s, p)', ', NEW p \\in Paths', 'UploadCopyW(s, p, c)', 'UploadCopyW',
                  "/\\ copies < MaxCopies\n"
                  "        /\\ live' = live \\cup {c} /\\ upped' = upped \\cup {c} /\\ minted' = minted \\cup {c} /\\ copies' = copies + 1\n"
                  "        /\\ w' = [w EXCEPT ![s] = UploadCopyW(s, p, c)] /\\ UNCHANGED <<doc, gw, nextGen>>\n    BY <1>2 DEF Upload",
                  'On(s) /\\ w[s].pc = "scanned" /\\ p \\in w[s].uploads \\ w[s].upDone', 'Upload',
                  {'upDone': 'w[s].upDone \\cup {p}',
                   'local': '[w[s].local EXCEPT ![p] = IF w[s].local[p] = w[s].snap[p] THEN c ELSE w[s].local[p]]',
                   'snap': '[w[s].snap EXCEPT ![p] = c]'},
                  L=2, head=False,
                  fresh="  <2>4. Fresh' /\\ c \\notin minted BY <1>c, <1>d, <1>0, <2>1, <1>r, FreshCopy\n",
                  ev_by="<2>1, <2>3, <2>4, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt",
                  private_by="<2>3, <2>4, <1>f",
                  pending_by="<2>3, <1>g, <1>c, <1>f DEF Pending",
                  uploaded_by=(None, "<2>1, <2>3, <2>4, <1>g, <1>c, <1>d, <1>0, <1>f, CopyHandle, NilHandle DEF Uploaded, Minted, Opt"))
BODY += '<1>. QED BY <1>1, <1>2\n\n'

BODY += '------------------------------------------------------------------------------\n(* The commit section.                                                      *)\n\n'
BODY += W('PullOnly', 'PullOnly(s)', '', 'PullOnlyW(s)', 'PullOnlyW',
          f"w' = [w EXCEPT ![s] = PullOnlyW(s)] /\\ {UNCH7} BY DEF PullOnly, bucket, aux",
          'On(s) /\\ w[s].pc = "scanned"', 'PullOnly',
          {'pc': '"idle"', 'upDone': '{}', 'gone': '{}', 'verified': 'FALSE', 'snap': '[q \\in Paths |-> Nil]'},
          unverified_by="<1>3")
BODY += W('Claim', 'Claim(s)', '', 'ClaimW(s)', 'ClaimW',
          f"w' = [w EXCEPT ![s] = ClaimW(s)] /\\ {UNCH7} BY DEF Claim, aux",
          'On(s) /\\ w[s].pc = "scanned"', 'Claim', {'pc': '"claimed"'},
          verified_by="<1>3, <1>g, <1>c DEF Unverified", unverified_by="<1>3")
BODY += W('Verify', 'Verify(s)', '', 'VerifyW(s)', 'VerifyW',
          f"w' = [w EXCEPT ![s] = VerifyW(s)] /\\ {UNCH7} BY DEF Verify, bucket, aux",
          'On(s) /\\ w[s].pc = "claimed"', 'Verify',
          {'gone': 'IF CommitVerifiesUploads THEN {q \\in w[s].uploads \\cap w[s].upDone : w[s].snap[q] \\notin live} ELSE {}',
           'verified': 'TRUE'},
          verified_by="<1>1, <1>3, <1>s", unverified_by="<1>3, <1>g")
BODY += W('Install', 'Install(s)', '', 'InstallW(s)', 'InstallW',
          "doc' = InstallInst(s) /\\ w' = [w EXCEPT ![s] = InstallW(s)] /\\ UNCHANGED <<live, minted, gw, nextGen, copies, upped>>\n  BY DEF Install, aux",
          'On(s) /\\ w[s].pc = "claimed" /\\ w[s].verified', 'Install',
          {'pc': '"cased"', 'upDone': 'w[s].upDone \\ w[s].gone'},
          nc='{w[s].snap[k] : k \\in InstallMine(s) \\ w[s].gone}',
          pre="<1>i. \\A k \\in Paths : InstallInst(s)[k] = Nil \\/ InstallInst(s)[k] = doc[k]\n"
              "                      \\/ (k \\in InstallMine(s) \\ w[s].gone /\\ InstallInst(s)[k] = w[s].snap[k])\n"
              "  BY DEF InstallInst\n"
              "<1>j. \\A k \\in InstallMine(s) \\ w[s].gone : w[s].snap[k][1] = k /\\ ~Cited(w[s].snap[k]) /\\ w[s].snap[k] # Nil /\\ w[s].snap[k] \\in live\n"
              "  BY <1>c DEF Uploaded, Up, Verified, InstallMine, Install\n",
          ev_by="<1>1, <1>3, <1>i, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet",
          flight_by="<1>1, <1>c, <1>f DEF Flight, InstallMine",
          uploaded_by=("<1>c, <1>f DEF Uploaded, Up, Priv, InstallMine", "<1>3"),
          cl_by="<1>1, <1>j",
          on_by=None)
# Install's Inv_OneName': replace the default line with the case analysis.
BODY = BODY.replace("<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName\n<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2\n\nLEMMA Collect_M2", "PLACEHOLDER_INSTALL_ON", 1) if False else BODY
_install_on = """<1>13. Inv_OneName'
  <2>1. \\A k \\in InstallMine(s) \\ w[s].gone, j \\in Paths : doc[j] # w[s].snap[k] BY <1>j DEF Cited
  <2>. QED BY <1>1, <1>c, <1>i, <1>j, <2>1 DEF Inv_OneName
"""
_marker = "<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite\n<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName\n<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2\n"
_idx = BODY.rfind(_marker)   # the last one is Install's
assert _idx > 0
BODY = BODY[:_idx] + _marker.replace("<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName\n", _install_on) + BODY[_idx+len(_marker):]

BODY += W('Collect', 'Collect(s)', '', 'CollectW(s)', 'CollectW',
          "live' = live /\\ w' = [w EXCEPT ![s] = CollectW(s)] /\\ UNCHANGED <<minted, doc, gw, nextGen, copies, upped>>\n  BY <1>s DEF Collect, aux",
          'On(s) /\\ w[s].pc = "cased"', 'Collect', {})
BODY += W('Finish', 'Finish(s)', '', 'FinishW(s)', 'FinishW',
          f"w' = [w EXCEPT ![s] = FinishW(s)] /\\ {UNCH7} BY DEF Finish, aux",
          'On(s) /\\ w[s].pc = "cased"', 'Finish',
          {'pc': '"idle"', 'uploads': '{}', 'upDone': '{}', 'gone': '{}', 'verified': 'FALSE', 'snap': '[q \\in Paths |-> Nil]'},
          unverified_by="<1>3")
BODY += '------------------------------------------------------------------------------\n(* The restart and the sync.                                                *)\n\n'
BODY += W('Restart', 'Restart(s)', '', 'RestartW(s)', 'RestartW',
          f"w' = [w EXCEPT ![s] = RestartW(s)] /\\ {UNCH7} BY DEF Restart",
          'On(s)', 'Restart',
          {'pc': '"idle"', 'uploads': '{}', 'upDone': '{}', 'gone': '{}', 'verified': 'FALSE', 'snap': '[q \\in Paths |-> Nil]'},
          unverified_by="<1>3")
BODY += W('Sync', 'Sync(s)', '', 'SyncW(s, fail)', 'SyncW',
          f"PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] /\\ {UNCH7} BY DEF Sync, bucket",
          'On(s) /\\ w[s].pc = "idle"', 'Sync',
          {'local': '[q \\in Paths |-> IF q \\in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]'},
          ev_by="<1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet",
          private_by=DOCLOCAL)
BODY += '------------------------------------------------------------------------------\n(* The narrow / widen verb.                                                 *)\n\n'
BODY += W('RescopeBegin', 'RescopeBegin(s)', '', 'RescopeBeginW(s, T)', 'RescopeBeginW',
          f"PICK T \\in Scopes \\ {{w[s].scope}} : w' = [w EXCEPT ![s] = RescopeBeginW(s, T)] /\\ {UNCH7} BY DEF RescopeBegin, bucket",
          'On(s) /\\ w[s].pc = "idle"', 'RescopeBegin', {})
BODY += W('RescopeFirst', 'RescopeFirst(s)', '', 'RescopeFirstW(s)', 'RescopeFirstW',
          f"w' = [w EXCEPT ![s] = RescopeFirstW(s)] /\\ {UNCH7} BY DEF RescopeFirst, bucket, aux",
          'On(s) /\\ w[s].pc = "idle"', 'RescopeFirst', {}, reads_by="<1>s DEF RescopeFirstW")
BODY += W('RescopeSecond', 'RescopeSecond(s)', '', 'RescopeSecondW(s, wfail)', 'RescopeSecondW',
          "PICK wfail \\in SUBSET {q \\in RescopeFetch0(s) : RescopeLocal1(s)[q] = Nil \\/ ~WidenKeepsLocal} :\n"
          f"        w' = [w EXCEPT ![s] = RescopeSecondW(s, wfail)] /\\ {UNCH7}\n  BY DEF RescopeSecond, bucket",
          'On(s) /\\ w[s].pc = "idle"', 'RescopeSecond',
          {'local': '[q \\in Paths |-> IF q \\in RescopeFetch(s, wfail) /\\ (RescopeLocal1(s)[q] = Nil \\/ ~WidenKeepsLocal)\n'
                    '                                        THEN doc[q] ELSE RescopeLocal1(s)[q]]'},
          pre="<1>u. RescopeLocal1(s) = [q \\in Paths |-> IF RescopeUnlink(s, q) THEN Nil ELSE w[s].local[q]] BY <1>s DEF RescopeLocal1\n",
          ev_by="<1>1, <1>3, <1>u, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet",
          private_by="<1>3, <1>u, <1>e, <1>c, <1>f, <1>0 DEF Inv_CitationsLive, Fresh")
BODY += '------------------------------------------------------------------------------\n(* A reader\'s tick.                                                         *)\n\n'
BODY += W('RPullRead', 'RPullRead(s)', '', 'RPullReadW(s)', 'RPullReadW',
          f"w' = [w EXCEPT ![s] = RPullReadW(s)] /\\ {UNCH7} BY DEF RPullRead, bucket, aux",
          'On(s) /\\ w[s].pc = "idle"', 'RPullRead', {'pc': '"pulling"'})
BODY += W('RPullSync', 'RPullSync(s)', '', 'RPullSyncW(s, fail)', 'RPullSyncW',
          f"PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] /\\ {UNCH7} BY DEF RPullSync, bucket",
          'On(s) /\\ w[s].pc = "pulling"', 'RPullSync',
          {'pc': '"idle"', 'local': '[q \\in Paths |-> IF q \\in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]'},
          ev_by="<1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet",
          private_by=DOCLOCAL)

TAIL2 = r'''------------------------------------------------------------------------------
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
'''

open(OUT, 'w').write(PRE + M2 + BODY + TAIL2 + END)
print('written', OUT, len((PRE + M2 + BODY + TAIL2 + END).splitlines()), 'lines')
