# mktables-c31.py: tables-c31.md, the gate arms c31 ran before the switch to the proof (out/c31.txt), every cell read
# from a file: verdicts from out/c31.txt, builds from out/RESULTS.txt, cfg md5s + bounds + properties from the cfgs in
# in/formal.tgz (ms12/, the files run.sh asserted: md5ok=1), the model md5 from out/c31.txt's S line.
import re, os, io, tarfile, hashlib
D = os.path.dirname(os.path.abspath(__file__))
tf = tarfile.open(f'{D}/in/formal.tgz')
def member(name):
    for m in tf.getmembers():
        if m.name.endswith('ms12/' + name): return tf.extractfile(m).read()
c31 = open(f'{D}/out/c31.txt').read().splitlines(); res = open(f'{D}/out/RESULTS.txt').read()
head = c31[0]; S = re.search(r'^S (\w+)', head)[1]
model = member('FlintCatchupS.tla'); assert hashlib.md5(model).hexdigest()[:8] == S
rows = []
for l in c31[1:]:
    m = re.match(r'^(HOLD|VIOL|NOVERDICT|ERROR|CUT) (\S+?)(?: |$)(.*)$', l)
    if not m: continue
    v, arm, rest = m.groups()
    cfg = member(f'FlintCatchup{arm}.cfg').decode(); md = hashlib.md5(cfg.encode()).hexdigest()[:8]
    reps = re.search(r'^\s*Reps = (.*)$', cfg, re.M)[1]; mf = re.search(r'^\s*MaxFails = (\d+)', cfg, re.M)[1]
    props = ' '.join(re.findall(r'^(?:INVARIANT|PROPERTY) (\S+)', cfg, re.M))
    what = re.sub(r'^c(27|31) \(EC2\): ', '', re.findall(r'^\\\* (.*)$', cfg, re.M)[3]).split(':')[0]
    b = re.search(rf'^BUILD {arm} gensrc (\w+) lockdeps (\w+) \(want gensrc (\w+) lockdeps (\w+)\)', res, re.M)
    build = f'{b[1]}/{b[2]}' + ('' if (b[1], b[2]) == (b[3], b[4]) else ' MISMATCH')
    if v == 'HOLD':
        full = re.search(r'(\d+) distinct states found\. Depth (\d+)', rest); ver = f'HOLD {int(full[1]):,} / d{full[2]} (full)'
    elif v == 'NOVERDICT':
        lv = re.search(r'depth (\d+), (\d+) generated, (\d+) distinct', rest); ver = f'no violation to the cap: d{lv[1]}, {int(lv[3]):,} distinct (bounded)'
    elif v == 'CUT': ver = 'CUT, not run' + re.search(r'(\(the user.*?\))', l)[1].join([' ', ''])
    else: ver = f'{v} {rest[:60]}'
    secs = re.search(r' (\d+)s cap=(\d+)s', rest)
    rows.append((arm, what, reps, mf, props, md, build, ver, f'{secs[1]} / {secs[2]}' if secs else ''))
out = io.StringIO()
out.write(f'### c31 gate arms (items 5 + 6): FlintCatchupS rev 12 ({S}), before the switch to the proof\n\n')
out.write(f'c31 (m7i.48xlarge us-west-1a, i-08f520c6866db048b): tla-mc 0.2.2 compiled, one arm at a time, 190 workers, -fpmem/-queue-mem 180000 MB (out/c31.txt: `{head}`). '
          'Every input md5 asserted before any build (in/run.sh; out/RESULTS.txt: md5ok=1); every arm\'s generated source and locked dependency set asserted equal to the box\'s (in/gensrc.expect). '
          'Bounded rows stopped at their cap: no violation to the depth shown, not a full pass.\n\n')
out.write('| arm | setting (cfg header) | Reps | MaxFails | checked | cfg md5 | gensrc / lockdeps | verdict | secs / cap |\n|---|---|---|---|---|---|---|---|---|\n')
for r in rows: out.write('| ' + ' | '.join(r) + ' |\n')
open(f'{D}/tables-c31.md', 'w').write(out.getvalue()); print(out.getvalue())
