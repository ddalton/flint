#!/usr/bin/env python3
"""pick.py FULL.tla OUT.tla NAME... -- a micro module: FULL's text with every
LEMMA/THEOREM proof that is not one of NAMEs (or before the first step lemma's
section) replaced by OMITTED, so tlapm checks only the named ones (and the
module still parses: every name stays defined)."""
import sys, re
src, out, names = sys.argv[1], sys.argv[2], set(sys.argv[3:])
L = open(src).read().split('\n')
res = []; i = 0
head = re.compile(r'^(LEMMA|THEOREM|COROLLARY) (\w+)')
while i < len(L):
    m = head.match(L[i])
    if not m:
        res.append(L[i]); i += 1; continue
    name = m.group(2)
    # the statement runs to the first line that starts a proof (<1>, BY, OBVIOUS, PROOF) at col 0
    j = i + 1
    stmt = [L[i]]
    while j < len(L) and not re.match(r'^(<\d+>|BY\b|OBVIOUS|PROOF|OMITTED)', L[j]) and not head.match(L[j]) and L[j].strip() != '':
        stmt.append(L[j]); j += 1
    # an inline BY on the statement's last line
    k = j
    while k < len(L) and L[k].strip() != '' and not head.match(L[k]) and not re.match(r'^(-{10,}|={10,}|\(\*|\w+(\(.*\))? ==)', L[k]):
        k += 1
    if name in names:
        res += L[i:k]
    else:
        s = '\n'.join(stmt)
        s = re.sub(r'\s+BY\s.*$', '', s, flags=re.S) if re.search(r'\bBY\b', stmt[-1]) and len(stmt) == 1 and '==' in stmt[0] and ' BY ' in stmt[0] else s
        res += s.split('\n') + ['OMITTED']
    i = k
open(out, 'w').write('\n'.join(res))
