#!/usr/bin/env python3
"""Print a tla-mc/TLC error trace as per-state changes: top-level variables that
changed, and for `w` each writer's changed fields."""
import sys, re
txt = open(sys.argv[1]).read()
states = re.split(r'\nState \d+:.*\n', '\n' + txt)
head, states = states[0], states[1:]
def parse(block):
    v = {}; cur = None
    for line in block.split('\n'):
        m = re.match(r'^/\\ (\w+) = (.*)$', line)
        if m: cur = m.group(1); v[cur] = m.group(2)
        elif cur and line and not line.startswith(('Error', 'tla-mc', 'real', 'user', 'sys', 'rc=')) and not re.match(r'^\d+ states', line):
            v[cur] += ' ' + line.strip()
    return v
def split_top(s, sep):
    out, d, cur = [], 0, ''
    i = 0
    while i < len(s):
        c = s[i]
        if s.startswith('<<', i): d += 1; cur += '<<'; i += 2; continue
        if s.startswith('>>', i): d -= 1; cur += '>>'; i += 2; continue
        if c in '([{': d += 1
        if c in ')]}': d -= 1
        if d == 0 and s.startswith(sep, i):
            out.append(cur); cur = ''; i += len(sep); continue
        cur += c; i += 1
    out.append(cur); return out
def writers(s):
    r = {}
    for part in split_top(s.strip()[1:-1] if s.strip().startswith('(') else s, ' @@ '):
        m = re.match(r'\s*(\w+) :> \[(.*)\]\s*$', part)
        if not m: continue
        f = {}
        for kv in split_top(m.group(2), ', '):
            k, _, val = kv.partition(' |-> ')
            f[k.strip()] = val.strip()
        r[m.group(1)] = f
    return r
prev = None
for i, b in enumerate(states, 1):
    v = parse(b)
    if prev is None:
        print(f'--- state 1 (init)'); prev = v; continue
    ch = [k for k in v if v[k] != prev.get(k) and k != 'w']
    out = [f'{k} = {v[k]}' for k in ch]
    if v.get('w') != prev.get('w'):
        a, c = writers(prev['w']), writers(v['w'])
        for wr in c:
            d = [f'{k}: {c[wr][k]}' for k in c[wr] if c[wr][k] != a.get(wr, {}).get(k)]
            if d: out.append(f'w[{wr}] ' + '; '.join(d))
    print(f'--- state {i}'); [print('   ', o) for o in out]
    prev = v
m = re.search(r'Error: .*', txt); print(m.group(0) if m else '')
