# mktables.py: tables-lv.md and tables-r17.md from the c31 / c33b job files (every cell read from a file)
import re, os, glob
C31 = os.path.dirname(os.path.abspath(__file__)); C33 = C31.replace('c31-ec2', 'c33-ec2')
def rows(path):
    out, cfg, start = [], {}, ''
    for l in open(path):
        l = l.rstrip()
        if l.startswith('JOBSTART'): start = l
        m = re.match(r'^cfg FlintCatchup(\S+)\.cfg (\w+)', l)
        if m: cfg[m.group(1)] = m.group(2); continue
        m = re.match(r'^(HOLD|VIOL|NOVERDICT|ERROR) (\S+) .*?got=\[(.*?)\]\s*(.*)$', l)
        if m:
            v, arm, got, rest = m.groups()
            full = re.search(r'(\d+) distinct states found\. Depth (\d+)', rest); lvl = re.search(r'depth (\d+), (\d+) generated, (\d+) distinct', rest)
            secs = re.search(r' (\d+)s( cap=\d+s)?$', rest)
            if v == 'HOLD': ver = f'HOLD {int(full[1]):,} / d{full[2]} (full)'
            elif v == 'VIOL':
                what = 'Prop (lasso)' if 'Temporal' in got else re.search(r'Invariant (\S+)', got)[1]
                ver = f'VIOL {what} d{full[2]} ({int(full[1]):,} distinct)'
            elif v == 'NOVERDICT': ver = f'no violation to the cap: d{lvl[1]}, {int(lvl[3]):,} distinct (bounded)'
            else: ver = f'ERROR rc / {got[:60]}'
            out.append((arm, ver, secs[1] if secs else ''))
        m = re.match(r'^PEEL (\S+) (HOLD|VIOL|NOVERDICT|ERROR) (.*)$', l)
        if m:
            base, v, rest = m.groups()
            if v == 'HOLD':
                n = len(re.search(r'\[(.*?)\]', rest)[1].split()); full = re.search(r'(\d+) distinct states found\. Depth (\d+)', rest)
                out.append((base, f'all {n} candidates HOLD: {int(full[1]):,} / d{full[2]} (full)', re.search(r'(\d+)s$', rest)[1]))
            elif v == 'VIOL': out.append((base, 'drops ' + rest.split()[0], ''))
            elif v == 'NOVERDICT':
                n = len(re.search(r'\[(.*?)\]', rest)[1].split()); lvl = re.search(r'depth (\d+), (\d+) generated, (\d+) distinct', rest)
                out.append((base, f'all {n} candidates: no violation to the cap: d{lvl[1]}, {int(lvl[3]):,} distinct (bounded)', ''))
    return out, cfg, start
def table(title, intro, files):
    L = [f'### {title}', '', intro, '', '| job | arm / base | cfg md5 | verdict | secs |', '|---|---|---|---|---|']
    for job, path in files:
        if not os.path.exists(path): continue
        r, cfg, start = rows(path)
        for arm, ver, secs in r: L.append(f'| {job} | {arm} | {cfg.get(arm, "")} | {ver} | {secs} |')
    return '\n'.join(L) + '\n'
J = lambda j: os.path.join(C31, 'out-jobs', j + '.txt')
lv = table('Liveness on rev 17 (FlintCatchupS 61ae6829, the proof\'s Full), two replicas, MaxFails 2',
  'c31 (m7i.48xlarge, i-08f520c6866db048b), tla-mc 0.2.2 compiled, 12-16 workers, -fpmem/-queue-mem 16384 MB. lv1 through FlintCatchupSLiveW.tla (752b1a56: SpecNoReturn = Spec less WF Return); lv2 through FlintCatchupSLiveW2.tla (6604c794: Prop_BarrierEndsM = a barrier ends or the raid has no member left; Prop_BarrierEndsH = ... or no member is healthy). The lv1 lasso: leg 2 begins to fail, the drain (or a second failure) takes leg 1, the failing leg fails the stamp: StampLand with no survivor, members = {}, attached and held forever.',
  [('lv1', J('lv1')), ('lv2', J('lv2'))])
r17 = table('rev 17 (FlintCatchupS 61ae6829): the identities, the four-replica zombie arm, the goal checks, the proof\'s candidate peels',
  'c33b (c6i.16xlarge, i-06c7808ed514f0dc1): tla-mc 0.2.1 compiled; c31 (m7i.48xlarge): tla-mc 0.2.2 compiled. Bounded rows stopped at their cap: no violation to the depth shown, not a full pass. Peel rows: the box\'s peel.sh (every candidate an INVARIANT; a violated one is dropped and the base rerun); the final set (66 conjuncts) is q5\'s.',
  [('pk3 (c33b, rev 16)', os.path.join(C33, 'out-pk3.txt')), ('pz1 (c33b, rev 16)', os.path.join(C33, 'out-pz1.txt')),
   ('pk8 (c33b)', os.path.join(C33, 'out-pk8.txt')), ('pg1 (c33b)', os.path.join(C33, 'out-pg1.txt')),
   ('q1', J('q1')), ('q2', J('q2')), ('q3', J('q3')), ('q4', J('q4')), ('q5', J('q5'))])
open(os.path.join(C31, 'tables-lv.md'), 'w').write(lv); open(os.path.join(C31, 'tables-r17.md'), 'w').write(r17)
print(lv); print(r17)
