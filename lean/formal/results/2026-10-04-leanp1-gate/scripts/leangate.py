#!/usr/bin/env python3
"""LeanP1 gate on a spot c8g: every world of WORLDS-LeanP1.tsv except
LeanP1Holds (done 2026-10-03, 06bd01cf) and LeanP1LiveHolds (replaced by
LiveHoldsSmall). Compiled checkers (tlc-rs main 129adc8b). Phase 1: all at
once, 4 workers, short cap. Phase 2: the rest one at a time on all cores,
until the deadline. Judged as run-gate.sh judges."""
import json, os, re, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor

F = "/opt/flint/lean/formal"; OUT = "/data/out"; ST = "/data/st"
DEADLINE = float(sys.argv[1]); P1CAP = int(sys.argv[2])
MC = {"LeanP1AllHolds", "LeanP1ProbeReaderRescoped", "LeanP1ProbeReaderPulledMidRescope"}
worlds = [l.rstrip("\n").split("\t") for l in open(f"{F}/WORLDS-LeanP1.tsv") if l.strip()]
worlds = [(w, e) for w, e in worlds if w not in ("LeanP1Holds", "LeanP1LiveHolds")]
os.makedirs(OUT, exist_ok=True)

def log(s):
    with open(f"{OUT}/RESULTS.txt", "a") as f:
        f.write(s + "\n")

def binary(w):
    return f"/data/gt/{w}/release/" + ("tlcgen-mcleanp1all" if w in MC else "tlcgen-leanp1")

def judge(w, exp, out, why):
    got = re.search(r"Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal propert[^.]* violated|No error has been found|^Error: [^.]*", out, re.M)
    got = got.group(0) if got else ""
    if why: v = why
    elif exp == "RECORD": v = "RECORDED"
    elif exp == "HOLDS" and got == "No error has been found": v = "OK-HOLDS"
    elif exp == "Temporal" and "Temporal propert" in got: v = "OK-FIRES"
    elif exp != "HOLDS" and exp in got: v = "OK-FIRES"
    else: v = "MISMATCH"
    m = re.findall(r"[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+", out)
    pr = [l for l in out.splitlines() if l.startswith("progress:")]
    return v, got, (m[-1] if m else (pr[-1][:110] if pr else ""))

def run(w, exp, workers, cap, extra):
    mod = "MCLeanP1All.tla" if w in MC else "LeanP1.tla"
    md = f"{ST}/{w}"; subprocess.run(["rm", "-rf", md])
    t0 = time.time(); why = ""
    with open(f"{OUT}/{w}.out", "w") as o:
        p = subprocess.Popen([binary(w), "-workers", str(workers), "-checkpoint", "0", "-metadir", md] + extra + ["-config", f"{w}.cfg", mod], cwd=F, stdout=o, stderr=subprocess.STDOUT)
        try:
            p.wait(timeout=max(1, cap))
        except subprocess.TimeoutExpired:
            p.kill(); p.wait(); why = "CAPPED"
    subprocess.run(["rm", "-rf", md])
    out = open(f"{OUT}/{w}.out").read()
    v, got, size = judge(w, exp, out, why)
    return dict(w=w, exp=exp, v=v, got=got, size=size, secs=round(time.time() - t0), workers=workers, rc=p.returncode)

def line(r, phase):
    return f"{r['w']:36} {r['v']:12} exp={r['exp']:26} | {r['got'][:60]:60} | {r['size']} | {r['secs']}s w{r['workers']} {phase}"

log(f"start {time.strftime('%FT%TZ', time.gmtime())} module 94da7541 tlc-rs main 129adc8b compiled, c8g.48xlarge; deadline {time.strftime('%TZ', time.gmtime(DEADLINE))}")
missing = [w for w, _ in worlds if not os.path.exists(binary(w))]
for w in missing:
    log(f"{w:36} BUILD-FAILED")
todo = [(w, e) for w, e in worlds if w not in missing]

# Phase 1: everything at once, small.
small = ["-fpmem", "1500", "-queue-mem", "3000"]
with ThreadPoolExecutor(len(todo)) as ex:
    rs = list(ex.map(lambda we: run(we[0], we[1], 4, P1CAP, small), todo))
later = []
for r in rs:
    if r["v"] == "CAPPED":
        later.append((r["w"], r["exp"]))
        log(f"{r['w']:36} -> phase 2 (not done in {P1CAP}s at 4 workers: {r['size']})")
    else:
        log(line(r, "p1"))

# Phase 2: one at a time on all cores. Mutations and probes first (they
# stop at a violation), then the claims smallest-known first, RECORD last.
rank = {"LeanP1Holds1p3b": 1, "LeanP1AllHolds": 2, "LeanP1DeleteOverrideOff": 3}
later.sort(key=lambda we: (we[1] == "RECORD", we[1] == "HOLDS", rank.get(we[0], 0)))
big = ["-fpmem", "32000", "-queue-mem", "200000"]
for w, e in later:
    left = DEADLINE - time.time()
    if left < 30:
        log(f"{w:36} NOT-RUN      exp={e:26} | deadline reached"); continue
    r = run(w, e, 192, left, big)
    if r["v"] == "CAPPED":
        r["v"] = "RECORDED-CAP" if e == "RECORD" else "UNDECIDED-DEADLINE"
    log(line(r, "p2"))
log(f"GATEDONE {time.strftime('%FT%TZ', time.gmtime())}")
