#!/usr/bin/env python3
"""Run every gate entry through tlc-rs; compare the verdict with the gate's expectation."""
import re, subprocess, sys, time, json, shlex

ROOT = "/Users/ddalton/github/flint"
BIN = sys.argv[1] if len(sys.argv) > 1 else ROOT + "/formal/tlc-rs/target/release/tlc-rs"
TIMEOUT = float(sys.argv[2]) if len(sys.argv) > 2 else 15
OUT = sys.argv[3] if len(sys.argv) > 3 else "sweep.jsonl"

entries = []
for script, d in [("scripts/check-tla.sh", "formal"), ("lean/formal/check.sh", "lean/formal")]:
    M = None
    for line in open(f"{ROOT}/{script}"):
        m = re.match(r"\s*M=(\S+)", line)
        if m:
            M = m.group(1)
        m = re.match(r"\s*(strict_run|mutation_run|liveness_mutation_run)\s+(.*)", line)
        if not m:
            continue
        try:
            args = shlex.split(m.group(2).replace("$M", M or "$M"))
        except ValueError:
            continue
        kind = m.group(1)
        mod, cfg = args[0], args[1]
        want = args[3] if kind == "mutation_run" and len(args) > 3 else None
        entries.append((d, kind, mod, cfg, want))

import os
only = set(open(os.environ['ONLY']).read().split()) if os.environ.get('ONLY') else None
with open(OUT, "w") as f:
    for d, kind, mod, cfg, want in entries:
        if only is not None and cfg not in only:
            continue
        t0 = time.time()
        try:
            p = subprocess.run([BIN, "-workers", "8", "-config", cfg, mod + ".tla"], cwd=f"{ROOT}/{d}",
                               capture_output=True, text=True, timeout=TIMEOUT)
            out, rc = p.stdout + p.stderr, p.returncode
        except subprocess.TimeoutExpired:
            out, rc = "TIMEOUT", -1
        secs = time.time() - t0
        m = re.search(r"(\d+) distinct states found", out)
        distinct = int(m.group(1)) if m else None
        viol = re.search(r"(?:Invariant|Temporal property|Action property|Property) (\S+)(?: \(for [^)]*\))? is violated", out)
        if rc == -1:
            status = "timeout"
        elif rc == 2:
            status = "unsupported"
        elif kind == "liveness_mutation_run":
            status = "agree" if rc == 12 and viol else "DISAGREE"
        elif kind == "strict_run":
            status = "agree" if rc == 0 else "DISAGREE"
        else:
            ok = rc == 12 and viol and (want is None or re.search(want, viol.group(1)))
            status = "agree" if ok else "DISAGREE"
        err = "" if rc in (0, 12, -1) else out.strip().splitlines()[-1][:200]
        if rc == 12 and not viol:
            err = [l for l in out.splitlines() if l.startswith("Error")][:1]
        rec = dict(dir=d, kind=kind, module=mod, cfg=cfg, want=want, status=status, secs=round(secs, 3),
                   distinct=distinct, violated=viol.group(1) if viol else None, err=err)
        f.write(json.dumps(rec) + "\n")
        f.flush()
        print(status, d, cfg, round(secs, 2), err, flush=True)
