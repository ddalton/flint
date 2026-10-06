#!/usr/bin/env python3
"""2026-10-05 forge merge check on a c8g: every gated ForgeSync world (24, the merged
model with the new rules OFF — must reproduce the 09-28 shipped gate) and every
ForgeSyncCode world (11, the code's combination). One compiled tlc-rs checker per
world (built 40 at a time), each run alone on 192 workers. Expectations:
WORLDS-ForgeSyncShipped.tsv, WORLDS-ForgeSyncCode.tsv."""
import os, re, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor
F = "/opt/flint/formal"; T = f"{F}/tlc-rs/target/release/tlc-rs"; OUT = "/data/out"; DEADLINE = float(sys.argv[1])
os.makedirs(OUT, exist_ok=True)
def log(s):
    with open(f"{OUT}/RESULTS.txt", "a") as f: f.write(s + "\n")
worlds = []
for tsv in ("WORLDS-ForgeSyncShipped.tsv", "WORLDS-ForgeSyncCode.tsv"):
    for l in open(f"{F}/{tsv}"):
        if l.strip() and not l.startswith("#"):
            w, e = l.rstrip("\n").split("\t")[:2]; worlds.append((w, e))
def build(w):
    g, tg = f"/data/gen/{w}", f"/data/gt/{w}"
    r = subprocess.run(f"cd {F} && {T} -codegen {g} -config {w}.cfg ForgeSync.tla && cd {g} && CARGO_TARGET_DIR={tg} cargo build --release", shell=True, capture_output=True, text=True)
    open(f"{OUT}/{w}.build.log", "w").write(r.stdout[-3000:] + r.stderr[-3000:])
    return w, (f"{tg}/release/tlcgen-forgesync" if r.returncode == 0 else None)
with ThreadPoolExecutor(40) as ex: bins = dict(ex.map(lambda we: build(we[0]), worlds))
log(f"built {sum(1 for b in bins.values() if b)} of {len(worlds)} checkers {time.strftime('%FT%TZ', time.gmtime())}")
def judge(exp, out):
    got = re.search(r"Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal propert[^.]* violated|No error has been found|^Error: [^.]*", out, re.M)
    got = got.group(0) if got else ""
    if exp == "RECORD": v = "RECORDED"
    elif exp == "HOLDS": v = "OK-HOLDS" if got == "No error has been found" else "MISMATCH"
    else: v = "OK-FIRES" if re.search(f"(Invariant|property) ({exp}) is violated", got) else "MISMATCH"
    m = re.findall(r"[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+", out)
    pr = [l for l in out.splitlines() if l.startswith("progress:")]
    return v, got, (m[-1] if m else (pr[-1][:120] if pr else ""))
# Mutations and probes first (they stop at a violation), then the holds, smallest first.
order = sorted(worlds, key=lambda we: (we[1] in ("HOLDS", "RECORD"), {"ForgeSyncLive": 0, "ForgeSyncCodeLive": 1, "ForgeSync": 2, "ForgeSyncCode": 3, "ForgeSyncCodeOverlap": 4}.get(we[0], 0)))
for w, exp in order:
    left = DEADLINE - time.time()
    if left < 120: log(f"{w:36} NOT-RUN deadline"); continue
    if not bins.get(w): log(f"{w:36} BUILD-FAILED"); continue
    cap = min(3600 if exp in ("HOLDS", "RECORD") else 1200, left); md = f"/data/st/{w}"; t0 = time.time(); why = ""
    with open(f"{OUT}/{w}.out", "w") as o:
        p = subprocess.Popen([bins[w], "-workers", "192", "-checkpoint", "0", "-metadir", md, "-fpmem", "32000", "-queue-mem", "60000",
                              "-config", f"{w}.cfg", "ForgeSync.tla"], cwd=F, stdout=o, stderr=subprocess.STDOUT)
        try: p.wait(timeout=cap)
        except subprocess.TimeoutExpired: p.kill(); p.wait(); why = "UNDECIDED-CAP"
    subprocess.run(["rm", "-rf", md])
    v, got, size = judge(exp, open(f"{OUT}/{w}.out").read())
    if why: v = why
    log(f"{w:36} {v:14} exp={exp:44} | {got[:56]:56} | {size} | {round(time.time()-t0)}s")
log(f"JOBSDONE {time.strftime('%FT%TZ', time.gmtime())}")
