# vtable.py <root>: the per-batch verdict tables (c21-c25) for flint-cb's §12 verdict amendment, from out/cNN.txt and
# each arm's cfg under <root>/ms7..ms11.  Columns as flint-cb asked (10-30): arm, replicas + fault, knobs on, decree
# form, invariants, verdict, trace class (each VIOL), method, flag (a control to leave out, and why).
import re, sys, os
R = sys.argv[1]
B = [('c21', 'ms7', 'rev 7'), ('c22', 'ms8', 'rev 8'), ('c23', 'ms9', 'rev 9'), ('c24', 'ms10', 'rev 10'), ('c25', 'ms11', 'rev 11'), ('c26', 'ms12', 'rev 12'), ('c28', 'ms12', 'rev 12'), ('c30', 'ms12', 'rev 12'), ('c30b', 'ms12', 'rev 12')]
# batches run fresh through rerun2.sh (its RERUN header): the method column says so, not "rerun"
FRESH = {'c28', 'c30', 'c30b'}
WRAP = {'sreach': 'FlintCatchupSReach.tla', 'slivew': 'FlintCatchupSLiveW.tla', 'ssym': 'FlintCatchupSSym.tla'}
FAULT = {'Z': 'zombie', 'C': 'crash', 'L': 'lost mark'}
# the trace class of each VIOL (probes: "reachable"); anything missing prints UNCLASSIFIED so it is looked at
CLASS = {
  ('c21', 'SZDecreeAll'): 'stamp gap, control (step 4 as first written: the decree strands the last in_sync leg)',
  ('c21', 'SZDecreeUngated'): 'stamp gap, control (the decree ungated: the third gap)',
  ('c21', 'SZAck'): 'permitted: the zombie of a record-confirmed sole continuation acks (design)',
  ('c21', 'SZFree'): 'write-hole class as classed at rev 7; rev 10 splits it: pre-stamp junk = the hole, a torn leg = excluded',
  ('c21', 'SZClean0'): 'write-hole class as classed at rev 7 (decree off); rev 10 splits it as SZFree',
  ('c21', 'SZClean'): 'write-hole class as classed at rev 7; rev 10 splits it as SZFree',
  ('c21', 'SLNoRV'): 'stamp gap, control (RelView off: an unconfirmed minority stamp hides the released membership)',
  ('c21', 'SCNoRV'): 'stamp gap, control (RelView off)', ('c21', 'SZNoRV'): 'stamp gap, control (RelView off)',
  ('c21', 'SL0'): 'control: no stamp (FlintCatchupT, the d9-class residual)', ('c21', 'SC0'): 'control: no stamp (FlintCatchupT)',
  ('c21', 'SZ0'): 'control: no stamp (FlintCatchupT)',
  ('c21', 'SZF3NoMR'): 'stamp gap, control (majority read off: the stage serves a leg whose stamp predates an unreached majority stamp; via the zombie, two failures)',
  ('c21', 'SZ'): 'hole class, pre-stamp junk (the zombie write that finds the fence, readmitted by a later catch-up); not a stamp gap',
  ('c21', 'SZMem'): 'hole class, pre-stamp junk (as c21 SZ; the member read changes nothing here)',
  ('c21', 'SZF3'): 'hole class, pre-stamp junk (as c21 SZ: the same 18-state trace up to the order of two snapshots; the third failure is not used)',
  ('c21', 'SZDecreeSkip'): 'hole class, pre-stamp junk by another path (z\'s junk on its local leg is copied to a returning leg by its catch-up and served after a second move); MonitorSkipsHold removes the ungated decree\'s d13 violation (none at depth <= 16), but the d17 hole masks the rest: whether the alternative fix closes the third gap is NOT shown (needs a rev 10+ arm with pre-stamp junk legal)',
  ('c21', 'SLiveNoRec'): 'control, expected: with the record down a minority barrier never ends (Prop_BarrierEnds lasso)',
  ('c28', 'S2LiveLNoRec'): 'control, expected: with the record down a minority barrier never ends (Prop_BarrierEnds lasso)',
  ('c28', 'S2LiveCNoRec'): 'control, expected (as S2LiveLNoRec)', ('c28', 'S2LiveZNoRec'): 'control, expected (as S2LiveLNoRec)',
  ('c30b', 'S2PathNoRet'): 'positive control, expected: without WF(Return) a failed path may stay out (Prop_PathReturns lasso), so SpecNoReturn is what ran',
  ('c22', 'SZDecreeAll'): 'stamp gap, control (as c21)',
  ('c22', 'SZRisk'): 'the expiry\'s cost (serve-with-risk loses a write held only on an unreachable leg); marked: see RiskMarked',
  ('c23', 'S2ZV'): 'hole class, pre-stamp junk (rev 9 has no ZombiePreStampLegal; see c24)',
  ('c23', 'S2Z'): 'hole class, pre-stamp junk (as S2ZV)',
  ('c23', 'S2ZVAck'): 'permitted: the zombie of a record-confirmed sole continuation acks (design)',
  ('c24', 'S2ZT'): 'hole class, pre-stamp junk (TornExcluded alone)',
  ('c24', 'S2ZH'): 'torn leg served, control (TornExcluded off)',
  ('c24', 'SZH'): 'torn leg served, control (TornExcluded off)',
  ('c26', 'S2ZTHJ'): 'torn junk served unmarked, control (TornNeverSole off: the torn z serves alone on the vouch)',
  ('c26', 'S2ZHJ'): 'torn junk served unmarked, control (TornExcluded off: a torn z assembled beside a clean leg)',
  ('c25', 'S2ZTHMarked'): 'torn leg served alone without the mark, control (TornNeverSole off: the d5 shape)',
}
# controls to leave out of the citations, and why
FLAG = {
  ('c21', 'SLF3NoMR'): 'WRONG CONFIGURATION: the model\'s last healthy member never fails and nothing fails after unstage, so the lost-mark shape cannot reach the gap; leave out',
  ('c21', 'SCF3NoMR'): 'WRONG CONFIGURATION (as SLF3NoMR); leave out',
  ('c23', 'S2ReachVouchLOff'): 'BY CONSTRUCTION: with the vouch off no stamped stage can set the ghost; shows only that the knob removes the rule (the situation itself is reachable: VouchSpot); do not cite as evidence of deferral',
  ('c23', 'S2ReachVouchZOff'): 'BY CONSTRUCTION (as S2ReachVouchLOff)', ('c23', 'S2ReachVouchCOff'): 'BY CONSTRUCTION (as S2ReachVouchLOff)',
  ('c24', 'SZH'): 'CONTROL NOT KILLED within the cap (no violation to d17 at three replicas, bounded); rules out nothing; the killed control is two-replica S2ZH (VIOL d17)',
  ('c25', 'S2ReachTornServedZN'): 'BY CONSTRUCTION: with TornNeverSole an ordinary stage assembles only clean legs; shows the wiring, not a property',
  ('c25', 'S2ZTNRiskMarked'): 'PART BY CONSTRUCTION: its Inv_TornMarked half restates the guard; cite only the Inv_LossMarked half (every loss under the expiry is marked)',
  ('c25', 'S2ZTNMarked'): 'BY CONSTRUCTION: Inv_TornMarked restates TornNeverSole\'s guard (an ordinary stage needs a clean member and then takes only clean legs); TornNeverSole carries weight by its control S2ZTHMarked and the expiry probe',
}
def cfg(d, c):
    p = f'{R}/{d}/FlintCatchup{c}.cfg'
    if not os.path.exists(p): return {}, [], []
    t = open(p).read()
    k = dict(re.findall(r'^\s+(\w+) = (.+)$', t, re.M)); sp = re.search(r'^SPECIFICATION (\w+)', t, re.M)
    if sp: k['__spec'] = sp.group(1)
    return k, re.findall(r'^(?:INVARIANT|PROPERTY) (\w+)', t, re.M), re.findall(r'^PROPERTY (\w+)', t, re.M)
