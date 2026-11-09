#!/usr/bin/env python3
"""Write the ForgeSync rewind worlds of the 2026-11-07 cluster batch.

The open question from 2026-10-05 (../2026-10-05-forgesync-rewind/SUMMARY.txt):
at two syncers, two pushes, one fold, ONE rewind and ONE re-push, neither the
restore fix (RestoreDropsNamedRetention, restore.rs ce2a1af6) nor its batch-side
twin (NamingUnretains, which the code lacks) changes a reachable state.  Larger
rewind bounds were untested.  Each world here is ForgeSyncCodeRewind.cfg with
its bounds or those two rules changed; nothing else.

  Code  = the code now: the restore fix ON, the batch twin OFF
  NoFix = the code before ce2a1af6: both OFF
  Model = both ON (the sandbox's rules)

If the three spaces stay identical at a bound, neither rule is load-bearing
there; a violation in Code or NoFix is the trace that justifies the twin.

  python3 mkcfg.py           # writes cfg/*.cfg next to this script
"""
import os
import re

HERE = os.path.dirname(os.path.abspath(__file__))
TEMPLATE = os.path.join(HERE, '..', '..', 'ForgeSyncCodeRewind.cfg')

RULES = {
    'Code': dict(RestoreDropsNamedRetention='TRUE', NamingUnretains='FALSE'),
    'NoFix': dict(RestoreDropsNamedRetention='FALSE', NamingUnretains='FALSE'),
    'Model': dict(RestoreDropsNamedRetention='TRUE', NamingUnretains='TRUE'),
}

# the bounds, smallest first; R1S1 is the 10-05 world (identity: 1,535,961,438)
BOUNDS = {
    'R1S1': dict(MaxRewinds='1', MaxResends='1'),
    'R2S1': dict(MaxRewinds='2', MaxResends='1'),
    'R1S2': dict(MaxRewinds='1', MaxResends='2'),
    'R2S2': dict(MaxRewinds='2', MaxResends='2'),
    'P3R1S1': dict(Pushes='{p1, p2, p3}', MaxRewinds='1', MaxResends='1'),
}


def world(bound, rule):
    text = open(TEMPLATE).read()
    for k, v in {**BOUNDS[bound], **RULES[rule]}.items():
        text, n = re.subn(r'(?m)^(  %s = ).*$' % k, r'\g<1>' + v, text)
        assert n == 1, (k, n)
    head = (f'\\* 2026-11-07 rewind world {bound} {rule}: ForgeSyncCodeRewind.cfg with '
            + ', '.join(f'{k} = {v}' for k, v in {**BOUNDS[bound], **RULES[rule]}.items()) + '\n'
            + '\\* expect: pass\n\\* spec: ForgeSync.tla\n')
    return head + text


if __name__ == '__main__':
    out = os.path.join(HERE, 'cfg')
    os.makedirs(out, exist_ok=True)
    n = 0
    for b in BOUNDS:
        for r in RULES:
            with open(os.path.join(out, f'F_{b}_{r}.cfg'), 'w') as f:
                f.write(world(b, r))
            n += 1
    print(n, 'cfgs in', out)
