#!/usr/bin/env python3
"""sync-conv-patch.py -- the proof sources' edits for the sync's convergence
(2026-10-17: LeanP1.tla's Sync and RPullSync take `owed \\cup {converging}`, as
the consume does; make-anc.py names it SyncTaken = SyncOwed \\cup SyncConv).
Run once from this directory, on the sources as pushed at 68fa0d6e; every edit
asserts its anchor occurs exactly once (or the count it names)."""
import sys


def edit(path, pairs):
    t = open(path).read()
    for item in pairs:
        o, n = item[0], item[1]
        k = item[2] if len(item) > 2 else 1
        assert t.count(o) == k, (path, t.count(o), k, o[:90])
        t = t.replace(o, n)
    open(path, 'w').write(t)
    print('patched', path)


# M0 (typing): the taken set is a set of paths; integrated grows by its gens.
edit('gen-m0.py', [
    ("[('fail \\\\subseteq Paths /\\\\ SyncOwed(s, fail) \\\\subseteq Paths', 'DEF SyncAll, SyncOwed'),",
     "[('fail \\\\subseteq Paths /\\\\ SyncOwed(s, fail) \\\\subseteq Paths /\\\\ SyncTaken(s, fail) \\\\subseteq Paths',\n"
     "                  'DEF SyncAll, SyncOwed, SyncConv, SyncTaken'),"),
    ("('{Gen(doc[p]) : p \\\\in {q \\\\in SyncOwed(s, fail) : doc[q] # Nil}} \\\\subseteq Gens', '<1>a, <1>3, DocGens'),",
     "('{Gen(doc[p]) : p \\\\in {q \\\\in SyncTaken(s, fail) : doc[q] # Nil}} \\\\subseteq Gens', '<1>a, <1>3, DocGens'),"),
])

# M1 (the narrow's R2/R3): a taken path leaves the tree at the document's
# version on both sides; every other path is untouched.
edit('gen-m1.py', [
    ("{'local': f'[q \\\\in Paths |-> IF q \\\\in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]',",
     "{'local': f'[q \\\\in Paths |-> IF q \\\\in SyncTaken(s, fail) THEN doc[q] ELSE w[s].local[q]]',"),
    ("""  <2>0. \\\\A q \\\\in Paths : q \\\\in SyncOwed(s, fail) => w[s].local[q] = w[s].baseline[q] BY DEF SyncOwed, SyncAll, Owed
  <2>1. \\\\A q \\\\in Paths : /\\\\ (w[s].local[q] = w[s].baseline[q] => {rec}.local[q] = {rec}.baseline[q])
                        /\\\\ (q \\\\notin SyncOwed(s, fail) => {rec}.local[q] = w[s].local[q] /\\\\ {rec}.baseline[q] = w[s].baseline[q])
    BY <1>3, <2>0 DEF SyncBl""",
     """  <2>0. \\\\A q \\\\in Paths : q \\\\in SyncTaken(s, fail) => {rec}.local[q] = doc[q] /\\\\ {rec}.baseline[q] = doc[q] BY <1>3 DEF SyncBl
  <2>1. \\\\A q \\\\in Paths : /\\\\ (w[s].local[q] = w[s].baseline[q] => {rec}.local[q] = {rec}.baseline[q])
                        /\\\\ (q \\\\in SyncTaken(s, fail) => {rec}.local[q] = {rec}.baseline[q])
                        /\\\\ (q \\\\notin SyncTaken(s, fail) => {rec}.local[q] = w[s].local[q] /\\\\ {rec}.baseline[q] = w[s].baseline[q])
    BY <1>3, <2>0 DEF SyncBl"""),
])

# M2: the tree takes the document's version at every taken path.
edit('gen-m2.py', [
    ("{'local': '[q \\\\in Paths |-> IF q \\\\in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]'},",
     "{'local': '[q \\\\in Paths |-> IF q \\\\in SyncTaken(s, fail) THEN doc[q] ELSE w[s].local[q]]'},"),
    ("{'pc': '\"idle\"', 'local': '[q \\\\in Paths |-> IF q \\\\in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]]'},",
     "{'pc': '\"idle\"', 'local': '[q \\\\in Paths |-> IF q \\\\in SyncTaken(s, fail) THEN doc[q] ELSE w[s].local[q]]'},"),
])

