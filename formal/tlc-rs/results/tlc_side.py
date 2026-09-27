#!/usr/bin/env python3
"""For each entry tlc-rs decided, run TLC the way the gate does and record time + distinct states."""
import json, re, subprocess, sys, time, os

ROOT = "/Users/ddalton/github/flint"
JAR = ROOT + "/.tla2tools.jar"
SRC, OUT = sys.argv[1], sys.argv[2]
TIMEOUT = float(sys.argv[3]) if len(sys.argv) > 3 else 300
META = os.path.dirname(os.path.abspath(OUT)) + "/tlcmeta"

rows = [json.loads(l) for l in open(SRC)]
with open(OUT, "w") as f:
    for r in rows:
        if r["status"] not in ("agree", "DISAGREE"):
            continue
        cmd = ["java", "-XX:+UseParallelGC", "-cp", JAR, "tlc2.TLC", "-workers", "auto",
               "-metadir", f"{META}/{r['cfg']}", "-config", r["cfg"], r["module"] + ".tla"]
        t0 = time.time()
        try:
            p = subprocess.run(cmd, cwd=f"{ROOT}/{r['dir']}", capture_output=True, text=True, timeout=TIMEOUT)
            out, rc = p.stdout, p.returncode
        except subprocess.TimeoutExpired:
            out, rc = "TIMEOUT", -1
        secs = time.time() - t0
        m = re.findall(r"(\d+) distinct states found", out)
        viol = re.search(r"Invariant (\S+) is violated", out)
        r.update(tlc_secs=round(secs, 3), tlc_rc=rc, tlc_distinct=int(m[-1]) if m else None,
                 tlc_violated=viol.group(1) if viol else None)
        f.write(json.dumps(r) + "\n")
        f.flush()
        print(r["cfg"], r["secs"], r["tlc_secs"], r["distinct"], r["tlc_distinct"], flush=True)
