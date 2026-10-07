#!/usr/bin/env python3
"""The M2 controls (plan section 5): four scratch variants of the proof text,
each with one rule constant dropped from Shipped AND from the step that
derives it, so the obligation that USES the rule fails:
  ctl-sul   SweepUnderLease: Sweep_M2's `holder = s` step removed -- its
            events step (LiveEv: the sweep spares the holder's PUTs) must fail.
  ctl-gsg   GatewaySweepGrace: Sweep_M2's `~InFlight(h)` step removed -- the
            same events step (a save in flight stays live) must fail.
  ctl-cvu   CommitVerifiesUploads: Verify_M2's Verified' hypothesis no longer
            cites it -- that step must fail (gone = {} proves nothing live).
  ctl-rage  RetireAge: the frame steps' RetEv (RetFacts needs it) and
            Collect_M2's `live' = live` must fail.
Run from lean/formal: python3 results/2026-10-07-tlaps-m2/make-controls.py OUTDIR"""
import sys, os
out = sys.argv[1]
anc = open('LeanP1Anc.tla').read()
prf = open('LeanP1Proof.tla').read()
def rep(s, old, new, n=1):
    assert s.count(old) == n, (old[:60], s.count(old))
    return s.replace(old, new)
def emit(name, p):
    d = os.path.join(out, name); os.makedirs(d, exist_ok=True)
    open(os.path.join(d, 'LeanP1Anc.tla'), 'w').write(anc)
    open(os.path.join(d, 'LeanP1Proof.tla'), 'w').write(p)
    print(name, 'proof', 'changed' if p != prf else 'same')
S_LINE = "<1>s. RetireAge /\\ SweepUnderLease /\\ GatewaySweepGrace /\\ CommitVerifiesUploads /\\ RescopeUnciteFirst\n"
NS = prf.count(S_LINE)
assert NS >= 25, NS

# ctl-sul
p = rep(prf, "  /\\ CommitSurfacesForeign /\\ CommitVerifiesUploads /\\ SweepUnderLease\n",
             "  /\\ CommitSurfacesForeign /\\ CommitVerifiesUploads\n")
p = p.replace(S_LINE, "<1>s. RetireAge /\\ GatewaySweepGrace /\\ CommitVerifiesUploads /\\ RescopeUnciteFirst\n")
p = rep(p, "<1>1h. holder = s BY <1>1, <1>s\n", "")
p = rep(p, "  BY <1>1, <1>1h, <1>1i, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv, InFlight\n",
           "  BY <1>1, <1>1i, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv, InFlight\n")
emit('ctl-sul', p)

# ctl-gsg
p = rep(prf, "  /\\ GatewayIgnoresLease /\\ GatewayJudgesRead /\\ GatewaySweepGrace /\\ RenameAtomic\n",
             "  /\\ GatewayIgnoresLease /\\ GatewayJudgesRead /\\ RenameAtomic\n")
p = p.replace(S_LINE, "<1>s. RetireAge /\\ SweepUnderLease /\\ CommitVerifiesUploads /\\ RescopeUnciteFirst\n")
p = rep(p, "<1>1i. ~InFlight(h) BY <1>1, <1>s\n", "")
p = rep(p, "  BY <1>1, <1>1h, <1>1i, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv, InFlight\n",
           "  BY <1>1, <1>1h, <1>0, WrNone, NoneWriter DEF DocEv, GwEv, TreeEv, LiveEv, InFlight\n")
emit('ctl-gsg', p)

# ctl-cvu
p = rep(prf, "  /\\ CommitSurfacesForeign /\\ CommitVerifiesUploads /\\ SweepUnderLease\n",
             "  /\\ CommitSurfacesForeign /\\ SweepUnderLease\n")
p = p.replace(S_LINE, "<1>s. RetireAge /\\ SweepUnderLease /\\ GatewaySweepGrace /\\ RescopeUnciteFirst\n")
p = rep(p, "<1>10a. (VerifyW(s).pc = \"claimed\" /\\ VerifyW(s).verified) => \\A pp \\in (VerifyW(s).uploads \\cap VerifyW(s).upDone) \\ VerifyW(s).gone : VerifyW(s).snap[pp] \\in live'\n  BY <1>1, <1>3, <1>s\n",
           "<1>10a. (VerifyW(s).pc = \"claimed\" /\\ VerifyW(s).verified) => \\A pp \\in (VerifyW(s).uploads \\cap VerifyW(s).upDone) \\ VerifyW(s).gone : VerifyW(s).snap[pp] \\in live'\n  BY <1>1, <1>3\n")
emit('ctl-cvu', p)

# ctl-rage
p = rep(prf, "  /\\ ContentConverges /\\ RecheckSkipped /\\ CommitAdvanceGuarded /\\ RetireAge\n",
             "  /\\ ContentConverges /\\ RecheckSkipped /\\ CommitAdvanceGuarded\n")
p = p.replace(S_LINE, "<1>s. SweepUnderLease /\\ GatewaySweepGrace /\\ CommitVerifiesUploads /\\ RescopeUnciteFirst\n")
emit('ctl-rage', p)
