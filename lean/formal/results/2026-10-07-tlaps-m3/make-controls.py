#!/usr/bin/env python3
"""The M3 controls (plan section 5): scratch variants of the proof text, each
with one rule constant dropped from Shipped AND from the step that cites it,
so the obligation that USES the rule fails:
  ctl-rs    RecheckSkipped: ShortcutFromRecord's CheapPath read (<1>1) must fail.
  ctl-cag   CommitAdvanceGuarded: Install_M3's `adv` fact (RAdv, <2>1) must fail.
  ctl-ckl   ConsumeKeepsLeft: Consume_M3's record (RRec, <3>1: derived = seq
            only when nothing was left) must fail.
  ctl-skl   SyncKeepsLeft: the same in Sync_M3 and RPullSync_M3.
  ctl-rro   ReaderRechecksOwed: ReaderFromRecord's skip read (<1>1) must fail.
  ctl-chs   ConsumeHonorsScope (not in the plan's list; M3 uses it): an owed
            path is held -- Consume's, Sync's and RPullSync's record and both
            claims' Owed read must fail.
Run from lean/formal: python3 results/2026-10-07-tlaps-m3/make-controls.py PROOF OUTDIR"""
import sys, os
prf = open(sys.argv[1]).read()
out = sys.argv[2]
anc = open('LeanP1Anc.tla').read()
def rep(s, old, new, n=1):
    assert s.count(old) == n, (old[:70], s.count(old), n)
    return s.replace(old, new)
def emit(name, p):
    d = os.path.join(out, name); os.makedirs(d, exist_ok=True)
    open(os.path.join(d, 'LeanP1Anc.tla'), 'w').write(anc)
    open(os.path.join(d, 'LeanP1Proof.tla'), 'w').write(p)
    print(name, 'proof', 'changed' if p != prf else 'same')
SH = ("  /\\ ContentConverges /\\ RecheckSkipped /\\ CommitAdvanceGuarded /\\ RetireAge\n",
      "  /\\ ConsumeHonorsScope /\\ RescopeUnciteFirst /\\ RescopeKeepsDirty /\\ WidenKeepsLocal\n",
      "  /\\ UnlinkChecksBytes /\\ ConsumeKeepsLeft /\\ SyncKeepsLeft /\\ ReaderRechecksOwed\n")
for l in SH: assert prf.count(l) == 1, l
M3 = prf.index('(* M3: Inv_ShortcutSound, Inv_ReaderSound.')
def m3rep(p, old, new, n):
    head, tail = p[:M3], p[M3:]
    return head + rep(tail, old, new, n)

p = rep(prf, SH[0], "  /\\ ContentConverges /\\ CommitAdvanceGuarded /\\ RetireAge\n")
p = m3rep(p, "<1>s. RecheckSkipped /\\ ConsumeHonorsScope BY ShippedShape DEF Shipped\n",
             "<1>s. ConsumeHonorsScope BY ShippedShape DEF Shipped\n", 1)
emit('ctl-rs', p)

p = rep(prf, SH[0], "  /\\ ContentConverges /\\ RecheckSkipped /\\ RetireAge\n")
p = m3rep(p, "<1>s. CommitAdvanceGuarded BY ShippedShape DEF Shipped\n", "<1>s. TRUE OBVIOUS\n", 1)
emit('ctl-cag', p)

p = rep(prf, SH[2], "  /\\ UnlinkChecksBytes /\\ SyncKeepsLeft /\\ ReaderRechecksOwed\n")
p = m3rep(p, "<1>s. ConsumeKeepsLeft /\\ ConsumeHonorsScope BY ShippedShape DEF Shipped\n",
             "<1>s. ConsumeHonorsScope BY ShippedShape DEF Shipped\n", 1)
emit('ctl-ckl', p)

p = rep(prf, SH[2], "  /\\ UnlinkChecksBytes /\\ ConsumeKeepsLeft /\\ ReaderRechecksOwed\n")
p = m3rep(p, "<1>s. SyncKeepsLeft /\\ ConsumeHonorsScope BY ShippedShape DEF Shipped\n",
             "<1>s. ConsumeHonorsScope BY ShippedShape DEF Shipped\n", 2)
emit('ctl-skl', p)

p = rep(prf, SH[2], "  /\\ UnlinkChecksBytes /\\ ConsumeKeepsLeft /\\ SyncKeepsLeft\n")
p = m3rep(p, "<1>s. ReaderRechecksOwed /\\ ConsumeHonorsScope BY ShippedShape DEF Shipped\n",
             "<1>s. ConsumeHonorsScope BY ShippedShape DEF Shipped\n", 1)
emit('ctl-rro', p)

p = rep(prf, SH[1], "  /\\ RescopeUnciteFirst /\\ RescopeKeepsDirty /\\ WidenKeepsLocal\n")
p = m3rep(p, "<1>s. ConsumeKeepsLeft /\\ ConsumeHonorsScope BY", "<1>s. ConsumeKeepsLeft BY", 1)
p = m3rep(p, "<1>s. SyncKeepsLeft /\\ ConsumeHonorsScope BY", "<1>s. SyncKeepsLeft BY", 2)
p = m3rep(p, "<1>s. RecheckSkipped /\\ ConsumeHonorsScope BY", "<1>s. RecheckSkipped BY", 1)
p = m3rep(p, "<1>s. ReaderRechecksOwed /\\ ConsumeHonorsScope BY", "<1>s. ReaderRechecksOwed BY", 1)
emit('ctl-chs', p)