# M3 (the cheap path's record): a held path the document differs at after
# the sync was not taken -- not owed, not converging -- so it is the agent's.
edit('gen-m3.py', [
    ("""      <4>1. p \\\\notin SyncOwed(s, fail) BY <3>2, <1>1, <1>3b DEF SyncBl
      <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF SyncOwed, SyncAll
      <4>3. {rec}.baseline[p] = w[s].baseline[p] BY <4>1, <1>3b DEF SyncBl""",
     """      <4>1. p \\\\notin SyncTaken(s, fail) BY <3>2, <1>1, <1>3b DEF SyncBl
      <4>2. ~Owed(s, p) BY <4>1, <3>2 DEF SyncTaken, SyncOwed, SyncAll
      <4>3. {rec}.baseline[p] = w[s].baseline[p] BY <4>1, <1>3b DEF SyncBl"""),
])

# M6 (the agent's work kept): a converging path holds the document's bytes,
# or nothing where the document has nothing -- TakeAW's second and third arms.
_m6_sync_old = """  <2>1. \\A q \\in SyncOwed(s, fail) : w[s].local[q] = w[s].baseline[q] /\\ Held(s, q)
    BY <1>sh DEF SyncOwed, SyncAll, Owed
  <2>2. \\A q \\in Paths : w'[s].local[q] = IF q \\in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]
    BY <1>w DEF {W}
  <2>3. w'[s].scope = w[s].scope BY <1>w DEF {W}
"""
_m6_sync_new = """  <2>1. \\A q \\in SyncOwed(s, fail) : w[s].local[q] = w[s].baseline[q] /\\ Held(s, q)
    BY <1>sh DEF SyncOwed, SyncAll, Owed
  <2>1c. \\A q \\in SyncConv(s) :
           \\/ Held(s, q) /\\ w[s].local[q] # Nil /\\ doc[q] # Nil /\\ Content(w[s].local[q]) = Content(doc[q])
           \\/ Held(s, q) /\\ w[s].local[q] = doc[q]
    BY <1>sh DEF SyncConv, Converges
  <2>2. \\A q \\in Paths : w'[s].local[q] = IF q \\in SyncTaken(s, fail) THEN doc[q] ELSE w[s].local[q]
    BY <1>w DEF {W}
  <2>3. w'[s].scope = w[s].scope BY <1>w DEF {W}
  <2>4. \\A q \\in SyncTaken(s, fail) :
          \\/ w[s].local[q] = w[s].baseline[q] /\\ Held(s, q)
          \\/ Held(s, q) /\\ w[s].local[q] # Nil /\\ doc[q] # Nil /\\ Content(w[s].local[q]) = Content(doc[q])
          \\/ Held(s, q) /\\ w[s].local[q] = doc[q]
    BY <2>1, <2>1c DEF SyncTaken
"""
edit('m6/LeanP1ProofM6.tla', [
    ("""<1>sh. ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync""",
     """<1>sh. ConsumeHonorsScope /\\ ContentConverges BY ShippedShape DEF Shipped
<1>1. PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = SyncW(s, fail)] BY DEF Sync"""),
    ("""<1>sh. ConsumeHonorsScope BY ShippedShape DEF Shipped
<1>1. PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync""",
     """<1>sh. ConsumeHonorsScope /\\ ContentConverges BY ShippedShape DEF Shipped
<1>1. PICK fail \\in SUBSET SyncAll(s) : w' = [w EXCEPT ![s] = RPullSyncW(s, fail)] BY DEF RPullSync"""),
    (_m6_sync_old.replace('{W}', 'SyncW') + """  <2>4. \\A q \\in SyncOwed(s, fail) :
          \\/ w[s].local[q] = w[s].baseline[q] /\\ Held(s, q)
          \\/ Held(s, q) /\\ w[s].local[q] # Nil /\\ doc[q] # Nil /\\ Content(w[s].local[q]) = Content(doc[q])
          \\/ Held(s, q) /\\ w[s].local[q] = doc[q]
    BY <2>1
""", _m6_sync_new.replace('{W}', 'SyncW')),
    (_m6_sync_old.replace('{W}', 'RPullSyncW') + """  <2>. QED BY <2>1, <2>2, <2>3, <1>k, TakeAW""",
     _m6_sync_new.replace('{W}', 'RPullSyncW') + """  <2>. QED BY <2>4, <2>2, <2>3, <1>k, TakeAW"""),
])

