#!/usr/bin/env python3
"""The M4 control (plan section 5): RetireAge dropped from Shipped AND from
every step that cites it.  M2 cites it too (its ctl-rage: the 25 frame
steps' RetEv and Collect's live' = live), so those 26 fail again; M4's own
must fail as well: every frame step's exact retire log (<1>r: RetExact),
Sweep's `h \\notin retiring` and Collect's `live' = live` (LiveLoss).
Run from lean/formal: python3 results/2026-10-07-tlaps-m4/make-controls.py PROOF OUTDIR"""
import sys, os
prf = open(sys.argv[1]).read(); out = sys.argv[2]
anc = open('LeanP1Anc.tla').read()
def rep(s, old, new, n=1):
    assert s.count(old) == n, (old[:70], s.count(old), n)
    return s.replace(old, new)
p = rep(prf, "  /\\ ContentConverges /\\ RecheckSkipped /\\ CommitAdvanceGuarded /\\ RetireAge\n",
             "  /\\ ContentConverges /\\ RecheckSkipped /\\ CommitAdvanceGuarded\n")
S2 = "<1>s. RetireAge /\\ SweepUnderLease /\\ GatewaySweepGrace /\\ CommitVerifiesUploads /\\ RescopeUnciteFirst\n"
assert p.count(S2) >= 25
p = p.replace(S2, "<1>s. SweepUnderLease /\\ GatewaySweepGrace /\\ CommitVerifiesUploads /\\ RescopeUnciteFirst\n")
M4 = p.index('(* M4: Inv_ReaderFetches.')
head, tail = p[:M4], p[M4:]
n4 = tail.count("<1>s. RetireAge BY ShippedShape DEF Shipped\n")
assert n4 >= 25, n4
tail = tail.replace("<1>s. RetireAge BY ShippedShape DEF Shipped\n", "<1>s. TRUE OBVIOUS\n")
p = head + tail
d = os.path.join(out, 'ctl-rage4'); os.makedirs(d, exist_ok=True)
open(os.path.join(d, 'LeanP1Anc.tla'), 'w').write(anc)
open(os.path.join(d, 'LeanP1Proof.tla'), 'w').write(p)
print('ctl-rage4: M2 <1>s lines', prf.count(S2), 'M4 <1>s lines', n4)
