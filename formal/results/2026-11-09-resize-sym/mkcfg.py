#!/usr/bin/env python3
"""Write the FlintCatchupSR symmetry cfgs of 2026-11-09 (gate item 7 at three replicas).

The knobs, shapes and invariants are the resize batch's (../2026-11-07-resize/mkcfg.py),
imported so these arms are its Res_/Res3_/Bug_/Design_ arms with only the ids, the
SYMMETRY and the VIEW changed.  Validation at two replicas, as the 2026-11-07 cluster
batch did for FlintCatchupSSym:

  Ids   model-value ids, no symmetry: must reproduce Res_*_r2f1a3 exactly
  Sym   SYMMETRY Perms (FlintCatchupSRSym.tla): every verdict and depth kept
  IdsV  VIEW ViewVars (FlintCatchupSRSymV.tla): no verdict changed
  SymV  both: the setting of the three-replica arms
  Bug_/Design_ under SymV: each still fails its intended invariant

Every arm also checks Inv_ReadUnique, the condition the symmetry rests on.

  python3 mkcfg.py            # writes cfg/*.cfg next to this script
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(HERE, '..', '2026-11-07-resize'))
import mkcfg as res  # noqa: E402  (its ARMS are built on import; nothing is written)

INV = res.SAFE + res.RINV + ['Inv_ReadUnique']
SPEC = {False: 'FlintCatchupSRSym.tla', True: 'FlintCatchupSRSymV.tla'}


def sizes(n, fails, maxasm=3):
    sz = res.sizes(n, fails, 1, 2, 1, maxasm)
    sz['Reps'] = '{%s}' % ', '.join(f'r{i}' for i in range(1, n + 1))
    return sz


ARMS = []
def arm(name, what, expect, shape, sz, sym, view, over=None):
    c = dict(res.FULL)
    c.update(res.SHAPES[shape])
    c.update(res.RES_ON)
    c.update(over or {})
    lines = [f'\\* {what}', f'\\* expect: {expect}', f'\\* spec: {SPEC[view]}', 'SPECIFICATION Spec', 'CONSTANTS']
    lines += [f'  {k} = {res.val(v)}' for k, v in sz.items()]
    lines += [f'  {k} = {res.val(v)}' for k, v in sorted(c.items())]
    lines += [f'INVARIANT {" ".join(INV)}', 'CHECK_DEADLOCK FALSE']
    lines += (['SYMMETRY Perms'] if sym else []) + (['VIEW ViewVars'] if view else [])
    ARMS.append((name, '\n'.join(lines) + '\n'))


TAGS = {'Ids': (False, False), 'Sym': (True, False), 'IdsV': (False, True), 'SymV': (True, True)}
HOW = {'Ids': 'model-value ids, no symmetry', 'Sym': 'SYMMETRY Perms',
       'IdsV': 'VIEW ViewVars', 'SymV': 'SYMMETRY Perms and VIEW ViewVars'}

# two replicas: the four settings against Res_*_r2f1a3 (natural ids, 2026-11-07)
for sh in ('lost', 'crash', 'zombie'):
    for tag, (sym, view) in TAGS.items():
        arm(f'V_{tag}_{sh}_r2f1a3', f'item 7 validation, {HOW[tag]}; Resize on, {sh}, 2 replicas, 1 failure, MaxVer 1 MaxEpoch 2 MaxAsm 3',
            'pass', sh, sizes(2, 1), sym, view)

# the planted bugs and design arms under SymV: each must still fail
for k, (over, inv, what) in res.BUGS.items():
    arm(f'V_SymV_Bug_{k}', f'item 7 validation, SymV: planted bug: {what}', inv, 'lost', sizes(2, 1), True, True, over=over)
arm('V_SymV_Design_NoRetryRun', 'item 7 validation, SymV: design: the retry of a crashed grow is a no-op success',
    'Inv_VacatedZero', 'lost', sizes(2, 1), True, True, over=dict(RetryRuns=False))
arm('V_SymV_Design_AckAfterGrow', 'item 7 validation, SymV: design: a leg counts as grown at the lvol resize, before its move',
    'Inv_FsRangeClean', 'lost', sizes(2, 1), True, True, over=dict(MoveBeforeAck=False))

# three replicas: the Res3 arms under SymV, on the cluster
for sh in ('lost', 'crash', 'zombie'):
    arm(f'R3_SymV_{sh}_r3f1a3', f'item 7, SymV; Resize on, {sh}, 3 replicas, 1 failure, MaxVer 1 MaxEpoch 2 MaxAsm 3',
        'pass', sh, sizes(3, 1), True, True)

if __name__ == '__main__':
    out = os.path.join(HERE, 'cfg')
    os.makedirs(out, exist_ok=True)
    for name, text in ARMS:
        open(os.path.join(out, name + '.cfg'), 'w').write(text)
    print(len(ARMS), 'cfgs in', out)