# M5C (NR5's premise, NRPre): a path whose baseline moves onto the document's
# version was owed (clean) or converged (the document's bytes) -- TakeWhy's
# first two arms, as Consume's case.
for W, H, A in [('SyncW', 'sy', 'Sync'), ('RPullSyncW', 'rs', 'RPullSync')]:
    edit('m5c/LeanP1ProofM5C.tla', [
        (f"""  <2>1. \\A p \\in Paths : (/\\ {W}(s, fail).baseline[p] = doc[p] /\\ doc[p] # Nil /\\ w[s].baseline[p] # Nil
                          /\\ {W}(s, fail).baseline[p] # w[s].baseline[p]) => TakeWhy(s, p)
    BY DEF {W}, SyncBl, SyncOwed, SyncAll, Owed, TakeWhy
""",
         f"""  <2>1. \\A p \\in Paths : (/\\ {W}(s, fail).baseline[p] = doc[p] /\\ doc[p] # Nil /\\ w[s].baseline[p] # Nil
                          /\\ {W}(s, fail).baseline[p] # w[s].baseline[p]) => TakeWhy(s, p)
    <3>. SUFFICES ASSUME NEW p \\in Paths, {W}(s, fail).baseline[p] = doc[p], doc[p] # Nil,
                         w[s].baseline[p] # Nil, {W}(s, fail).baseline[p] # w[s].baseline[p]
                  PROVE  TakeWhy(s, p)
      OBVIOUS
    <3>1. p \\in SyncTaken(s, fail) BY DEF {W}, SyncBl
    <3>2. CASE p \\in SyncOwed(s, fail) BY <3>2 DEF SyncOwed, SyncAll, Owed, TakeWhy
    <3>3. CASE p \\in SyncConv(s) BY <3>3 DEF SyncConv, Converges, TakeWhy
    <3>. QED BY <3>1, <3>2, <3>3 DEF SyncTaken
"""),
    ])

# M8 (NR1): a taken path was owed (clean) or converged (both a delete, or the
# document's bytes) -- never a deleted path brought back.
edit('gen-m8m9.py', [
    (" 'Sync': '<1>f DEF SyncW, SyncOwed, SyncAll, Owed',\n 'RPullSync': '<1>f DEF RPullSyncW, SyncOwed, SyncAll, Owed',",
     " 'Sync': '<1>f DEF SyncW, SyncBl, SyncTaken, SyncOwed, SyncConv, SyncAll, Owed, Converges',\n"
     " 'RPullSync': '<1>f DEF RPullSyncW, SyncBl, SyncTaken, SyncOwed, SyncConv, SyncAll, Owed, Converges',"),
])

# M13 (A3Side): a taken path is clean after, so not a candidate (one is
# uncited with a file) -- a converged one too.
edit('gen-m13.py', [
    ("      /\\\\ {W}(s, fail).local = [p \\\\in Paths |-> IF p \\\\in SyncOwed(s, fail) THEN doc[p] ELSE w[s].local[p]]",
     "      /\\\\ {W}(s, fail).local = [p \\\\in Paths |-> IF p \\\\in SyncTaken(s, fail) THEN doc[p] ELSE w[s].local[p]]"),
    ("    <3>1. p \\\\notin SyncOwed(s, fail) BY <1>s, <1>f, <1>g DEF A1LeftCand, SyncBl",
     "    <3>1. p \\\\notin SyncTaken(s, fail) BY <1>s, <1>f, <1>g DEF A1LeftCand, SyncBl"),
    ("\\* Sync and RPullSync take owed paths only (clean or deleted: a candidate is a\n\\* dirty file), and clean after; the narrow's fields are kept.",
     "\\* Sync and RPullSync take owed and converged paths, each clean after (a\n\\* candidate is an uncited file); the narrow's fields are kept."),
])

