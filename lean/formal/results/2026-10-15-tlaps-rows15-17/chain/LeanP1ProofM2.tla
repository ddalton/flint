---------------------------- MODULE LeanP1ProofM2 ----------------------------
EXTENDS LeanP1ProofM1

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
\* I5: a verified upload is live until the CAS -- while its writer is not
\* deposed (rows 15-17: the taker may sweep a deposed writer's uploads; the
\* fence keeps that writer from installing).
Verified == \A s \in Writers : (w[s].pc = "claimed" /\ w[s].verified /\ ~w[s].deposed) =>
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
         t \in Writers => ((R.pc = "claimed" /\ R.verified /\ ~R.deposed) =>
                             \A p \in (R.uploads \cap R.upDone) \ R.gone : R.snap[p] \in live')
  PROVE  Verified'
<1>. SUFFICES ASSUME NEW s \in Writers, w'[s].pc = "claimed", w'[s].verified, ~w'[s].deposed,
                     NEW p \in (w'[s].uploads \cap w'[s].upDone) \ w'[s].gone
              PROVE  w'[s].snap[p] \in live'
  BY DEF Verified
<1>1. CASE s = t BY <1>1 DEF Wr
<1>2. CASE s # t
  <2>1. w'[s] = w[s] BY <1>2 DEF Wr
  <2>2. w[s].snap[p] \in live BY <2>1 DEF Verified
  <2>3. w[s].snap[p] \notin aged BY <2>1 DEF Uploaded, Up
  <2>4. holder = s BY <2>1 DEF Inv_OneHolder, CC
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ EditW(s, p).deposed = w[s].deposed
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
<1>10a. (EditW(s, p).pc = "claimed" /\ EditW(s, p).verified /\ ~EditW(s, p).deposed) => \A pp \in (EditW(s, p).uploads \cap EditW(s, p).upDone) \ EditW(s, p).gone : EditW(s, p).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ DeleteW(s, p).deposed = w[s].deposed
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
<1>10a. (DeleteW(s, p).pc = "claimed" /\ DeleteW(s, p).verified /\ ~DeleteW(s, p).deposed) => \A pp \in (DeleteW(s, p).uploads \cap DeleteW(s, p).upDone) \ DeleteW(s, p).gone : DeleteW(s, p).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ CheckoutW(s, T).deposed = w[s].deposed
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
<1>10a. (CheckoutW(s, T).pc = "claimed" /\ CheckoutW(s, T).verified /\ ~CheckoutW(s, T).deposed) => \A pp \in (CheckoutW(s, T).uploads \cap CheckoutW(s, T).upDone) \ CheckoutW(s, T).gone : CheckoutW(s, T).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Consume_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>ck. PICK ck \in CarryClocks(s) :
          IF CheapPath(s)
          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]
          ELSE \E fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]
  BY DEF Consume
