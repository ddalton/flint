#!/usr/bin/env python3
"""Print a TLC counterexample as per-state DIFFS instead of full states.

Usage: python3 tracediff.py <tlc-output.log>

TLC prints every variable of every state in a trace; on this module that is
~200 lines a state, and what matters is what CHANGED. This prints state 1's
manifest/objects/cell and then, per state, only the fields that moved
(records and functions are descended so `sc.A.baseline.p1: 1 -> 0` names the
field). Written during the 2026-09-18 review; every H1b-H1f trace was read
with it.
"""
import re, sys
txt = open(sys.argv[1]).read()
m = re.search(r"Error: The behavior up to this point is:\n(.*?)\n\d+ states generated", txt, re.S)
body = m.group(1).replace('|->','↦').replace(':>','↣').replace('<<','⟨').replace('>>','⟩')
states = re.split(r"^State \d+: <[^\n]*>\n", body, flags=re.M)[1:]
def split_top(s, sep=','):
    out, depth, cur = [], 0, []
    for ch in s:
        if ch in '[({⟨': depth += 1
        if ch in '])}⟩': depth -= 1
        if ch == sep and depth == 0:
            out.append(''.join(cur)); cur = []
        else: cur.append(ch)
    if ''.join(cur).strip(): out.append(''.join(cur))
    return out
def parse_vars(st):
    vars = {}
    for chunk in re.split(r"^/\\ ", st, flags=re.M):
        chunk = chunk.strip()
        if not chunk: continue
        name, _, val = chunk.partition(' = ')
        vars[name.strip()] = re.sub(r"\s+", " ", val.strip())
    return vars
def rec_fields(v):
    # [a |-> x, b |-> y]  or  (k :> v @@ k2 :> v2)  or scalar
    if v.startswith('[') and '↦' in v:
        inner = v[1:-1]
        d = {}
        for f in split_top(inner):
            k, _, x = f.partition('↦')
            d[k.strip()] = x.strip()
        return d
    if v.startswith('(') and '↣' in v:
        d = {}
        for f in split_top(v[1:-1], '@'):  # crude: split on @@
            pass
        parts = re.split(r"\s@@\s", v[1:-1])
        for f in parts:
            k, _, x = f.partition('↣')
            d[k.strip()] = x.strip()
        return d
    return None
def diff(a, b, prefix=''):
    fa, fb = rec_fields(a), rec_fields(b)
    if fa is not None and fb is not None and set(fa) == set(fb):
        for k in fa:
            if fa[k] != fb[k]:
                diff(fa[k], fb[k], prefix + '.' + k.strip('"'))
    else:
        print(f"    {prefix}: {a}  ->  {b}")
prev = None
for i, st in enumerate(states, 1):
    v = parse_vars(st)
    if prev is None:
        print(f"State 1 (init):")
        for k in ['manifest', 'objects', 'cellEpoch', 'cellHolder']:
            print(f"    {k} = {v.get(k)}")
    else:
        print(f"State {i}:")
        for k in v:
            if prev.get(k) != v[k]:
                diff(prev[k], v[k], k)
    prev = v