# M14 (A4Shr): a taken path is the document's on both sides after.
edit('gen-m14.py', [
    ("""        /\\\\ {rec}(s, fail).local[q] = IF q \\\\in SyncOwed(s, fail) THEN doc[q] ELSE w[s].local[q]
        /\\\\ {rec}(s, fail).baseline[q] = IF q \\\\in SyncOwed(s, fail) THEN doc[q] ELSE w[s].baseline[q]""",
     """        /\\\\ {rec}(s, fail).local[q] = IF q \\\\in SyncTaken(s, fail) THEN doc[q] ELSE w[s].local[q]
        /\\\\ {rec}(s, fail).baseline[q] = IF q \\\\in SyncTaken(s, fail) THEN doc[q] ELSE w[s].baseline[q]"""),
    ("  <2>1. CASE q \\\\in SyncOwed(s, fail) BY <2>1, <1>l DEF A4Shr\n  <2>2. CASE q \\\\notin SyncOwed(s, fail)",
     "  <2>1. CASE q \\\\in SyncTaken(s, fail) BY <2>1, <1>l DEF A4Shr\n  <2>2. CASE q \\\\notin SyncTaken(s, fail)"),
])

# M11 (AK1's invariant): a taken path is the document's on both sides after,
# owed or converged (the consume's case, Consume_A1).  NEW: a sync may run at
# "saved" (a restart mid-narrow), and a converged path may be the narrow's
# leftover -- the document still cites the bytes the uncite let go -- which it
# re-cites; the leftover set shrinks.  A declared FILE there is in carrier form
# (CPubL) before, and clean after: CPub3.  So no step asserts the leftover set
# is kept; each path's membership is kept where its tree is.
_PEND = ("               /\\ R.pend = w[s].pend /\\ R.dset = w[s].dset /\\ R.decl = w[s].decl /\\ R.moved = w[s].moved"
         " /\\ R.dMint = w[s].dMint /\\ R.car = w[s].car /\\ R.bound = w[s].bound /\\ R.nt = w[s].nt"
         " /\\ R.lost = w[s].lost /\\ R.inst = w[s].inst /\\ R.cId = w[s].cId /\\ R.cFold = w[s].cFold\n")


def m11_r_old(W, PC):
    return (f"<1>R. /\\ R.pc = {PC}\n" + _PEND +
            "               /\\ A1LeftSet(R) = A1LeftSet(w[s]) /\\ R.sStage = w[s].sStage\n"
            "               /\\ \\A q \\in Paths : \\/ R.local[q] = w[s].local[q] /\\ R.baseline[q] = w[s].baseline[q]\n"
            "                                   \\/ R.local[q] = doc[q] /\\ R.baseline[q] = doc[q] /\\ Owed(s, q)\n")


def m11_r_new(W, PC):
    return (f"<1>R. /\\ R.pc = {PC}\n" + _PEND +
            "               /\\ R.sStage = w[s].sStage /\\ R.sDrop = w[s].sDrop /\\ R.sHeld = w[s].sHeld /\\ R.sKeep = w[s].sKeep\n"
            "               /\\ \\A q \\in Paths : \\/ R.local[q] = w[s].local[q] /\\ R.baseline[q] = w[s].baseline[q]\n"
            "                                   \\/ /\\ R.local[q] = doc[q] /\\ R.baseline[q] = doc[q]\n"
            "                                      /\\ (Owed(s, q) \\/ Converges(s, q))\n")


def m11_t_old(W):
    return f"""  <2>t1. {W}(s, fail).local = [p \\in Paths |-> IF p \\in SyncOwed(s, fail) THEN doc[p] ELSE w[s].local[p]] BY DEF {W}
  <2>t2. {W}(s, fail).baseline = SyncBl(s, fail) BY DEF {W}
  <2>t. \\A q \\in Paths : \\/ {W}(s, fail).local[q] = w[s].local[q] /\\ {W}(s, fail).baseline[q] = w[s].baseline[q]
                         \\/ {W}(s, fail).local[q] = doc[q] /\\ {W}(s, fail).baseline[q] = doc[q] /\\ Owed(s, q)
    <3>. SUFFICES ASSUME NEW q \\in Paths
                  PROVE  \\/ {W}(s, fail).local[q] = w[s].local[q] /\\ {W}(s, fail).baseline[q] = w[s].baseline[q]
                         \\/ {W}(s, fail).local[q] = doc[q] /\\ {W}(s, fail).baseline[q] = doc[q] /\\ Owed(s, q) OBVIOUS
    <3>1. CASE q \\in SyncOwed(s, fail) BY <3>1, <2>t1, <2>t2 DEF SyncBl, SyncOwed, SyncAll
    <3>2. CASE q \\notin SyncOwed(s, fail) BY <3>2, <2>t1, <2>t2 DEF SyncBl
    <3>. QED BY <3>1, <3>2
"""


