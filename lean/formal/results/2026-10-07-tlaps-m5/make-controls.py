#!/usr/bin/env python3
"""M5 part A's controls (plan section 5).  Part A reads no Shipped rule, so
the dropped-rule control does not apply; instead, two that each knock out
one load-bearing fact and predict exactly where the proof must break:

  ctl-blm  BaselineMinted dropped from Hist (the proof text only).  Edit
           mints with base := the tree's baseline, so its HistEv needs the
           baseline minted, and HistWrite needs it to carry BaselineMinted'
           through: EXACTLY Edit_M5's <2>5 and HistWrite's <1>4 <2>2 fail.
  ctl-cor  THE MODEL MUTATED (a scratch LeanP1Anc.tla, never committed as
           the module): a re-upload's copy records orig := the handle it
           copies, not that handle's Content -- so a copy of a copy names a
           copy, and HistMinted's "an original is no copy" breaks.  Every
           restatement of the write in the proof follows the mutation
           (M0's typing step and the fact <2>3 it types orig'[c] with,
           M5's two), so the ONE obligation that must
           fail is the load-bearing one: Upload_M5's <3>6, case
           orig[h] # Nil (h a copy).
           Run 1 left <2>3 at Content(h): Upload_TypeOK's <2>7 failed too
           (line 673, the unfollowed typing fact); run 2 follows it.
Run from lean/formal: python3 results/2026-10-07-tlaps-m5/make-controls.py PROOF OUTDIR"""
import sys, os
prf = open(sys.argv[1]).read(); out = sys.argv[2]
anc = open('LeanP1Anc.tla').read()
def rep(s, old, new, n=1):
    assert s.count(old) == n, (old[:70], s.count(old), n)
    return s.replace(old, new)
def emit(name, a, p):
    d = os.path.join(out, name); os.makedirs(d, exist_ok=True)
    open(os.path.join(d, 'LeanP1Anc.tla'), 'w').write(a)
    open(os.path.join(d, 'LeanP1Proof.tla'), 'w').write(p)
    print(name, 'written')

p = rep(prf, "Hist == HistNew /\\ HistAnc /\\ HistMinted /\\ BaselineMinted\n",
             "Hist == HistNew /\\ HistAnc /\\ HistMinted\n")
emit('ctl-blm', anc, p)

W = "orig' = [orig EXCEPT ![c] = Content(h)]"
a = rep(anc, W, "orig' = [orig EXCEPT ![c] = h]")
p = rep(prf, W, "orig' = [orig EXCEPT ![c] = h]", n=3)
p = rep(p, "<4>3. base'[c] = base[h] /\\ orig'[c] = Content(h) BY",
           "<4>3. base'[c] = base[h] /\\ orig'[c] = h BY")
p = rep(p, "<2>3. Content(h) \\in Opt(Handles) BY <1>b, <1>2 DEF Ghosts, Content, Opt",
           "<2>3. h \\in Opt(Handles) BY <1>2 DEF Opt")
p = rep(p, "<3>6. Content(h) \\in Opt(minted) /\\ (Content(h) # Nil => orig[Content(h)] = Nil)",
           "<3>6. h \\in Opt(minted) /\\ (h # Nil => orig[h] = Nil)")
emit('ctl-cor', a, p)
