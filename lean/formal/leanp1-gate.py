#!/usr/bin/env python3
"""The LeanP1 gate's runner (2026-10-14): every world of WORLDS-LeanP1.tsv,
judged as the EC2 gate judged it (results/2026-10-04-leanp1-gate/scripts/
leangate.py), on tla-mc -- the default checker; TLC is the reference.

WORLDS-LeanP1.tsv, one world per line, tab-separated:
  <world> <expectation> [<module> [<mutant>]]
  expectation: HOLDS (no error), RECORD (run, not judged), Temporal (a
  liveness violation), or the name a violation must carry (an invariant,
  an action property, HReach for a scripted trace's end).  module: the
  .tla to check (default LeanP1.tla).  mutant: a gen-leanp1-mutants.py name;
  the world then runs in a copy of this directory whose LeanP1.tla is that
  mutant -- never over the module.

  python3 leanp1-gate.py [--only REGEX] [--skip REGEX] [--jobs N]
                         [--workers N] [--cap SECS] [--out DIR] [--compile]
                         [--tla-mc PATH] [--extra "TLA-MC ARGS"]

Prints one line per world and a verdict; exits 1 on any MISMATCH or a world
not decided within its cap (UNDECIDED), 0 when every run world is as
expected.  A world skipped is not a world passed: the summary names them."""
import argparse, os, re, shutil, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
ap = argparse.ArgumentParser()
ap.add_argument('--only', default='')
ap.add_argument('--skip', default='')
ap.add_argument('--jobs', type=int, default=1)
ap.add_argument('--workers', type=int, default=2)
ap.add_argument('--cap', type=int, default=3600)
ap.add_argument('--out', default=os.path.join(HERE, 'states', 'leanp1-gate'))
ap.add_argument('--compile', action='store_true')
ap.add_argument('--tla-mc', default=os.environ.get('TLA_MC', 'tla-mc'))
ap.add_argument('--extra', default='', help='more tla-mc arguments, e.g. "-fpmem 32000 -queue-mem 120000"')
a = ap.parse_args()

worlds = []
for l in open(os.path.join(HERE, 'WORLDS-LeanP1.tsv')):
    c = l.rstrip('\n').split('\t')
    if not c[0]:
        continue
    w, exp = c[0], c[1]
    mod = c[2] if len(c) > 2 and c[2] else 'LeanP1.tla'
    mut = c[3] if len(c) > 3 and c[3] else ''
    if a.only and not re.search(a.only, w):
        continue
    if a.skip and re.search(a.skip, w):
        continue
    worlds.append((w, exp, mod, mut))
os.makedirs(a.out, exist_ok=True)

# The mutants, each in its own copy of this directory.
muts = sorted({m for _, _, _, m in worlds if m})
for m in muts:
    d = os.path.join(a.out, 'mut', m)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    for f in os.listdir(HERE):
        if f.endswith('.tla'):
            shutil.copy(os.path.join(HERE, f), d)
    subprocess.run([sys.executable, os.path.join(HERE, 'gen-leanp1-mutants.py'), os.path.join(a.out, 'mut'), m],
                   cwd=HERE, check=True, stdout=subprocess.DEVNULL)

def judge(exp, out, capped):
    got = re.search(r"Invariant [A-Za-z0-9_]+ is violated|Action property [A-Za-z0-9_]+ is violated|"
                    r"Temporal propert[^.\n]* violated|No error has been found|^Error: [^\n]*", out, re.M)
    got = got.group(0) if got else ''
    if capped:
        v = 'RECORDED-CAP' if exp == 'RECORD' else 'UNDECIDED'
    elif exp == 'RECORD':
        v = 'RECORDED'
    elif exp == 'HOLDS':
        v = 'OK-HOLDS' if got == 'No error has been found' else 'MISMATCH'
    elif exp == 'Temporal':
        v = 'OK-FIRES' if 'Temporal propert' in got else 'MISMATCH'
    else:
        v = 'OK-FIRES' if re.search(r'(Invariant|Action property) ' + re.escape(exp) + r' is violated', got) else 'MISMATCH'
    m = re.findall(r'[0-9]+ states generated, [0-9]+ distinct states found\. Depth [0-9]+', out)
    return v, got, (m[-1] if m else '')

def run(world):
    w, exp, mod, mut = world
    cwd = os.path.join(a.out, 'mut', mut) if mut else HERE
    cfg = os.path.join(HERE, w + '.cfg')
    env = dict(os.environ)
    if a.compile:
        env['TLAMC_CACHE'] = os.path.join(a.out, 'cache-' + w)
    md = os.path.join(a.out, 'meta-' + w)
    shutil.rmtree(md, ignore_errors=True)
    cmd = ([a.tla_mc] + (['-compile'] if a.compile else [])
           + ['-workers', str(a.workers), '-metadir', md, '-checkpoint', '0'] + a.extra.split()
           + ['-config', cfg, mod])
    t0 = time.time(); capped = False
    path = os.path.join(a.out, w + '.out')
    with open(path, 'w') as o:
        o.write('# ' + ' '.join(cmd) + '  (cwd ' + cwd + ')\n'); o.flush()
        p = subprocess.Popen(cmd, cwd=cwd, env=env, stdout=o, stderr=subprocess.STDOUT)
        try:
            p.wait(timeout=a.cap)
        except subprocess.TimeoutExpired:
            p.kill(); p.wait(); capped = True
    shutil.rmtree(md, ignore_errors=True)
    if a.compile:
        shutil.rmtree(env['TLAMC_CACHE'], ignore_errors=True)
    v, got, size = judge(exp, open(path).read(), capped)
    line = f"{w:34} {v:12} exp={exp:28} | {got[:58]:58} | {size} | {round(time.time() - t0)}s"
    print(line, flush=True)
    return v

with ThreadPoolExecutor(max(1, a.jobs)) as ex:
    vs = list(ex.map(run, worlds))
bad = [w for (w, *_), v in zip(worlds, vs) if v in ('MISMATCH', 'UNDECIDED')]
print(f"leanp1 gate: {len(worlds)} world(s) run, {len(worlds) - len(bad)} as expected, "
      f"{len(bad)} not: {' '.join(bad) if bad else '-'}")
sys.exit(1 if bad else 0)
