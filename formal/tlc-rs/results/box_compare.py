#!/usr/bin/env python3
"""Correctness comparison, tlc-rs vs TLC, gentle enough to share a busy box.

For each gate entry: run tlc-rs (1 worker, niced, capped); only if it
finishes, run TLC the same way (1 worker, niced, small heap, capped) with a
throwaway -metadir that is deleted afterwards. Records verdicts and
distinct-state counts; timings are NOT benchmarks on a shared machine.

  box_compare.py ROOT ENTRIES.jsonl OUT.jsonl [rs_timeout] [tlc_timeout]
"""
import json, os, re, shutil, subprocess, sys, time

ROOT, SRC, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
RS_T = float(sys.argv[4]) if len(sys.argv) > 4 else 150
TLC_T = float(sys.argv[5]) if len(sys.argv) > 5 else 600
BIN = f"{ROOT}/formal/tlc-rs/target/release/tlc-rs"
JAR = f"{ROOT}/.tla2tools.jar"
META = f"{ROOT}/tlcmeta"


def run(cmd, cwd, timeout):
    t0 = time.time()
    try:
        p = subprocess.run(["nice", "-n", "15"] + cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout)
        return p.stdout + p.stderr, p.returncode, time.time() - t0
    except subprocess.TimeoutExpired:
        return "TIMEOUT", -1, time.time() - t0


def parse(out):
    m = re.findall(r"(\d+) distinct states found", out)
    v = re.search(r"Invariant (\S+) is violated", out)
    return (int(m[-1]) if m else None), (v.group(1) if v else None)


done = set()
if os.path.exists(OUT):
    done = {json.loads(l)["cfg"] for l in open(OUT)}
with open(OUT, "a") as f:
    for line in open(SRC):
        r = json.loads(line)
        if r["cfg"] in done:
            continue
        cwd = f"{ROOT}/{r['dir']}"
        out, rc, secs = run([BIN, "-workers", "1", "-config", r["cfg"], r["module"] + ".tla"], cwd, RS_T)
        r["rs_rc"], r["rs_secs"] = rc, round(secs, 2)
        r["rs_distinct"], r["rs_violated"] = parse(out)
        r["rs_err"] = "" if rc in (0, 12, -1) else out.strip().splitlines()[-1][:200]
        if rc in (0, 12):
            meta = f"{META}/{r['cfg']}"
            out, rc2, secs2 = run(["java", "-Xmx4g", "-XX:+UseParallelGC", "-cp", JAR, "tlc2.TLC", "-workers", "1",
                                   "-metadir", meta, "-config", r["cfg"], r["module"] + ".tla"], cwd, TLC_T)
            shutil.rmtree(meta, ignore_errors=True)
            r["tlc_rc"], r["tlc_secs"] = rc2, round(secs2, 2)
            r["tlc_distinct"], r["tlc_violated"] = parse(out)
        f.write(json.dumps(r) + "\n")
        f.flush()
        print(r["cfg"], r["rs_rc"], r.get("rs_distinct"), r.get("tlc_rc"), r.get("tlc_distinct"), flush=True)
