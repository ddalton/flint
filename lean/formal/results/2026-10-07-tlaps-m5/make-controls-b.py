#!/usr/bin/env python3
"""M5 part B's controls: the two shipped rules part B's proof cites, each
dropped from `Shipped` in turn (the proof text only; the model unchanged).
Run on part B's section only, each must fail exactly the step that cites it:

  ctl-gjr  GatewayJudgesRead dropped: GCas_Rev's <1>1 (the CAS lands only
           over the version the save read), line 8074.
  ctl-csf  CommitSurfacesForeign dropped: Install_Rev's <1>5 <2>1 (a commit
           over a foreign version records it), line 8100.

Every line keeps its number (the edit is inside line 70 / 72).
Run from lean/formal: python3 results/2026-10-07-tlaps-m5/make-controls-b.py PROOF OUTDIR"""
import sys, os
prf = open(sys.argv[1]).read(); out = sys.argv[2]
def rep(s, old, new):
    assert s.count(old) == 1, (old, s.count(old))
    return s.replace(old, new)
def emit(name, p):
    d = os.path.join(out, name); os.makedirs(d, exist_ok=True)
    open(os.path.join(d, 'LeanP1Proof.tla'), 'w').write(p)
    print(name, 'written')
emit('ctl-gjr', rep(prf, "  /\\ GatewayIgnoresLease /\\ GatewayJudgesRead /\\ GatewaySweepGrace",
                         "  /\\ GatewayIgnoresLease /\\ GatewaySweepGrace"))
emit('ctl-csf', rep(prf, "  /\\ CommitSurfacesForeign /\\ CommitVerifiesUploads /\\ SweepUnderLease",
                         "  /\\ CommitVerifiesUploads /\\ SweepUnderLease"))