<1>1. CASE CheapPath(s)
  <2>1. w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <1>1, <1>ck DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" BY DEF Consume
  <2>2. Wr(s, ConsumeCheapW(s, ck)) BY <1>a, <2>1, WriteAny DEF Wr
  <2>3. /\ ConsumeCheapW(s, ck).pc = "consumed"
        /\ ConsumeCheapW(s, ck).uploads = w[s].uploads
        /\ ConsumeCheapW(s, ck).upDone = w[s].upDone
        /\ ConsumeCheapW(s, ck).gone = w[s].gone
        /\ ConsumeCheapW(s, ck).verified = w[s].verified
        /\ ConsumeCheapW(s, ck).local = w[s].local
        /\ ConsumeCheapW(s, ck).snap = w[s].snap
        /\ ConsumeCheapW(s, ck).deposed = w[s].deposed
    BY DEF ConsumeCheapW
  <2>4. Fresh' BY <1>c, <2>1, <1>r, FreshKeep
  <2>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, ConsumeCheapW(s, ck)) /\ LiveEv
        /\ minted \subseteq minted' /\ upped \subseteq upped'
    BY <2>1, <2>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
  <2>5a. \A q \in Paths : ConsumeCheapW(s, ck).snap[q] \in Opt(minted') BY <2>1, <2>3, <1>c, <1>d, <2>e, <1>f DEF SnapMinted, Minted, Opt
  <2>5. SnapMinted' BY <1>c, <2>2, <2>e, <2>5a, SnapMintedWrite
  <2>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <2>1
  <2>6. Flight' BY <1>c, <1>d, <2>6a, <2>e, <1>r, <2>2, FlightKeep
  <2>7a. \A pp \in Paths : (ConsumeCheapW(s, ck).local[pp] # Nil /\ ConsumeCheapW(s, ck).local[pp] \notin upped') =>
            \/ ConsumeCheapW(s, ck).local[pp] = w[s].local[pp]
            \/ (ConsumeCheapW(s, ck).local[pp][1] = pp /\ ConsumeCheapW(s, ck).local[pp] \notin minted)
    BY <2>3
  <2>7. Private' BY <1>c, <1>d, <2>2, <2>e, <2>7a, PrivateWrite
  <2>8a. ConsumeCheapW(s, ck).pc \in {"scanned", "claimed"} =>
            \A pp \in ConsumeCheapW(s, ck).uploads \ ConsumeCheapW(s, ck).upDone :
              /\ ConsumeCheapW(s, ck).snap[pp] # Nil
              /\ (ConsumeCheapW(s, ck).snap[pp] \notin upped' =>
                    \/ (ConsumeCheapW(s, ck).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                    \/ ConsumeCheapW(s, ck).snap[pp] = w[s].local[pp]
                    \/ (ConsumeCheapW(s, ck).snap[pp][1] = pp /\ ConsumeCheapW(s, ck).snap[pp] \notin minted))
    BY <2>3, <2>g, <1>c DEF Pending
  <2>8. Pending' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <2>8a, PendingWrite
  <2>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
    OBVIOUS
  <2>9b. ConsumeCheapW(s, ck).pc \in {"scanned", "claimed"} =>
            \A pp \in ConsumeCheapW(s, ck).upDone : ConsumeCheapW(s, ck).snap[pp][1] = pp /\
              \/ (ConsumeCheapW(s, ck).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
              \/ (ConsumeCheapW(s, ck).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                  /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
              \/ (ConsumeCheapW(s, ck).snap[pp] # Nil /\ ConsumeCheapW(s, ck).snap[pp] \notin minted /\ ConsumeCheapW(s, ck).snap[pp] \in upped' /\ ConsumeCheapW(s, ck).snap[pp] \notin {}
                  /\ \A k \in Paths : gw'[k] # ConsumeCheapW(s, ck).snap[pp])
    BY <2>3, <2>g, <1>c DEF Uploaded
  <2>9. Uploaded' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <1>r, <2>9a, <2>9b, UploadedWrite
  <2>10a. (ConsumeCheapW(s, ck).pc = "claimed" /\ ConsumeCheapW(s, ck).verified /\ ~ConsumeCheapW(s, ck).deposed) => \A pp \in (ConsumeCheapW(s, ck).uploads \cap ConsumeCheapW(s, ck).upDone) \ ConsumeCheapW(s, ck).gone : ConsumeCheapW(s, ck).snap[pp] \in live'
    BY <2>1, <2>3, <2>g, <1>c DEF Verified
  <2>10. Verified' BY <1>c, <1>m, <2>2, <2>e, <2>10a, VerifiedWrite
  <2>11a. ConsumeCheapW(s, ck).pc = "scanned" => ~ConsumeCheapW(s, ck).verified BY <2>3, <2>g, <1>c DEF Unverified
  <2>11. Unverified' BY <1>c, <2>2, <2>11a, UnverifiedWrite
  <2>12a. \A h \in {} : h \in live' OBVIOUS
  <2>12. Inv_CitationsLive' BY <1>c, <2>e, <2>12a, CLWrite
  <2>13. Inv_OneName' BY <2>1, <1>c DEF Inv_OneName
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13 DEF IndM2, M2
<1>2. CASE ~CheapPath(s)
  <2>1. PICK fail \in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <1>2, <1>ck DEF Consume
  <2>g. On(s) /\ w[s].pc = "idle" BY DEF Consume
  <2>2. Wr(s, ConsumeW(s, fail, ck)) BY <1>a, <2>1, WriteAny DEF Wr
  <2>3. /\ ConsumeW(s, fail, ck).pc = "consumed"
        /\ ConsumeW(s, fail, ck).uploads = w[s].uploads
        /\ ConsumeW(s, fail, ck).upDone = w[s].upDone
        /\ ConsumeW(s, fail, ck).gone = w[s].gone
        /\ ConsumeW(s, fail, ck).verified = w[s].verified
        /\ ConsumeW(s, fail, ck).local = [q \in Paths |-> IF q \in ConsumeTaken(s, fail) THEN doc[q] ELSE w[s].local[q]]
        /\ ConsumeW(s, fail, ck).snap = w[s].snap
        /\ ConsumeW(s, fail, ck).deposed = w[s].deposed
    BY DEF ConsumeW
  <2>4. Fresh' BY <1>c, <2>1, <1>r, FreshKeep
  <2>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, ConsumeW(s, fail, ck)) /\ LiveEv
        /\ minted \subseteq minted' /\ upped \subseteq upped'
    BY <2>1, <2>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt, CitedSet
  <2>5a. \A q \in Paths : ConsumeW(s, fail, ck).snap[q] \in Opt(minted') BY <2>1, <2>3, <1>c, <1>d, <2>e, <1>f DEF SnapMinted, Minted, Opt
  <2>5. SnapMinted' BY <1>c, <2>2, <2>e, <2>5a, SnapMintedWrite
  <2>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <2>1
  <2>6. Flight' BY <1>c, <1>d, <2>6a, <2>e, <1>r, <2>2, FlightKeep
  <2>7a. \A pp \in Paths : (ConsumeW(s, fail, ck).local[pp] # Nil /\ ConsumeW(s, fail, ck).local[pp] \notin upped') =>
            \/ ConsumeW(s, fail, ck).local[pp] = w[s].local[pp]
            \/ (ConsumeW(s, fail, ck).local[pp][1] = pp /\ ConsumeW(s, fail, ck).local[pp] \notin minted)
    BY <2>3, <2>e, <1>c, <1>f, <1>0 DEF Inv_CitationsLive, Fresh
  <2>7. Private' BY <1>c, <1>d, <2>2, <2>e, <2>7a, PrivateWrite
  <2>8a. ConsumeW(s, fail, ck).pc \in {"scanned", "claimed"} =>
            \A pp \in ConsumeW(s, fail, ck).uploads \ ConsumeW(s, fail, ck).upDone :
              /\ ConsumeW(s, fail, ck).snap[pp] # Nil
              /\ (ConsumeW(s, fail, ck).snap[pp] \notin upped' =>
                    \/ (ConsumeW(s, fail, ck).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                    \/ ConsumeW(s, fail, ck).snap[pp] = w[s].local[pp]
                    \/ (ConsumeW(s, fail, ck).snap[pp][1] = pp /\ ConsumeW(s, fail, ck).snap[pp] \notin minted))
    BY <2>3, <2>g, <1>c DEF Pending
  <2>8. Pending' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <2>8a, PendingWrite
  <2>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
    OBVIOUS
  <2>9b. ConsumeW(s, fail, ck).pc \in {"scanned", "claimed"} =>
            \A pp \in ConsumeW(s, fail, ck).upDone : ConsumeW(s, fail, ck).snap[pp][1] = pp /\
              \/ (ConsumeW(s, fail, ck).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
              \/ (ConsumeW(s, fail, ck).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                  /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
              \/ (ConsumeW(s, fail, ck).snap[pp] # Nil /\ ConsumeW(s, fail, ck).snap[pp] \notin minted /\ ConsumeW(s, fail, ck).snap[pp] \in upped' /\ ConsumeW(s, fail, ck).snap[pp] \notin {}
                  /\ \A k \in Paths : gw'[k] # ConsumeW(s, fail, ck).snap[pp])
    BY <2>3, <2>g, <1>c DEF Uploaded
  <2>9. Uploaded' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <1>r, <2>9a, <2>9b, UploadedWrite
  <2>10a. (ConsumeW(s, fail, ck).pc = "claimed" /\ ConsumeW(s, fail, ck).verified /\ ~ConsumeW(s, fail, ck).deposed) => \A pp \in (ConsumeW(s, fail, ck).uploads \cap ConsumeW(s, fail, ck).upDone) \ ConsumeW(s, fail, ck).gone : ConsumeW(s, fail, ck).snap[pp] \in live'
    BY <2>1, <2>3, <2>g, <1>c DEF Verified
  <2>10. Verified' BY <1>c, <1>m, <2>2, <2>e, <2>10a, VerifiedWrite
  <2>11a. ConsumeW(s, fail, ck).pc = "scanned" => ~ConsumeW(s, fail, ck).verified BY <2>3, <2>g, <1>c DEF Unverified
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Scan_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. PICK dels \in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s) : w' = [w EXCEPT ![s] = ScanW(s, dels)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY DEF Scan, bucket, aux
<1>g. On(s) /\ w[s].pc = "consumed" BY DEF Scan
<1>2. Wr(s, ScanW(s, dels)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ ScanW(s, dels).pc = "scanned"
      /\ ScanW(s, dels).uploads = ScanUps(s)
      /\ ScanW(s, dels).upDone = {}
      /\ ScanW(s, dels).gone = {}
      /\ ScanW(s, dels).verified = FALSE
      /\ ScanW(s, dels).local = w[s].local
      /\ ScanW(s, dels).snap = w[s].local
      /\ ScanW(s, dels).deposed = w[s].deposed
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
<1>10a. (ScanW(s, dels).pc = "claimed" /\ ScanW(s, dels).verified /\ ~ScanW(s, dels).deposed) => \A pp \in (ScanW(s, dels).uploads \cap ScanW(s, dels).upDone) \ ScanW(s, dels).gone : ScanW(s, dels).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ SkipW(s).deposed = w[s].deposed
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
<1>10a. (SkipW(s).pc = "claimed" /\ SkipW(s).verified /\ ~SkipW(s).deposed) => \A pp \in (SkipW(s).uploads \cap SkipW(s).upDone) \ SkipW(s).gone : SkipW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
        /\ UploadW(s, p).deposed = w[s].deposed
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
  <2>10a. (UploadW(s, p).pc = "claimed" /\ UploadW(s, p).verified /\ ~UploadW(s, p).deposed) => \A pp \in (UploadW(s, p).uploads \cap UploadW(s, p).upDone) \ UploadW(s, p).gone : UploadW(s, p).snap[pp] \in live'
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
        /\ UploadCopyW(s, p, c).deposed = w[s].deposed
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
  <2>10a. (UploadCopyW(s, p, c).pc = "claimed" /\ UploadCopyW(s, p, c).verified /\ ~UploadCopyW(s, p, c).deposed) => \A pp \in (UploadCopyW(s, p, c).uploads \cap UploadCopyW(s, p, c).upDone) \ UploadCopyW(s, p, c).gone : UploadCopyW(s, p, c).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ PullOnlyW(s).deposed = w[s].deposed
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
<1>10a. (PullOnlyW(s).pc = "claimed" /\ PullOnlyW(s).verified /\ ~PullOnlyW(s).deposed) => \A pp \in (PullOnlyW(s).uploads \cap PullOnlyW(s).upDone) \ PullOnlyW(s).gone : PullOnlyW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ ClaimW(s).deposed = w[s].deposed
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
<1>10a. (ClaimW(s).pc = "claimed" /\ ClaimW(s).verified /\ ~ClaimW(s).deposed) => \A pp \in (ClaimW(s).uploads \cap ClaimW(s).upDone) \ ClaimW(s).gone : ClaimW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ VerifyW(s).deposed = w[s].deposed
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
<1>10a. (VerifyW(s).pc = "claimed" /\ VerifyW(s).verified /\ ~VerifyW(s).deposed) => \A pp \in (VerifyW(s).uploads \cap VerifyW(s).upDone) \ VerifyW(s).gone : VerifyW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
<1>k. holder = s /\ ~w[s].deposed BY <1>s2, <1>m DEF Install, Dep
<1>j. \A k \in InstallMine(s) \ w[s].gone : w[s].snap[k][1] = k /\ ~Cited(w[s].snap[k]) /\ w[s].snap[k] # Nil /\ w[s].snap[k] \in live
  BY <1>c, <1>k DEF Uploaded, Up, Verified, InstallMine, Install
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
      /\ InstallW(s).deposed = w[s].deposed
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
<1>10a. (InstallW(s).pc = "claimed" /\ InstallW(s).verified /\ ~InstallW(s).deposed) => \A pp \in (InstallW(s).uploads \cap InstallW(s).upDone) \ InstallW(s).gone : InstallW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ CollectW(s).deposed = w[s].deposed
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
<1>10a. (CollectW(s).pc = "claimed" /\ CollectW(s).verified /\ ~CollectW(s).deposed) => \A pp \in (CollectW(s).uploads \cap CollectW(s).upDone) \ CollectW(s).gone : CollectW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ FinishW(s).deposed = FALSE
  <2>1. FinishW(s).pc = "idle" BY DEF FinishW
  <2>2. FinishW(s).uploads = {} BY DEF FinishW
  <2>3. FinishW(s).upDone = {} BY DEF FinishW
  <2>4. FinishW(s).gone = {} BY DEF FinishW
  <2>5. FinishW(s).verified = FALSE BY DEF FinishW
  <2>6. FinishW(s).local = w[s].local BY DEF FinishW
  <2>7. FinishW(s).snap = [q \in Paths |-> Nil] BY DEF FinishW
  <2>8. FinishW(s).deposed = FALSE BY DEF FinishW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8
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
<1>10a. (FinishW(s).pc = "claimed" /\ FinishW(s).verified /\ ~FinishW(s).deposed) => \A pp \in (FinishW(s).uploads \cap FinishW(s).upDone) \ FinishW(s).gone : FinishW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ RestartW(s).deposed = FALSE
  <2>1. RestartW(s).pc = "idle" BY DEF RestartW
  <2>2. RestartW(s).uploads = {} BY DEF RestartW
  <2>3. RestartW(s).upDone = {} BY DEF RestartW
  <2>4. RestartW(s).gone = {} BY DEF RestartW
  <2>5. RestartW(s).verified = FALSE BY DEF RestartW
  <2>6. RestartW(s).local = w[s].local BY DEF RestartW
  <2>7. RestartW(s).snap = [q \in Paths |-> Nil] BY DEF RestartW
  <2>8. RestartW(s).deposed = FALSE BY DEF RestartW
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <2>5, <2>6, <2>7, <2>8
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
<1>10a. (RestartW(s).pc = "claimed" /\ RestartW(s).verified /\ ~RestartW(s).deposed) => \A pp \in (RestartW(s).uploads \cap RestartW(s).upDone) \ RestartW(s).gone : RestartW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ SyncW(s, fail).deposed = w[s].deposed
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
<1>10a. (SyncW(s, fail).pc = "claimed" /\ SyncW(s, fail).verified /\ ~SyncW(s, fail).deposed) => \A pp \in (SyncW(s, fail).uploads \cap SyncW(s, fail).upDone) \ SyncW(s, fail).gone : SyncW(s, fail).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ RescopeBeginW(s, T).deposed = w[s].deposed
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
<1>10a. (RescopeBeginW(s, T).pc = "claimed" /\ RescopeBeginW(s, T).verified /\ ~RescopeBeginW(s, T).deposed) => \A pp \in (RescopeBeginW(s, T).uploads \cap RescopeBeginW(s, T).upDone) \ RescopeBeginW(s, T).gone : RescopeBeginW(s, T).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ RescopeFirstW(s).deposed = w[s].deposed
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
<1>10a. (RescopeFirstW(s).pc = "claimed" /\ RescopeFirstW(s).verified /\ ~RescopeFirstW(s).deposed) => \A pp \in (RescopeFirstW(s).uploads \cap RescopeFirstW(s).upDone) \ RescopeFirstW(s).gone : RescopeFirstW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ RescopeSecondW(s, wfail).deposed = w[s].deposed
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
<1>10a. (RescopeSecondW(s, wfail).pc = "claimed" /\ RescopeSecondW(s, wfail).verified /\ ~RescopeSecondW(s, wfail).deposed) => \A pp \in (RescopeSecondW(s, wfail).uploads \cap RescopeSecondW(s, wfail).upDone) \ RescopeSecondW(s, wfail).gone : RescopeSecondW(s, wfail).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ RPullReadW(s).deposed = w[s].deposed
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
<1>10a. (RPullReadW(s).pc = "claimed" /\ RPullReadW(s).verified /\ ~RPullReadW(s).deposed) => \A pp \in (RPullReadW(s).uploads \cap RPullReadW(s).upDone) \ RPullReadW(s).gone : RPullReadW(s).snap[pp] \in live'
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
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
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
      /\ RPullSyncW(s, fail).deposed = w[s].deposed
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
<1>10a. (RPullSyncW(s, fail).pc = "claimed" /\ RPullSyncW(s, fail).verified /\ ~RPullSyncW(s, fail).deposed) => \A pp \in (RPullSyncW(s, fail).uploads \cap RPullSyncW(s, fail).upDone) \ RPullSyncW(s, fail).gone : RPullSyncW(s, fail).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. RPullSyncW(s, fail).pc = "scanned" => ~RPullSyncW(s, fail).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

------------------------------------------------------------------------------
(* The answer (row 15).                                                     *)

LEMMA Touch_M2 ==
  ASSUME IndM2, NEW s \in Writers, Touch(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Touch_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = TouchW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>
  <2>1. w' = [w EXCEPT ![s] = TouchW(s)] BY DEF Touch
  <2>0. UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>> BY DEF Touch
  <2>2. UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <2>0 DEF bucket, aux
  <2> QED BY <2>1, <2>2
<1>g. On(s) /\ w[s].pc = "idle" BY DEF Touch
<1>2. Wr(s, TouchW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ TouchW(s).pc = w[s].pc
      /\ TouchW(s).uploads = w[s].uploads
      /\ TouchW(s).upDone = w[s].upDone
      /\ TouchW(s).gone = w[s].gone
      /\ TouchW(s).verified = w[s].verified
      /\ TouchW(s).local = w[s].local
      /\ TouchW(s).snap = w[s].snap
      /\ TouchW(s).deposed = w[s].deposed
  BY DEF TouchW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, TouchW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : TouchW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (TouchW(s).local[pp] # Nil /\ TouchW(s).local[pp] \notin upped') =>
          \/ TouchW(s).local[pp] = w[s].local[pp]
          \/ (TouchW(s).local[pp][1] = pp /\ TouchW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. TouchW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in TouchW(s).uploads \ TouchW(s).upDone :
            /\ TouchW(s).snap[pp] # Nil
            /\ (TouchW(s).snap[pp] \notin upped' =>
                  \/ (TouchW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ TouchW(s).snap[pp] = w[s].local[pp]
                  \/ (TouchW(s).snap[pp][1] = pp /\ TouchW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. TouchW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in TouchW(s).upDone : TouchW(s).snap[pp][1] = pp /\
            \/ (TouchW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (TouchW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (TouchW(s).snap[pp] # Nil /\ TouchW(s).snap[pp] \notin minted /\ TouchW(s).snap[pp] \in upped' /\ TouchW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # TouchW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (TouchW(s).pc = "claimed" /\ TouchW(s).verified /\ ~TouchW(s).deposed) => \A pp \in (TouchW(s).uploads \cap TouchW(s).upDone) \ TouchW(s).gone : TouchW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. TouchW(s).pc = "scanned" => ~TouchW(s).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA Honor_M2 ==
  ASSUME IndM2, NEW s \in Writers, Honor(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Honor_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = HonorW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>
  <2>1. w' = [w EXCEPT ![s] = HonorW(s)] BY DEF Honor
  <2>0. UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>> BY DEF Honor
  <2>2. UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <2>0 DEF bucket, aux
  <2> QED BY <2>1, <2>2
<1>g. On(s) /\ w[s].pc = "idle" BY DEF Honor
<1>2. Wr(s, HonorW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ HonorW(s).pc = w[s].pc
      /\ HonorW(s).uploads = w[s].uploads
      /\ HonorW(s).upDone = w[s].upDone
      /\ HonorW(s).gone = w[s].gone
      /\ HonorW(s).verified = w[s].verified
      /\ HonorW(s).local = w[s].local
      /\ HonorW(s).snap = w[s].snap
      /\ HonorW(s).deposed = w[s].deposed
  BY DEF HonorW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, HonorW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : HonorW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (HonorW(s).local[pp] # Nil /\ HonorW(s).local[pp] \notin upped') =>
          \/ HonorW(s).local[pp] = w[s].local[pp]
          \/ (HonorW(s).local[pp][1] = pp /\ HonorW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. HonorW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in HonorW(s).uploads \ HonorW(s).upDone :
            /\ HonorW(s).snap[pp] # Nil
            /\ (HonorW(s).snap[pp] \notin upped' =>
                  \/ (HonorW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ HonorW(s).snap[pp] = w[s].local[pp]
                  \/ (HonorW(s).snap[pp][1] = pp /\ HonorW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. HonorW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in HonorW(s).upDone : HonorW(s).snap[pp][1] = pp /\
            \/ (HonorW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (HonorW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (HonorW(s).snap[pp] # Nil /\ HonorW(s).snap[pp] \notin minted /\ HonorW(s).snap[pp] \in upped' /\ HonorW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # HonorW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (HonorW(s).pc = "claimed" /\ HonorW(s).verified /\ ~HonorW(s).deposed) => \A pp \in (HonorW(s).uploads \cap HonorW(s).upDone) \ HonorW(s).gone : HonorW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. HonorW(s).pc = "scanned" => ~HonorW(s).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA HonorReader_M2 ==
  ASSUME IndM2, NEW s \in Writers, HonorReader(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, HonorReader_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>
  <2>1. w' = [w EXCEPT ![s] = HonorReaderW(s)] BY DEF HonorReader
  <2>0. UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>> BY DEF HonorReader
  <2>2. UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <2>0 DEF bucket, aux
  <2> QED BY <2>1, <2>2
<1>g. On(s) /\ w[s].pc = "idle" BY DEF HonorReader
<1>2. Wr(s, HonorReaderW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ HonorReaderW(s).pc = w[s].pc
      /\ HonorReaderW(s).uploads = w[s].uploads
      /\ HonorReaderW(s).upDone = w[s].upDone
      /\ HonorReaderW(s).gone = w[s].gone
      /\ HonorReaderW(s).verified = w[s].verified
      /\ HonorReaderW(s).local = w[s].local
      /\ HonorReaderW(s).snap = w[s].snap
      /\ HonorReaderW(s).deposed = w[s].deposed
  BY DEF HonorReaderW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, HonorReaderW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : HonorReaderW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (HonorReaderW(s).local[pp] # Nil /\ HonorReaderW(s).local[pp] \notin upped') =>
          \/ HonorReaderW(s).local[pp] = w[s].local[pp]
          \/ (HonorReaderW(s).local[pp][1] = pp /\ HonorReaderW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. HonorReaderW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in HonorReaderW(s).uploads \ HonorReaderW(s).upDone :
            /\ HonorReaderW(s).snap[pp] # Nil
            /\ (HonorReaderW(s).snap[pp] \notin upped' =>
                  \/ (HonorReaderW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ HonorReaderW(s).snap[pp] = w[s].local[pp]
                  \/ (HonorReaderW(s).snap[pp][1] = pp /\ HonorReaderW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. HonorReaderW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in HonorReaderW(s).upDone : HonorReaderW(s).snap[pp][1] = pp /\
            \/ (HonorReaderW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (HonorReaderW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (HonorReaderW(s).snap[pp] # Nil /\ HonorReaderW(s).snap[pp] \notin minted /\ HonorReaderW(s).snap[pp] \in upped' /\ HonorReaderW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # HonorReaderW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (HonorReaderW(s).pc = "claimed" /\ HonorReaderW(s).verified /\ ~HonorReaderW(s).deposed) => \A pp \in (HonorReaderW(s).uploads \cap HonorReaderW(s).upDone) \ HonorReaderW(s).gone : HonorReaderW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. HonorReaderW(s).pc = "scanned" => ~HonorReaderW(s).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

LEMMA RetireByAck_M2 ==
  ASSUME IndM2, NEW s \in Writers, RetireByAck(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, RetireByAck_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>
  <2>1. w' = [w EXCEPT ![s] = RetireByAckW(s)] BY DEF RetireByAck
  <2>0. UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>> BY DEF RetireByAck
  <2>2. UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <2>0 DEF bucket, aux
  <2> QED BY <2>1, <2>2
<1>g. On(s) /\ w[s].pc = "idle" BY DEF RetireByAck
<1>2. Wr(s, RetireByAckW(s)) BY <1>a, <1>1, WriteAny DEF Wr
<1>3. /\ RetireByAckW(s).pc = w[s].pc
      /\ RetireByAckW(s).uploads = w[s].uploads
      /\ RetireByAckW(s).upDone = w[s].upDone
      /\ RetireByAckW(s).gone = w[s].gone
      /\ RetireByAckW(s).verified = w[s].verified
      /\ RetireByAckW(s).local = w[s].local
      /\ RetireByAckW(s).snap = w[s].snap
      /\ RetireByAckW(s).deposed = w[s].deposed
  BY DEF RetireByAckW
<1>4. Fresh' BY <1>c, <1>1, <1>r, FreshKeep
<1>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, RetireByAckW(s)) /\ LiveEv
      /\ minted \subseteq minted' /\ upped \subseteq upped'
  BY <1>1, <1>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
<1>5a. \A q \in Paths : RetireByAckW(s).snap[q] \in Opt(minted') BY <1>1, <1>3, <1>c, <1>d, <1>e, <1>f DEF SnapMinted, Minted, Opt
<1>5. SnapMinted' BY <1>c, <1>2, <1>e, <1>5a, SnapMintedWrite
<1>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <1>1
<1>6. Flight' BY <1>c, <1>d, <1>6a, <1>e, <1>r, <1>2, FlightKeep
<1>7a. \A pp \in Paths : (RetireByAckW(s).local[pp] # Nil /\ RetireByAckW(s).local[pp] \notin upped') =>
          \/ RetireByAckW(s).local[pp] = w[s].local[pp]
          \/ (RetireByAckW(s).local[pp][1] = pp /\ RetireByAckW(s).local[pp] \notin minted)
  BY <1>3
<1>7. Private' BY <1>c, <1>d, <1>2, <1>e, <1>7a, PrivateWrite
<1>8a. RetireByAckW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in RetireByAckW(s).uploads \ RetireByAckW(s).upDone :
            /\ RetireByAckW(s).snap[pp] # Nil
            /\ (RetireByAckW(s).snap[pp] \notin upped' =>
                  \/ (RetireByAckW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                  \/ RetireByAckW(s).snap[pp] = w[s].local[pp]
                  \/ (RetireByAckW(s).snap[pp][1] = pp /\ RetireByAckW(s).snap[pp] \notin minted))
  BY <1>3, <1>g, <1>c DEF Pending
<1>8. Pending' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>8a, PendingWrite
<1>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
  OBVIOUS
<1>9b. RetireByAckW(s).pc \in {"scanned", "claimed"} =>
          \A pp \in RetireByAckW(s).upDone : RetireByAckW(s).snap[pp][1] = pp /\
            \/ (RetireByAckW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
            \/ (RetireByAckW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
            \/ (RetireByAckW(s).snap[pp] # Nil /\ RetireByAckW(s).snap[pp] \notin minted /\ RetireByAckW(s).snap[pp] \in upped' /\ RetireByAckW(s).snap[pp] \notin {}
                /\ \A k \in Paths : gw'[k] # RetireByAckW(s).snap[pp])
  BY <1>3, <1>g, <1>c DEF Uploaded
<1>9. Uploaded' BY <1>c, <1>d, <1>t2, <1>2, <1>e, <1>r, <1>9a, <1>9b, UploadedWrite
<1>10a. (RetireByAckW(s).pc = "claimed" /\ RetireByAckW(s).verified /\ ~RetireByAckW(s).deposed) => \A pp \in (RetireByAckW(s).uploads \cap RetireByAckW(s).upDone) \ RetireByAckW(s).gone : RetireByAckW(s).snap[pp] \in live'
  BY <1>1, <1>3, <1>g, <1>c DEF Verified
<1>10. Verified' BY <1>c, <1>m, <1>2, <1>e, <1>10a, VerifiedWrite
<1>11a. RetireByAckW(s).pc = "scanned" => ~RetireByAckW(s).verified BY <1>3, <1>g, <1>c DEF Unverified
<1>11. Unverified' BY <1>c, <1>2, <1>11a, UnverifiedWrite
<1>12a. \A h \in {} : h \in live' OBVIOUS
<1>12. Inv_CitationsLive' BY <1>c, <1>e, <1>12a, CLWrite
<1>13. Inv_OneName' BY <1>1, <1>c DEF Inv_OneName
<1>. QED BY <1>t, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13 DEF IndM2, M2

------------------------------------------------------------------------------
(* Deposal (row 17).                                                        *)

\* Two trees move, each in its pc and marks only: no handle changes hands.
LEMMA Depose_M2 ==
  ASSUME IndM2, NEW t \in Writers, Depose(t), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Depose_M1
<1>1. /\ w[t].pc = "scanned" /\ holder \in Writers /\ holder # t
      /\ w' = [w EXCEPT ![t] = DeposeW(t), ![holder] = DeposedW(holder)]
      /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>
  BY DEF Depose, aux
<1>2. \A u \in Writers : w'[u] = IF u = t THEN DeposeW(t) ELSE IF u = holder THEN DeposedW(holder) ELSE w[u]
  BY <1>a, <1>1 DEF TypeOK
<1>3. \A u \in Writers : /\ w'[u].local = w[u].local /\ w'[u].snap = w[u].snap
                         /\ w'[u].uploads = w[u].uploads /\ w'[u].upDone = w[u].upDone
                         /\ w'[u].gone = w[u].gone /\ w'[u].verified = w[u].verified
  BY <1>2 DEF DeposeW, DeposedW
<1>4. /\ w'[t].pc = "claimed" /\ w'[t].deposed = w[t].deposed
      /\ w'[holder].pc = w[holder].pc /\ w'[holder].deposed = TRUE
      /\ \A u \in Writers \ {t, holder} : w'[u] = w[u]
  BY <1>1, <1>2 DEF DeposeW, DeposedW
<1>r2. retiring' = retiring /\ aged' = aged
  \* EC2 10-15: 140% of the limit in one step; the document is kept, so RetUpdate keeps retiring.
  <2>1. doc' = doc BY <1>1
  <2>2. RetUpdate /\ aged' = aged BY DEF Frame
  <2>3. retiring' = retiring BY <2>1, <2>2 DEF RetUpdate
  <2> QED BY <2>2, <2>3
<1>5. ~w[t].verified BY <1>1, <1>c DEF Unverified
<1>6. Fresh' /\ SnapMinted' /\ Flight' BY <1>c, <1>1, <1>3, <1>r2 DEF Fresh, SnapMinted, Flight, Cited
<1>7. Private' BY <1>c, <1>1, <1>3, <1>r2 DEF Private, Priv
<1>8. Pending' BY <1>c, <1>1, <1>3, <1>4 DEF Pending, Priv
<1>9. Uploaded' BY <1>c, <1>1, <1>3, <1>4, <1>r2 DEF Uploaded, Up, Priv, Cited
<1>10. Verified' BY <1>c, <1>1, <1>3, <1>4, <1>5 DEF Verified
<1>11. Unverified' BY <1>c, <1>1, <1>3, <1>4 DEF Unverified
<1>12. Inv_CitationsLive' /\ Inv_OneName' BY <1>c, <1>1 DEF Inv_CitationsLive, Inv_OneName
<1>. QED BY <1>t, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12 DEF IndM2, M2

LEMMA Fence_M2 ==
  ASSUME IndM2, NEW s \in Writers, Fence(s), Frame
  PROVE  IndM2'
<1>a. IndM1 /\ IndTypeOK /\ TypeOK BY DEF IndM2, IndM1, IndTypeOK
<1>m. Inv_OneHolder /\ Holder /\ Dep /\ Mine /\ Ups /\ Rescope /\ R1 BY DEF IndM2, IndM1, M1, Rescope
<1>c. /\ Fresh /\ SnapMinted /\ Flight /\ Private /\ Pending /\ Uploaded /\ Verified /\ Unverified
      /\ Inv_CitationsLive /\ Inv_OneName
  BY DEF IndM2, M2
<1>d. Minted /\ Ghosts BY <1>a DEF IndTypeOK
<1>0. /\ gw \in [Paths -> Opt(Handles)] /\ doc \in [Paths -> Opt(Handles)] /\ nextGen \in Nat
      /\ holder \in Writers \cup {"none"} /\ live \subseteq Handles /\ minted \subseteq Handles
  BY <1>a DEF TypeOK
<1>s. RetireAge /\ SweepUnderLease /\ GatewaySweepGrace /\ CommitVerifiesUploads /\ RescopeUnciteFirst
  BY ShippedShape DEF Shipped
<1>s2. InstallFenced /\ DeletesUnderLease BY ShippedShape DEF Shipped
<1>r. RetEv /\ aged' = aged BY <1>s, RetFacts DEF Frame
<1>t. IndM1' BY <1>a, Fence_M1
<1>t2. w' \in [Writers -> Writer] BY <1>t DEF IndM1, IndTypeOK, TypeOK
<1>w. w[s] \in Writer BY <1>a DEF TypeOK
<1>f. /\ w[s].local \in [Paths -> Opt(Handles)] /\ w[s].snap \in [Paths -> Opt(Handles)]
      /\ w[s].uploads \subseteq Paths /\ w[s].upDone \subseteq Paths /\ w[s].deletes \subseteq Paths /\ w[s].gone \subseteq Paths
      /\ w[s].pc \in {"idle", "consumed", "scanned", "claimed", "cased", "pulling"} /\ w[s].verified \in BOOLEAN
  BY <1>w, WriterFields
<1>1. CASE w[s].pc = "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>
    <3>1. w' = [w EXCEPT ![s] = FenceBeforeW(s)] BY <1>1 DEF Fence
    <3>0. UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>> BY DEF Fence
    <3>2. UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <3>0 DEF bucket, aux
    <3> QED BY <3>1, <3>2
  <2>g. On(s) BY DEF Fence
  <2>2. Wr(s, FenceBeforeW(s)) BY <1>a, <2>1, WriteAny DEF Wr
  <2>3. /\ FenceBeforeW(s).pc = "idle"
        /\ FenceBeforeW(s).uploads = {}
        /\ FenceBeforeW(s).upDone = {}
        /\ FenceBeforeW(s).gone = {}
        /\ FenceBeforeW(s).verified = FALSE
        /\ FenceBeforeW(s).local = w[s].local
        /\ FenceBeforeW(s).snap = [q \in Paths |-> Nil]
        /\ FenceBeforeW(s).deposed = FALSE
    BY DEF FenceBeforeW
  <2>4. Fresh' BY <1>c, <2>1, <1>r, FreshKeep
  <2>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, FenceBeforeW(s)) /\ LiveEv
        /\ minted \subseteq minted' /\ upped \subseteq upped'
    BY <2>1, <2>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
  <2>5a. \A q \in Paths : FenceBeforeW(s).snap[q] \in Opt(minted') BY <2>1, <2>3, <1>c, <1>d, <2>e, <1>f DEF SnapMinted, Minted, Opt
  <2>5. SnapMinted' BY <1>c, <2>2, <2>e, <2>5a, SnapMintedWrite
  <2>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <2>1
  <2>6. Flight' BY <1>c, <1>d, <2>6a, <2>e, <1>r, <2>2, FlightKeep
  <2>7a. \A pp \in Paths : (FenceBeforeW(s).local[pp] # Nil /\ FenceBeforeW(s).local[pp] \notin upped') =>
            \/ FenceBeforeW(s).local[pp] = w[s].local[pp]
            \/ (FenceBeforeW(s).local[pp][1] = pp /\ FenceBeforeW(s).local[pp] \notin minted)
    BY <2>3
  <2>7. Private' BY <1>c, <1>d, <2>2, <2>e, <2>7a, PrivateWrite
  <2>8a. FenceBeforeW(s).pc \in {"scanned", "claimed"} =>
            \A pp \in FenceBeforeW(s).uploads \ FenceBeforeW(s).upDone :
              /\ FenceBeforeW(s).snap[pp] # Nil
              /\ (FenceBeforeW(s).snap[pp] \notin upped' =>
                    \/ (FenceBeforeW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                    \/ FenceBeforeW(s).snap[pp] = w[s].local[pp]
                    \/ (FenceBeforeW(s).snap[pp][1] = pp /\ FenceBeforeW(s).snap[pp] \notin minted))
    BY <2>3, <2>g, <1>c DEF Pending
  <2>8. Pending' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <2>8a, PendingWrite
  <2>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
    OBVIOUS
  <2>9b. FenceBeforeW(s).pc \in {"scanned", "claimed"} =>
            \A pp \in FenceBeforeW(s).upDone : FenceBeforeW(s).snap[pp][1] = pp /\
              \/ (FenceBeforeW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
              \/ (FenceBeforeW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                  /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
              \/ (FenceBeforeW(s).snap[pp] # Nil /\ FenceBeforeW(s).snap[pp] \notin minted /\ FenceBeforeW(s).snap[pp] \in upped' /\ FenceBeforeW(s).snap[pp] \notin {}
                  /\ \A k \in Paths : gw'[k] # FenceBeforeW(s).snap[pp])
    BY <2>3, <2>g, <1>c DEF Uploaded
  <2>9. Uploaded' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <1>r, <2>9a, <2>9b, UploadedWrite
  <2>10a. (FenceBeforeW(s).pc = "claimed" /\ FenceBeforeW(s).verified /\ ~FenceBeforeW(s).deposed) => \A pp \in (FenceBeforeW(s).uploads \cap FenceBeforeW(s).upDone) \ FenceBeforeW(s).gone : FenceBeforeW(s).snap[pp] \in live'
    BY <2>1, <2>3, <2>g, <1>c DEF Verified
  <2>10. Verified' BY <1>c, <1>m, <2>2, <2>e, <2>10a, VerifiedWrite
  <2>11a. FenceBeforeW(s).pc = "scanned" => ~FenceBeforeW(s).verified BY <2>3
  <2>11. Unverified' BY <1>c, <2>2, <2>11a, UnverifiedWrite
  <2>12a. \A h \in {} : h \in live' OBVIOUS
  <2>12. Inv_CitationsLive' BY <1>c, <2>e, <2>12a, CLWrite
  <2>13. Inv_OneName' BY <2>1, <1>c DEF Inv_OneName
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13 DEF IndM2, M2
<1>2. CASE w[s].pc # "claimed"
  <2>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] /\ UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>>
    <3>1. w' = [w EXCEPT ![s] = FenceAfterW(s)] BY <1>2 DEF Fence
    <3>0. UNCHANGED <<bucket, nextGen, ui, reqs, barriers, aux>> BY DEF Fence
    <3>2. UNCHANGED <<live, minted, doc, gw, nextGen, copies, upped>> BY <3>0 DEF bucket, aux
    <3> QED BY <3>1, <3>2
  <2>g. On(s) BY DEF Fence
  <2>2. Wr(s, FenceAfterW(s)) BY <1>a, <2>1, WriteAny DEF Wr
  <2>3. /\ FenceAfterW(s).pc = w[s].pc
        /\ FenceAfterW(s).uploads = w[s].uploads
        /\ FenceAfterW(s).upDone = w[s].upDone
        /\ FenceAfterW(s).gone = w[s].gone
        /\ FenceAfterW(s).verified = w[s].verified
        /\ FenceAfterW(s).local = w[s].local
        /\ FenceAfterW(s).snap = w[s].snap
        /\ FenceAfterW(s).deposed = w[s].deposed
    BY DEF FenceAfterW
  <2>4. Fresh' BY <1>c, <2>1, <1>r, FreshKeep
  <2>e. /\ DocEv({}) /\ GwEv /\ TreeEv(s, FenceAfterW(s)) /\ LiveEv
        /\ minted \subseteq minted' /\ upped \subseteq upped'
    BY <2>1, <2>3, <1>0, <1>d, <1>f DEF DocEv, GwEv, TreeEv, LiveEv, Minted, Opt
  <2>5a. \A q \in Paths : FenceAfterW(s).snap[q] \in Opt(minted') BY <2>1, <2>3, <1>c, <1>d, <2>e, <1>f DEF SnapMinted, Minted, Opt
  <2>5. SnapMinted' BY <1>c, <2>2, <2>e, <2>5a, SnapMintedWrite
  <2>6a. \A p \in Paths : gw'[p] # Nil => gw'[p] = gw[p] /\ gw[p] \notin {} BY <2>1
  <2>6. Flight' BY <1>c, <1>d, <2>6a, <2>e, <1>r, <2>2, FlightKeep
  <2>7a. \A pp \in Paths : (FenceAfterW(s).local[pp] # Nil /\ FenceAfterW(s).local[pp] \notin upped') =>
            \/ FenceAfterW(s).local[pp] = w[s].local[pp]
            \/ (FenceAfterW(s).local[pp][1] = pp /\ FenceAfterW(s).local[pp] \notin minted)
    BY <2>3
  <2>7. Private' BY <1>c, <1>d, <2>2, <2>e, <2>7a, PrivateWrite
  <2>8a. FenceAfterW(s).pc \in {"scanned", "claimed"} =>
            \A pp \in FenceAfterW(s).uploads \ FenceAfterW(s).upDone :
              /\ FenceAfterW(s).snap[pp] # Nil
              /\ (FenceAfterW(s).snap[pp] \notin upped' =>
                    \/ (FenceAfterW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone)
                    \/ FenceAfterW(s).snap[pp] = w[s].local[pp]
                    \/ (FenceAfterW(s).snap[pp][1] = pp /\ FenceAfterW(s).snap[pp] \notin minted))
    BY <2>3, <2>g, <1>c DEF Pending
  <2>8. Pending' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <2>8a, PendingWrite
  <2>9a. \A u \in Writers, pp \in Paths : (u # s /\ w[u].pc \in {"scanned", "claimed"} /\ pp \in w[u].upDone) => w[u].snap[pp] \notin {}
    OBVIOUS
  <2>9b. FenceAfterW(s).pc \in {"scanned", "claimed"} =>
            \A pp \in FenceAfterW(s).upDone : FenceAfterW(s).snap[pp][1] = pp /\
              \/ (FenceAfterW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].upDone /\ w[s].snap[pp] \notin {})
              \/ (FenceAfterW(s).snap[pp] = w[s].snap[pp] /\ w[s].pc \in {"scanned", "claimed"} /\ pp \in w[s].uploads \ w[s].upDone
                  /\ w[s].snap[pp] \notin upped /\ w[s].snap[pp] \in upped' /\ w[s].snap[pp] \notin {})
              \/ (FenceAfterW(s).snap[pp] # Nil /\ FenceAfterW(s).snap[pp] \notin minted /\ FenceAfterW(s).snap[pp] \in upped' /\ FenceAfterW(s).snap[pp] \notin {}
                  /\ \A k \in Paths : gw'[k] # FenceAfterW(s).snap[pp])
    BY <2>3, <2>g, <1>c DEF Uploaded
  <2>9. Uploaded' BY <1>c, <1>d, <1>t2, <2>2, <2>e, <1>r, <2>9a, <2>9b, UploadedWrite
  <2>10a. (FenceAfterW(s).pc = "claimed" /\ FenceAfterW(s).verified /\ ~FenceAfterW(s).deposed) => \A pp \in (FenceAfterW(s).uploads \cap FenceAfterW(s).upDone) \ FenceAfterW(s).gone : FenceAfterW(s).snap[pp] \in live'
    BY <2>1, <2>3, <2>g, <1>c DEF Verified
  <2>10. Verified' BY <1>c, <1>m, <2>2, <2>e, <2>10a, VerifiedWrite
  <2>11a. FenceAfterW(s).pc = "scanned" => ~FenceAfterW(s).verified BY <2>3, <2>g, <1>c DEF Unverified
  <2>11. Unverified' BY <1>c, <2>2, <2>11a, UnverifiedWrite
  <2>12a. \A h \in {} : h \in live' OBVIOUS
  <2>12. Inv_CitationsLive' BY <1>c, <2>e, <2>12a, CLWrite
  <2>13. Inv_OneName' BY <2>1, <1>c DEF Inv_OneName
  <2>. QED BY <1>t, <2>4, <2>5, <2>6, <2>7, <2>8, <2>9, <2>10, <2>11, <2>12, <2>13 DEF IndM2, M2
<1>. QED BY <1>1, <1>2

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
    <3>22. ASSUME NEW s \in Writers, Touch(s) PROVE IndM2' BY <3>22, Touch_M2
    <3>23. ASSUME NEW s \in Writers, Honor(s) PROVE IndM2' BY <3>23, Honor_M2
    <3>24. ASSUME NEW s \in Writers, HonorReader(s) PROVE IndM2' BY <3>24, HonorReader_M2
    <3>25. ASSUME NEW s \in Writers, RetireByAck(s) PROVE IndM2' BY <3>25, RetireByAck_M2
    <3>26. ASSUME NEW s \in Writers, Depose(s) PROVE IndM2' BY <3>26, Depose_M2
    <3>27. ASSUME NEW s \in Writers, Fence(s) PROVE IndM2' BY <3>27, Fence_M2
    <3>. QED BY <2>2, <3>1, <3>2, <3>3, <3>4, <3>5, <3>6, <3>7, <3>8, <3>9, <3>10,
                <3>11, <3>12, <3>13, <3>14, <3>15, <3>16, <3>17, <3>18, <3>19, <3>20, <3>21,
                <3>22, <3>23, <3>24, <3>25, <3>26, <3>27
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

==============================================================================
