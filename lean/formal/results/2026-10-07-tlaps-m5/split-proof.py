#!/usr/bin/env python3
"""Split LeanP1Proof.tla into a chain of modules, one per milestone, each
EXTENDING the one before: LeanP1ProofM0 (TypeOK), M1, M2, M3, M4, M5A, M5B.

Why: tlapm checks only the obligations of the module it is given -- an
EXTENDed module's theorems are facts, its proofs are not re-checked
(tested: /mnt/nvme/splittest, SB EXTENDS SA, 2 obligations, SA's failing
proof not attempted).  The whole-module record run reached 28.9 GiB on the
30 GiB box at M5B; one module per milestone bounds each run by its own
milestone, and the modules can be checked in parallel.

The text is the generator's, byte for byte: each module is the lines from
one milestone's section rule to the next, plus a MODULE header and EXTENDS.
Run from lean/formal:
  python3 results/2026-10-07-tlaps-m5/split-proof.py LeanP1Proof.tla OUTDIR"""
import sys, os
src, out = sys.argv[1], sys.argv[2]
L = open(src).read().split('\n')
RULE = '-' * 78
# The headers that open each milestone (the section rule is the line before).
HEADS = [('M1', '(* M1: Inv_OneHolder'), ('M2', '(* M2: Inv_CitationsLive'),
         ('M3', '(* M3: Inv_ShortcutSound'), ('M4', '(* M4: Inv_ReaderFetches'),
         ('M5A', '(* M5, part A: the history.'), ('M5B', '(* M5, part B: Prop_NoSilentRevert.')]
cuts = []
for name, head in HEADS:
    idx = [i for i, l in enumerate(L) if l.startswith(head)]
    assert len(idx) == 1, (head, idx)
    i = idx[0]
    assert L[i - 1] == RULE, (head, L[i - 1])
    cuts.append((name, i - 1))
end = [i for i, l in enumerate(L) if l.startswith('=' * 78)]
assert len(end) == 1 and L[0].startswith('-' * 28 + ' MODULE LeanP1Proof '), (end, L[0])
end = end[0]
os.makedirs(out, exist_ok=True)
def header(name):
    t = ' MODULE LeanP1Proof%s ' % name
    pad = 78 - len(t)
    return '-' * (pad // 2) + t + '-' * (pad - pad // 2)
bounds = [('M0', 0)] + cuts + [(None, end)]
total = 0
for (name, a), (_, b) in zip(bounds, bounds[1:]):
    if name == 'M0':
        body = [header('M0')] + L[1:b]
    else:
        body = [header(name), 'EXTENDS LeanP1Proof' + prev, ''] + L[a:b]
    body += ['=' * 78, '']
    path = os.path.join(out, 'LeanP1Proof%s.tla' % name)
    open(path, 'w').write('\n'.join(body))
    total += b - (1 if name == 'M0' else a)
    print(path, len(body) - 1, 'lines')
    prev = name
assert total == end - 1, (total, end)
