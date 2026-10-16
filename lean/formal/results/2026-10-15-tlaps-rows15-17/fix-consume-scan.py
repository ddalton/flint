#!/usr/bin/env python3
"""Rows 15-17: Consume picks a clock `ck` (CarryClocks), its trees take it as an
argument; a carrying Scan confirms every first absence (dels = ScanAbsent).
Applied to the hand-written M5C, M5D, M6 modules (this directory's copies)."""
import re
PICK = ("{I}<{L}>ck. PICK ck \\in CarryClocks(s) :\n"
        "{I}          IF CheapPath(s)\n"
        "{I}          THEN w' = [w EXCEPT ![s] = ConsumeCheapW(s, ck)]\n"
        "{I}          ELSE \\E fail \\in SUBSET ConsumeOwed(s) : w' = [w EXCEPT ![s] = ConsumeW(s, fail, ck)]\n"
        "{I}  BY {by}DEF Consume\n")
def pick(L, by=''):
    return PICK.format(I='  ' * (L - 1), L=L, by=by)
def sub_in(t, start, end, pairs):
    a = t.index(start); b = t.index(end, a + len(start))
    seg = t[a:b]
    for old, new, cnt in pairs:
        assert seg.count(old) == cnt, (start, old, seg.count(old))
        seg = seg.replace(old, new)
    return t[:a] + seg + t[b:]
SCAN = ('PICK dels \\in SUBSET ScanAbsent(s)', 'PICK dels \\in IF Carries(s) THEN {ScanAbsent(s)} ELSE SUBSET ScanAbsent(s)')
CW = [('ConsumeCheapW(s)', 'ConsumeCheapW(s, ck)'), ('ConsumeW(s, fail)', 'ConsumeW(s, fail, ck)')]

# M5C
p = 'm5c/LeanP1ProofM5C.tla'; t = open(p).read()
t = sub_in(t, 'LEMMA Consume_C ==', 'LEMMA', [
    ('<1>1. CASE CheapPath(s)\n', pick(1) + '<1>1. CASE CheapPath(s)\n', 1),
    ('BY <1>1 DEF Consume\n', 'BY <1>1, <1>ck\n', 1),
    ('BY <1>2 DEF Consume\n', 'BY <1>2, <1>ck\n', 1)] + [(o, n, t[t.index('LEMMA Consume_C =='):].split('\nLEMMA')[0].count(o)) for o, n in CW])
t = sub_in(t, '<1>co. ASSUME NEW s \\in Writers, Consume(s) PROVE NRPre', '<1>sy.', [
    ('  <2>1. CASE CheapPath(s)\n', pick(2, '<1>co ') + '  <2>1. CASE CheapPath(s)\n', 1),
    ('BY <1>co, <2>1 DEF Consume', 'BY <2>ck, <2>1', 1),
    ('BY <1>co, <2>2 DEF Consume', 'BY <2>ck, <2>2', 1)] + [(o, n, t[t.index('<1>co. ASSUME'):t.index('<1>sy.')].count(o)) for o, n in CW])
assert t.count(SCAN[0]) == 2, t.count(SCAN[0]); t = t.replace(SCAN[0], SCAN[1])
open(p, 'w').write(t)

# M5D
p = 'm5d/LeanP1ProofM5D.tla'; t = open(p).read()
seg = t[t.index('LEMMA Consume_D =='):t.index('LEMMA Sync_D')]
t = sub_in(t, 'LEMMA Consume_D ==', 'LEMMA Sync_D', [
    ('<1>1. CASE CheapPath(s)\n', pick(1) + '<1>1. CASE CheapPath(s)\n', 1),
    ('BY <1>1, <1>k DEF Consume, WStep', 'BY <1>1, <1>k, <1>ck DEF WStep', 1),
    ('BY <1>2, <1>k DEF Consume, WStep', 'BY <1>2, <1>k, <1>ck DEF WStep', 1)] + [(o, n, seg.count(o)) for o, n in CW])
assert t.count(SCAN[0]) == 1; t = t.replace(SCAN[0], SCAN[1])
open(p, 'w').write(t)

# M6
p = 'm6/LeanP1ProofM6.tla'; t = open(p).read()
seg = t[t.index('LEMMA Consume_M6 =='):t.index('LEMMA Sync_M6')]
t = sub_in(t, 'LEMMA Consume_M6 ==', 'LEMMA Sync_M6', [
    ('<1>1. CASE CheapPath(s)\n', pick(1) + '<1>1. CASE CheapPath(s)\n', 1),
    ('BY <1>1 DEF Consume\n', 'BY <1>1, <1>ck\n', 1),
    ('BY <1>2 DEF Consume\n', 'BY <1>2, <1>ck\n', 1)] + [(o, n, seg.count(o)) for o, n in CW])
assert t.count(SCAN[0]) == 1; t = t.replace(SCAN[0], SCAN[1])
open(p, 'w').write(t)
print('ok')
