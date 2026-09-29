#!/usr/bin/env python3
"""Run every gate entry through tlc-rs; compare the verdict with the gate's expectation."""
import os, re, subprocess, sys, time, json, shlex

ROOT = os.environ.get("FLINT_ROOT", "/Users/ddalton/github/flint")
BIN = sys.argv[1] if len(sys.argv) > 1 else ROOT + "/formal/tlc-rs/target/release/tlc-rs"
TIMEOUT = float(sys.argv[2]) if len(sys.argv) > 2 else 15
OUT = sys.argv[3] if len(sys.argv) > 3 else "sweep.jsonl"

entries = []
for script, d in [("scripts/check-tla.sh", "formal"), ("lean/formal/check.sh", "lean/formal")]:
    # module variables (M=..., M2=..., C=...), substituted by whole name
    V = {}
    # join `\` continuation lines, so an entry split over lines is one entry
    for line in open(f"{ROOT}/{script}").read().replace("\\\n", " ").splitlines():
        m = re.match(r"\s*([A-Z][A-Za-z0-9_]*)=(\w+)\s*$", line)
        if m:
            V[m.group(1)] = m.group(2)
        m = re.match(r"\s*(strict_run|mutation_run|liveness_mutation_run)\s+(.*)", line)
        if not m:
            continue
        try:
            args = shlex.split(re.sub(r"\$(\w+)", lambda v: V.get(v.group(1), v.group(0)), m.group(2)))
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
            # a run killed at the timeout leaves its spilled queue behind:
            # give each run its own metadir and always remove it
            meta = f"{ROOT}/formal/tlc-rs/gen/sweep-meta"
            p = subprocess.run([BIN, "-workers", "8", "-metadir", meta, "-config", cfg, mod + ".tla"], cwd=f"{ROOT}/{d}",
                               capture_output=True, text=True, timeout=TIMEOUT)
            out, rc = p.stdout + p.stderr, p.returncode
        except subprocess.TimeoutExpired:
            out, rc = "TIMEOUT", -1
        finally:
            import shutil
            shutil.rmtree(f"{ROOT}/formal/tlc-rs/gen/sweep-meta", ignore_errors=True)
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
            # formal/ gives the invariant's name (a regex); lean/formal gives a
            # substring of the output ("Invariant X is violated")
            ok = rc == 12 and viol and (want is None or re.search(want, viol.group(1)) or want in out)
            status = "agree" if ok else "DISAGREE"
        err = "" if rc in (0, 12, -1) else out.strip().splitlines()[-1][:200]
        if rc == 12 and not viol:
            err = [l for l in out.splitlines() if l.startswith("Error")][:1]
        rec = dict(dir=d, kind=kind, module=mod, cfg=cfg, want=want, status=status, secs=round(secs, 3),
                   distinct=distinct, violated=viol.group(1) if viol else None, err=err)
        f.write(json.dumps(rec) + "\n")
        f.flush()
        print(status, d, cfg, round(secs, 2), err, flush=True)
