#!/usr/bin/env python3
"""M5 E12: the KNOWN-BAD runs for Inv_AckedNamed and Inv_NoRegress.  No shipped
rule switched off breaks either original in the gate's FetchHolds or ScopeHolds
(E11: 22 rules x 2 worlds x 2 claims, all hold), so the positive controls are
MUTATIONS of the model -- scratch copies of LeanP1.tla, never the module:

  MA1  the gateway's delete records nothing: no tombstone, no acknowledged
       delete (an acked version uncited with no trace).
  MA2  a commit that deletes a path leaves no tombstone (the tree's own
       delete untraced at the path; `took` may still name it).
  MN2  the gateway's delete is not recorded as acknowledged (`udel`); the
       tombstone stays.  E6's regress with nothing that excuses it.

Each mutant must break its original; each restatement candidate must break
with it, or it has widened the claim past the loss.
Run from lean/formal: python3 results/2026-10-07-tlaps-m5/make-mutants.py OUTDIR"""
import sys, os
out = sys.argv[1]
src = open('LeanP1.tla').read()
def rep(s, old, new):
    assert s.count(old) == 1, (old, s.count(old))
    return s.replace(old, new)
GD_T = "  /\\ tomb' = [tomb EXCEPT ![p] = doc[p]]\n  /\\ udel' = udel \\cup {<<p, doc[p]>>}\n"
M = {
  'MA1': [(GD_T, "  /\\ UNCHANGED <<tomb, udel>>\n")],
  'MA2': [("                                   ELSE IF doc[p] # Nil THEN doc[p]\n",
           "                                   ELSE IF doc[p] # Nil THEN tomb[p]\n")],
  'MN2': [(GD_T, "  /\\ tomb' = [tomb EXCEPT ![p] = doc[p]]\n  /\\ UNCHANGED udel\n")],
}
for name, edits in M.items():
    s = src
    for a, b in edits: s = rep(s, a, b)
    d = os.path.join(out, name); os.makedirs(d, exist_ok=True)
    open(os.path.join(d, 'LeanP1.tla'), 'w').write(s)
    print(name, 'written')
