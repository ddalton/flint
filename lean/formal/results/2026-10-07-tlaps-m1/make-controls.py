#!/usr/bin/env python3
"""The M1 controls (plan section 5): three scratch variants, each in its own
directory, each expected to FAIL exactly the named obligation.
  ctl-claim    LeanP1Anc.tla's Claim without `holder = "none"`, and the proof's
               Claim lemma without that guard in its restatement:
               Claim_M1 <2>1 (Inv_OneHolder') must fail.
  ctl-dwp      Shipped without DeleteWinsPreserved, and Install_M1's <1>i
               without the step that cites it: <1>i's QED must fail.
  ctl-ruf      Shipped without RescopeUnciteFirst, and the two rescope lemmas
               without the step that cites it: their tree reads (<1>3), the
               uncite-first shape, must fail.
Run from lean/formal: python3 results/2026-10-07-tlaps-m1/make-controls.py OUTDIR"""
import sys, os
out = sys.argv[1]
anc = open('LeanP1Anc.tla').read()
prf = open('LeanP1Proof.tla').read()
def rep(s, old, new):
    assert s.count(old) == 1, (old[:60], s.count(old))
    return s.replace(old, new)
def emit(name, a, p):
    d = os.path.join(out, name); os.makedirs(d, exist_ok=True)
    open(os.path.join(d, 'LeanP1Anc.tla'), 'w').write(a)
    open(os.path.join(d, 'LeanP1Proof.tla'), 'w').write(p)
    print(name, 'anc', 'changed' if a != anc else 'same', 'proof', 'changed' if p != prf else 'same')

# ctl-claim
a = rep(anc, '  /\\ holder = "none"\n  /\\ holder\' = s\n', '  /\\ holder\' = s\n')
p = rep(prf, '<1>g. On(s) /\\ w[s].pc = "scanned" /\\ holder = "none" BY DEF Claim\n',
             '<1>g. On(s) /\\ w[s].pc = "scanned" BY DEF Claim\n')
emit('ctl-claim', a, p)

# ctl-dwp
p = rep(prf, '  /\\ CollectorSparesCited /\\ CommitRecordsDeleteOverride /\\ DeleteWinsPreserved\n',
             '  /\\ CollectorSparesCited /\\ CommitRecordsDeleteOverride\n')
p = rep(p, '  <2>1. DeleteWinsPreserved BY ShippedShape DEF Shipped\n', '')
p = rep(p, '  <2>. QED BY <2>1, <2>2, <1>f DEF InstallInst\n', '  <2>. QED BY <2>2, <1>f DEF InstallInst\n')
emit('ctl-dwp', anc, p)

# ctl-ruf
p = rep(prf, '  /\\ ConsumeHonorsScope /\\ RescopeUnciteFirst /\\ RescopeKeepsDirty /\\ WidenKeepsLocal\n',
             '  /\\ ConsumeHonorsScope /\\ RescopeKeepsDirty /\\ WidenKeepsLocal\n')
assert p.count('<1>u. RescopeUnciteFirst BY ShippedShape DEF Shipped\n') == 2
p = p.replace('<1>u. RescopeUnciteFirst BY ShippedShape DEF Shipped\n', '')
p = rep(p, '  BY <1>u DEF RescopeFirstW\n', '  BY DEF RescopeFirstW\n')
p = rep(p, '    BY <1>u DEF RescopeLocal1, RescopeBase1\n', '    BY DEF RescopeLocal1, RescopeBase1\n')
p = rep(p, 'BY <1>3, <1>u\n', 'BY <1>3\n')
emit('ctl-ruf', anc, p)
