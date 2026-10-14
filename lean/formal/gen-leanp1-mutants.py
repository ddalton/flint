#!/usr/bin/env python3
"""The LeanP1 gate's MODEL mutations (2026-10-14): copies of LeanP1.tla with
one rule broken that no rule constant switches off.  Written to
OUTDIR/<name>/LeanP1.tla, never over the module; the gate runs a world's
module in that directory (WORLDS-LeanP1.tsv, fourth column).  Each edit
asserts its anchor occurs exactly once, so a changed LeanP1.tla fails here
loudly rather than producing a mutant that mutates nothing.

  MN2       the gateway's delete is not acknowledged in `udel` (the tombstone
            stays): NR5 must break (results/2026-10-07-tlaps-m5 E14, E35).
  MA1       the gateway's delete records nothing, no tombstone and no `udel`:
            AN5 must break (E14, E41).
  EditBase  Edit's ghost base is the baseline, as before 2026-10-13: the
            scripted traces E27, E31, E33 must break NR5 (the fix they pin).

Run from lean/formal:  python3 gen-leanp1-mutants.py OUTDIR [NAME...]"""
import os, sys
out = sys.argv[1]
want = sys.argv[2:]
src = open('LeanP1.tla').read()
GD_T = "  /\\ tomb' = [tomb EXCEPT ![p] = doc[p]]\n  /\\ udel' = udel \\cup {<<p, doc[p]>>}\n"
EDIT = ("     /\\ base' = [base EXCEPT ![h] = IF w[s].local[p] # Nil THEN w[s].local[p]\n"
        "                                    ELSE IF p \\in w[s].uploads THEN w[s].snap[p]\n"
        "                                    ELSE w[s].baseline[p]]\n")
M = {
  'MN2': [(GD_T, "  /\\ tomb' = [tomb EXCEPT ![p] = doc[p]]\n  /\\ UNCHANGED udel\n")],
  'MA1': [(GD_T, "  /\\ UNCHANGED <<tomb, udel>>\n")],
  'EditBase': [(EDIT, "     /\\ base' = [base EXCEPT ![h] = w[s].baseline[p]]\n")],
}
for name, edits in M.items():
    if want and name not in want:
        continue
    s = src
    for old, new in edits:
        assert s.count(old) == 1, (name, old[:60], s.count(old))
        s = s.replace(old, new)
    d = os.path.join(out, name)
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, 'LeanP1.tla'), 'w').write(s)
    print(f'{name}: {d}/LeanP1.tla')