def m11_t_new(W):
    return f"""  <2>t1. {W}(s, fail).local = [p \\in Paths |-> IF p \\in SyncTaken(s, fail) THEN doc[p] ELSE w[s].local[p]] BY DEF {W}
  <2>t2. {W}(s, fail).baseline = SyncBl(s, fail) BY DEF {W}
  <2>t. \\A q \\in Paths : \\/ {W}(s, fail).local[q] = w[s].local[q] /\\ {W}(s, fail).baseline[q] = w[s].baseline[q]
                         \\/ /\\ {W}(s, fail).local[q] = doc[q] /\\ {W}(s, fail).baseline[q] = doc[q]
                            /\\ (Owed(s, q) \\/ Converges(s, q))
    <3>. SUFFICES ASSUME NEW q \\in Paths
                  PROVE  \\/ {W}(s, fail).local[q] = w[s].local[q] /\\ {W}(s, fail).baseline[q] = w[s].baseline[q]
                         \\/ /\\ {W}(s, fail).local[q] = doc[q] /\\ {W}(s, fail).baseline[q] = doc[q]
                            /\\ (Owed(s, q) \\/ Converges(s, q)) OBVIOUS
    <3>1. CASE q \\in SyncOwed(s, fail) BY <3>1, <2>t1, <2>t2 DEF SyncBl, SyncTaken, SyncOwed, SyncAll
    <3>1c. CASE q \\in SyncConv(s) BY <3>1c, <2>t1, <2>t2 DEF SyncBl, SyncTaken, SyncConv
    <3>2. CASE q \\notin SyncTaken(s, fail) BY <3>2, <2>t1, <2>t2 DEF SyncBl
    <3>. QED BY <3>1, <3>1c, <3>2 DEF SyncTaken
"""


def m11_l_old(W):
    return f"""  \\* the leftover is kept: a taken path was owed, so clean (a leftover is dirty:
  \\* uncited with a file) and clean after (local' = baseline')
  <2>l. A1LeftSet({W}(s, fail)) = A1LeftSet(w[s])
    <3>0. w[s].sDrop \\subseteq Paths BY <1>0, WriterFields DEF TypeOK
    <3>1. /\\ {W}(s, fail).sDrop = w[s].sDrop /\\ {W}(s, fail).sHeld = w[s].sHeld
          /\\ {W}(s, fail).sStage = w[s].sStage /\\ {W}(s, fail).sKeep = w[s].sKeep BY DEF {W}
    <3>2. \\A q \\in Paths : q \\in SyncOwed(s, fail) => w[s].local[q] = w[s].baseline[q] \\/ w[s].local[q] = Nil
      BY DEF SyncOwed, SyncAll, Owed
    <3>3. \\A q \\in Paths : (q \\in A1LeftSet({W}(s, fail))) <=> (q \\in A1LeftSet(w[s]))
      <4>. SUFFICES ASSUME NEW q \\in Paths
                    PROVE  (q \\in A1LeftSet({W}(s, fail))) <=> (q \\in A1LeftSet(w[s])) OBVIOUS
      <4>1. CASE q \\in SyncOwed(s, fail)
        <5>1. {W}(s, fail).local[q] = doc[q] /\\ {W}(s, fail).baseline[q] = doc[q] BY <4>1, <2>t1, <2>t2 DEF SyncBl
        <5>2. q \\notin A1LeftSet({W}(s, fail)) BY <5>1, <3>1, <1>g DEF A1LeftSet
        <5>3. q \\notin A1LeftSet(w[s]) BY <4>1, <3>2, <1>g DEF A1LeftSet
        <5>. QED BY <5>2, <5>3
      <4>2. CASE q \\notin SyncOwed(s, fail)
        <5>1. {W}(s, fail).local[q] = w[s].local[q] /\\ {W}(s, fail).baseline[q] = w[s].baseline[q] BY <4>2, <2>t1, <2>t2 DEF SyncBl
        <5>. QED BY <5>1, <3>1 DEF A1LeftSet
      <4>. QED BY <4>1, <4>2
    <3>4. A1LeftSet({W}(s, fail)) \\subseteq Paths /\\ A1LeftSet(w[s]) \\subseteq Paths BY <3>0, <3>1 DEF A1LeftSet
    <3>5. \\A x : (x \\in A1LeftSet({W}(s, fail))) <=> (x \\in A1LeftSet(w[s])) BY <3>3, <3>4
    <3>. QED BY <3>5, Zenon
  <2>st. {W}(s, fail).sStage = w[s].sStage BY DEF {W}
  <2>. QED BY <2>p, <2>k0, <2>k1, <2>k2, <2>k3, <2>l, <2>t, <2>st, Zenon DEF R
"""


