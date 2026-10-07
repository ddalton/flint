#!/usr/bin/env python3
"""The tla-mc gate: every 'agree' entry of the gate sweep, through tla-mc's
interpreter AND a checker it generates and builds, in parallel.
Pass = same verdict as the gate's expectation, and (for runs that hold)
the same distinct count from both engines (and TLC's, where recorded).

  gate_par.py ROOT GATE_SWEEP.jsonl LIVENESS_TLC.jsonl[,..] OUT.jsonl JOBS
"""
import json, re, subprocess, sys, time, os, shutil
from concurrent.futures import ThreadPoolExecutor, as_completed

ROOT, SRC, TLC, OUT, JOBS = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
# tla-mc as installed (cargo install tla-mc --version X --locked): $TLAMC, else on PATH
BIN = os.environ.get("TLAMC") or shutil.which("tla-mc") or sys.exit("no tla-mc: cargo install tla-mc")
WORK = os.environ.get("GATE_WORK", "/data/gate-work")
VIOL = re.compile(r"(?:Invariant|Temporal property|Action property|Property) (\S+)(?: \(for [^)]*\))? is violated")


def run(cmd, cwd, timeout, env=None):
    t0 = time.time()
    try:
        p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout, env=env)
        return p.stdout + p.stderr, p.returncode, time.time() - t0
    except subprocess.TimeoutExpired:
        return "TIMEOUT", -1, time.time() - t0


def parse(out, rc, s):
    m = re.findall(r"(\d+) distinct states found", out)
    v = VIOL.search(out)
    return dict(rc=rc, secs=round(s, 2), distinct=int(m[-1]) if m else None,
                violated=v.group(1) if v else None,
                timeout=out == "TIMEOUT", tail=out.strip().splitlines()[-1][:160] if out.strip() else "")


# the first file that has an entry wins: pass today's tlc_side.py output first, recorded
# counts after it (2026-10-07: a 2026-09-26 count of an older ForgeSyncLive overrode
# today's and showed as a difference)
tlc = {}
for fn in TLC.split(","):
    for line in open(fn):
        r = json.loads(line)
        tlc.setdefault((r["dir"], r["cfg"]), r)


def one(i, r):
    cwd = f"{ROOT}/{r['dir']}"
    if not os.path.exists(f"{cwd}/{r['cfg']}"):
        r["skip"] = "cfg gone"
        return r
    base = ["-workers", os.environ.get("GATE_WORKERS", "4"), "-config", r["cfg"], r["module"] + ".tla"]
    r["interp"] = parse(*run([BIN, "-engine", "interp", "-metadir", f"{WORK}/md-i{i}"] + base, cwd, 1200))
    d = f"{WORK}/gen{i}"
    tgt = f"{WORK}/tgt{i}"
    out, rc, s = run([BIN, "-codegen", d, "-config", r["cfg"], r["module"] + ".tla"], cwd, 300)
    g = dict(codegen_rc=rc)
    if rc == 0:
        out, rc, s = run(["cargo", "build", "--release"], d, 1200, env=dict(os.environ, CARGO_TARGET_DIR=tgt))
        g["build_rc"], g["build_secs"] = rc, round(s, 1)
        if rc != 0:
            g["build_err"] = [l for l in out.splitlines() if l.startswith("error")][:3]
        else:
            pkg = "tlcgen-" + re.sub(r"[^a-z0-9]", "-", r["module"].lower())
            g.update(parse(*run([f"{tgt}/release/{pkg}", "-metadir", f"{WORK}/md-g{i}"] + base, cwd, 1200)))
            g["step_props_compiled"] = len(re.findall(r"fn aprop_\d+", open(f"{d}/src/main.rs").read())) if os.path.exists(f"{d}/src/main.rs") else None
    else:
        g["codegen_err"] = out.strip().splitlines()[-1][:200] if out.strip() else ""
    r["gen"] = g
    for p in (d, tgt, f"{WORK}/md-i{i}", f"{WORK}/md-g{i}"):
        shutil.rmtree(p, ignore_errors=True)
    # Verdicts, by sweep.py's rules: strict_run holds; mutation_run violates
    # an invariant matching want; liveness_mutation_run violates (the gate's
    # recorded name). Both engines must give the same verdict.
    want, kind = r.get("want"), r["kind"]
    t = tlc.get((r["dir"], r["cfg"]))
    probs = []
    for k in ("interp", "gen"):
        e = r.get(k, {})
        v = e.get("violated")
        if e.get("timeout"):
            probs.append(f"{k} timeout"); continue
        if e.get("distinct") is None and v is None:
            probs.append(f"{k} no result"); continue
        if kind == "strict_run" and v is not None:
            probs.append(f"{k} violated {v} (must hold)")
        if kind == "mutation_run" and not (v and (want is None or re.search(want, v))):
            probs.append(f"{k} verdict {v} want {want}")
        if kind == "liveness_mutation_run" and v != r.get("violated"):
            probs.append(f"{k} verdict {v} gate recorded {r.get('violated')}")
    if r["interp"].get("violated") != r["gen"].get("violated"):
        probs.append(f"engines differ: interp {r['interp'].get('violated')} gen {r['gen'].get('violated')}")
    if kind == "strict_run" and not probs:
        if r["interp"]["distinct"] != r["gen"].get("distinct"):
            probs.append(f"distinct interp {r['interp']['distinct']} gen {r['gen'].get('distinct')}")
        if t and t.get("tlc_distinct") and not t.get("tlc_violated") and t["tlc_distinct"] != r["gen"].get("distinct"):
            probs.append(f"distinct TLC {t['tlc_distinct']} gen {r['gen'].get('distinct')}")
    if t:
        r["tlc"] = dict(distinct=t.get("tlc_distinct"), violated=t.get("tlc_violated"))
    r["result"] = "OK" if not probs else "DIFF: " + "; ".join(probs)
    return r


rows = [json.loads(l) for l in open(SRC)]
rows = [r for r in rows if r["status"] == "agree"]
os.makedirs(WORK, exist_ok=True)
t0 = time.time()
with open(OUT, "w") as f, ThreadPoolExecutor(JOBS) as ex:
    futs = {ex.submit(one, i, r): r for i, r in enumerate(rows)}
    n = 0
    for fu in as_completed(futs):
        try:
            r = fu.result()
        except Exception as e:
            r = dict(futs[fu], result=f"DIFF: harness {e!r}")
        n += 1
        f.write(json.dumps(r) + "\n"); f.flush()
        print(f"[{n}/{len(rows)} {time.time()-t0:.0f}s] {r['dir']}/{r['cfg']} {r.get('skip') or r['result']}", flush=True)