def g(k, n, d='-'): return k.get(n, d).strip('"')
for b, d, rev in B:
    p = f'{R}/out/{b}.txt'
    if not os.path.exists(p): continue
    L = open(p).read().splitlines()
    hs = sorted({(a, w, r) for l in L for a, w, r in re.findall(r'^(?:RESUME |RERUN (?:tla-mc \S+ )?|BATCH (?:tla-mc \S+ )?)?S ([0-9a-f]{8}) (sreach|slivew|ssym) ([0-9a-f]{8})', l)})
    rows, meth, rr = {}, {}, {}
    for l in L:
        if l.startswith('RERUN ') or l.startswith('BATCH '):
            caps = re.search(r'fpmem (\d+) queue-mem (\d+)', l).groups()
            ver = re.search(r'tla-mc (\S+)', l); caps = (caps[0], caps[1] + (f' MB, tla-mc {ver.group(1)}' if ver else ' MB, tla-mc 0.1.2')) 
            wk = re.search(r'workers (\d+)', l)
            if wk and wk.group(1) != '3': caps = (caps[0], caps[1] + (f', {wk.group(1)} workers' if b in FRESH else f', {wk.group(1)} workers (a parallel runner)'))
            for a in re.search(r'arms: (.*?) \d\d:\d\d:\d\dZ', l).group(1).split(): rr[a.split(':')[0]] = caps
            continue
        m = re.match(r'^(HOLD|VIOL|NOVERDICT|ERROR) (\S+) \((\S+)\) rc=(\d+) got=\[(.*?)\] ?(.*)$', l)
        if not m: continue
        c = m.group(2)
        if c in rr and c in rows and rows[c][0] == 'ERROR': meth[c] = f'first run cut by the memory guard; rerun with -fpmem/-queue-mem {rr[c][0]}/{rr[c][1]}'
        elif c in rr: meth[c] = f'{"first run" if b in FRESH else "rerun"}, -fpmem/-queue-mem {rr[c][0]}/{rr[c][1]}'
        rows[c] = m.groups()[0:1] + m.groups()[2:]
    stopped = [l for l in L if l.startswith('STOPPED')]
    print(f'\n### {b}: FlintCatchupS {rev}; ' + '; '.join(f'FlintCatchupS.tla {a}, {WRAP[w]} {r}' for a, w, r in hs) + '\n')
    print('| arm | replicas, fault | knobs on | decree form | invariants | verdict | trace class | method | flag |')
    print('|---|---|---|---|---|---|---|---|---|')
    for c, (v, spec, rc, got, rest) in rows.items():
        k, inv, props = cfg(d, c)
        n = g(k, 'Reps').count(',') + 1 if k else '?'
        sh = 'Z' if g(k, 'ZombieMoves') == 'TRUE' else 'C' if g(k, 'Crash') == 'TRUE' else 'L'
        rf = f'{n}, {FAULT[sh]}' + (f', MaxFails {g(k, "MaxFails")}' if g(k, 'MaxFails') != '2' else '') + (', record down' if g(k, 'RecordUp') == 'FALSE' else '')
        on = [] if g(k, 'Stamp') == 'TRUE' else ['NO STAMP']
        on += [f'MajorityRead {g(k, "MajorityRead")}'] if g(k, 'Stamp') == 'TRUE' else []
        on += [x for x in ('RelView', 'DeferExpires', 'MonitorSkipsHold', 'TornExcluded', 'ZombiePreStampLegal', 'TornNeverSole') if g(k, x) == 'TRUE']
        on += [] if g(k, 'EvenVouch', 'off') == 'off' else [f'EvenVouch {g(k, "EvenVouch")}']
        on += [] if g(k, '__spec', 'Spec') == 'Spec' else [f'SPECIFICATION {g(k, "__spec")}']
        if g(k, 'Stamp') == 'TRUE' and g(k, 'RelView') == 'FALSE': on.append('RelView OFF')
        if g(k, 'Decree') == 'FALSE': df = 'no decree'
        elif g(k, 'DecreeGated') == 'FALSE': df = 'ungated (any stage attempt)' + ('' if g(k, 'DecreeSparesLast') == 'TRUE' else ', last leg not spared')
        else: df = ('majority read' if g(k, 'MajorityRead') != 'off' else 'any conclusive read (majority read off)') + (' or the vouch' if g(k, 'EvenVouch') == 'decree' else '') + ('' if g(k, 'DecreeSparesLast') == 'TRUE' else ', last leg not spared')
        if g(k, 'Stamp') != 'TRUE': df = '-'
        probe = bool(inv) and all(i.startswith('Reach_') for i in inv)
        st = re.search(r'(\d+) distinct states found\. Depth (\d+)', rest); bd = re.search(r'depth (\d+), \d+ generated, (\d+) distinct', rest)
        if v == 'HOLD': vv = (f'unreachable, full space {int(st.group(1)):,} / d{st.group(2)}' if probe else f'HOLD, full space {int(st.group(1)):,} / d{st.group(2)}')
        elif v == 'VIOL' and 'emporal' in got: vv = f'VIOL {", ".join(props) or "a temporal property"} (lasso) d{st.group(2)}' if st else 'VIOL (temporal)'
        elif v == 'VIOL':
            w = re.search(r'(?:Invariant|propert\w*) (\w+)', got).group(1)
            w = w.replace('Inv_', '')
            vv = (f'reachable d{st.group(2)}' if probe else f'VIOL {w} d{st.group(2)}') if st else f'VIOL {w}'
        elif v == 'NOVERDICT' and props: vv = f'NO VERDICT: cut at the cap (d{bd.group(1)}, {int(bd.group(2)):,} states) before the liveness check could conclude; claims nothing' if bd else 'NO VERDICT (liveness, cut at the cap)'
        elif v == 'NOVERDICT': vv = f'no violation to the cap ({int(bd.group(2)):,} / d{bd.group(1)}): bounded' if bd else 'cap, no progress line'
        else: vv = f'ERROR rc={rc}'
        cl = ('' if v != 'VIOL' else 'probe: the state is reachable' if probe else CLASS.get((b, c), 'UNCLASSIFIED'))
        print(f'| {c} | {rf} | {", ".join(on) or "-"} | {df} | {", ".join(i.replace("Inv_", "") for i in inv)} | {vv} | {cl} | {meth.get(c, "")} | {FLAG.get((b, c), "")} |')
    for s in stopped: print(f'\n_{s}_')