def m11_l_new(W):
    return f"""  \\* the narrow's record is kept (a converged path may leave the leftover set:
  \\* below, per path)
  <2>n. /\\ {W}(s, fail).sDrop = w[s].sDrop /\\ {W}(s, fail).sHeld = w[s].sHeld
        /\\ {W}(s, fail).sStage = w[s].sStage /\\ {W}(s, fail).sKeep = w[s].sKeep BY DEF {W}
  <2>. QED BY <2>p, <2>k0, <2>k1, <2>k2, <2>k3, <2>n, <2>t, Zenon DEF R
"""


_CASES_OLD = """    <3>1. CASE w'[s].local[p] = w[s].local[p] /\\ w'[s].baseline[p] = w[s].baseline[p]
      <4>1. (A1Decl(s, p) /\\ w[s].local[p] # w[s].baseline[p] /\\ p \\notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>1, <3>d, <3>car, <1>old, <1>c, <2>w, <1>R
      <4>l. (A1Decl(s, p) /\\ w[s].decl[p] # Nil /\\ p \\in A1LeftSet(w[s]))' =>
              (w[s].car.id = w[s].pend.id /\\ CarForm(s, p, w[s].car))'
        BY <3>d, <3>cf, <3>car, <1>old, <2>w, <1>R
      <4>2. (A1Decl(s, p) /\\ w[s].decl[p] # Nil /\\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\\ CarForm(s, p, w[s].car))'
        BY <3>1, <3>d, <3>cf, <3>car, <1>old
      <4>. QED BY <4>1, <4>l, <4>2, <3>dc, <3>t DEF A1TreePath
    <3>2. CASE w'[s].local[p] = doc[p] /\\ w'[s].baseline[p] = doc[p] /\\ Owed(s, p)
      \\* clean now (DTree vacuous); it was clean (owed), so the old CPub3 holds
      <4>1. (A1Decl(s, p) /\\ w[s].local[p] # w[s].baseline[p] /\\ p \\notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>2
      <4>2. (A1Decl(s, p) /\\ w[s].decl[p] # Nil /\\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\\ CarForm(s, p, w[s].car))'
        BY <3>2, <3>d, <3>cf, <3>car, <1>old DEF Owed
      <4>l. p \\notin A1LeftSet(w'[s]) BY <3>2, <2>w, <1>R, <1>g DEF A1LeftSet
      <4>. QED BY <4>1, <4>l, <4>2, <3>dc, <3>t DEF A1TreePath
    <3>. QED BY <3>1, <3>2, <2>w, <1>R
"""
_CASES_NEW = """    \\* a path whose tree is kept keeps its leftover membership (the narrow's
    \\* record is kept)
    <3>lk. (w'[s].local[p] = w[s].local[p] /\\ w'[s].baseline[p] = w[s].baseline[p]) =>
             (p \\in A1LeftSet(w'[s]) <=> p \\in A1LeftSet(w[s]))
      BY <2>w, <1>R DEF A1LeftSet
    <3>1. CASE w'[s].local[p] = w[s].local[p] /\\ w'[s].baseline[p] = w[s].baseline[p]
      <4>0. p \\in A1LeftSet(w'[s]) <=> p \\in A1LeftSet(w[s]) BY <3>1, <3>lk
      <4>1. (A1Decl(s, p) /\\ w[s].local[p] # w[s].baseline[p] /\\ p \\notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>1, <4>0, <3>d, <3>car, <1>old, <1>c, <2>w, <1>R
      <4>l. (A1Decl(s, p) /\\ w[s].decl[p] # Nil /\\ p \\in A1LeftSet(w[s]))' =>
              (w[s].car.id = w[s].pend.id /\\ CarForm(s, p, w[s].car))'
        BY <4>0, <3>d, <3>cf, <3>car, <1>old, <2>w, <1>R
      <4>2. (A1Decl(s, p) /\\ w[s].decl[p] # Nil /\\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\\ CarForm(s, p, w[s].car))'
        BY <3>1, <3>d, <3>cf, <3>car, <1>old
      <4>. QED BY <4>1, <4>l, <4>2, <3>dc, <3>t DEF A1TreePath
    <3>2. CASE /\\ w'[s].local[p] = doc[p] /\\ w'[s].baseline[p] = doc[p] /\\ (Owed(s, p) \\/ Converges(s, p))
      \\* clean now (DTree vacuous)
      <4>1. (A1Decl(s, p) /\\ w[s].local[p] # w[s].baseline[p] /\\ p \\notin A1LeftSet(w[s]))' =>
              (Content(w[s].local[p]) = Content(w[s].decl[p]))'
        BY <3>2
      \\* CPub3: owed was clean (the old CPub3); a converged leftover was in
      \\* carrier form (CPubL); any other converged path holds the declared bytes
      \\* in doc (DocCar), as in Consume_A1
      <4>2. (A1Decl(s, p) /\\ w[s].decl[p] # Nil /\\ w[s].local[p] = w[s].baseline[p])' =>
              (w[s].car.id = w[s].pend.id /\\ CarForm(s, p, w[s].car))'
        <5>. SUFFICES ASSUME A1Decl(s, p), w[s].decl[p] # Nil
                      PROVE  w[s].car.id = w[s].pend.id /\\ CarForm(s, p, w[s].car)
          BY <3>d, <3>cf, <3>car
        <5>1. CASE Owed(s, p) BY <5>1, <1>old DEF Owed
        <5>2. CASE Converges(s, p) /\\ ~Owed(s, p) /\\ p \\in A1LeftSet(w[s]) BY <5>2, <1>old
        <5>3. CASE Converges(s, p) /\\ ~Owed(s, p) /\\ p \\notin A1LeftSet(w[s])
          <6>1. w[s].local[p] # w[s].baseline[p] BY <5>3 DEF Converges
          <6>2. Content(w[s].local[p]) = Content(w[s].decl[p]) BY <6>1, <5>3, <1>old
          <6>3. CASE w[s].local[p] # Nil /\\ doc[p] # Nil /\\ Content(w[s].local[p]) = Content(doc[p])
            BY <6>2, <6>3, <1>old
          <6>4. CASE w[s].local[p] = Nil /\\ doc[p] = Nil
            \\* the declared bytes would be Nil: a declared delete, not a file
            BY <6>2, <6>4 DEF Content
          <6>. QED BY <5>3, <6>3, <6>4 DEF Converges
        <5>. QED BY <5>1, <5>2, <5>3, <3>2
      <4>l. p \\notin A1LeftSet(w'[s]) BY <3>2, <2>w, <1>R, <1>g DEF A1LeftSet
      <4>. QED BY <4>1, <4>l, <4>2, <3>dc, <3>t DEF A1TreePath
    <3>. QED BY <3>1, <3>2, <2>w, <1>R
"""

t = open('gen-m11.py').read()
for lem, W, PC in [('Sync_A1', 'SyncW', 'w[s].pc'), ('RPullSync_A1', 'RPullSyncW', '"idle"')]:
    a = t.index(f'LEMMA {lem} ==')
    b = t.index('\nLEMMA ', a + 10)
    seg = t[a:b]
    for o, n in [(m11_r_old(W, PC), m11_r_new(W, PC)), (m11_t_old(W), m11_t_new(W)),
                 (m11_l_old(W), m11_l_new(W)), (_CASES_OLD, _CASES_NEW)]:
        assert seg.count(o) == 1, (lem, seg.count(o), o[:80])
        seg = seg.replace(o, n)
    t = t[:a] + seg + t[b:]
open('gen-m11.py', 'w').write(t)
print('patched gen-m11.py')
