#!/usr/bin/env python3
"""Run tlc-rs on the tlaplus/Examples exhaustive-search models and compare
with the results their manifests record (from TLC)."""
import json, glob, os, re, subprocess, sys, time, shutil
EX, BIN, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
WORKERS = os.environ.get('WORKERS', '4'); TIMEOUT = float(os.environ.get('TIMEOUT', '300'))
META = OUT + '.meta'
def secs(t):
    try:
        h, m, s = map(int, t.split(':')); return h * 3600 + m * 60 + s
    except ValueError:
        return None
rows = []
for f in sorted(glob.glob(f'{EX}/specifications/*/manifest.json')):
    for mod in json.load(open(f)).get('modules', []):
        for mo in mod.get('models', []):
            if mo.get('mode') != 'exhaustive search':
                continue
            s = secs(mo['runtime'])
            if s is None or s > 600:
                continue
            rows.append((mod, mo))
skip = set()
for f in os.environ.get('SKIPFROM', '').split(','):
    if f:
        skip |= {json.loads(l)['model'] for l in open(f)}
rows = [(m, mo) for m, mo in rows if mo['path'] not in skip]
sh = os.environ.get('SHARD')
if sh:
    k, n = map(int, sh.split('/')); rows = rows[k::n]
only = os.environ.get('ONLY')
with open(OUT, 'w') as out:
    for mod, mo in rows:
        if only and only not in mo['path']:
            continue
        tla = os.path.join(EX, mod['path']); d = os.path.dirname(tla)
        cfg = os.path.relpath(os.path.join(EX, mo['path']), d)
        t0 = time.time()
        try:
            p = subprocess.run([BIN, '-workers', WORKERS, '-metadir', META, '-config', cfg, os.path.basename(tla)],
                               cwd=d, capture_output=True, text=True, timeout=TIMEOUT)
            o, rc = p.stdout + p.stderr, p.returncode
        except subprocess.TimeoutExpired:
            o, rc = 'TIMEOUT', -1
        finally:
            shutil.rmtree(META, ignore_errors=True)
        secs_rs = round(time.time() - t0, 2)
        m = re.search(r'(\d+) distinct states found\. Depth (\d+)', o)
        distinct, depth = (int(m.group(1)), int(m.group(2))) if m else (None, None)
        if rc == -1: got = 'timeout'
        elif rc == 2: got = 'unsupported'
        elif rc == 0: got = 'success'
        elif re.search(r'^Error: Temporal property', o, re.M): got = 'liveness failure'
        elif re.search(r'^Error: (Invariant|Action property|Property|Deadlock)', o, re.M): got = 'safety failure'
        else: got = 'error'
        err = ''
        if rc in (2,) or got == 'error':
            lines = [l for l in o.splitlines() if l.startswith('tlc-rs:') or l.startswith('Error')]
            err = (lines[-1] if lines else o.strip().splitlines()[-1] if o.strip() else '')[:300]
        rec = dict(model=mo['path'], module=mod['path'], features=mod.get('features', []), want=mo['result'],
                   want_distinct=mo.get('distinctStates'), want_depth=mo.get('stateDepth'), tlc_runtime=mo['runtime'],
                   got=got, distinct=distinct, depth=depth, secs=secs_rs, err=err)
        out.write(json.dumps(rec) + '\n'); out.flush()
        print(got, mo['path'], secs_rs, err[:120], flush=True)
