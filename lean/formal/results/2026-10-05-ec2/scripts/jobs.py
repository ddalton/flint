#!/usr/bin/env python3
"""2026-10-05 spot c8g.48xlarge (user-approved): the LeanP1 RECORD worlds, OPEN3's two
undecided forge worlds, the three-syncer LeanP1 rungs; then, only with time left and
only if TLC scales, TLC on LeanP1AllHolds. One compiled tlc-rs checker per world
(tlc-rs main at the payload's commit). Expectations are the repo's WORLDS files."""
import os, re, subprocess, sys, time
DEADLINE = float(sys.argv[1]); OUT = "/data/out"; ST = "/data/st"; T = "/opt/flint/formal/tlc-rs/target/release/tlc-rs"
LF = "/opt/flint/lean/formal"; FK = "/opt/flint/formal/pending/forge-keptset"
os.makedirs(OUT, exist_ok=True)
def log(s):
    with open(f"{OUT}/RESULTS.txt", "a") as f: f.write(s + "\n")
def build(w, d, mod):
    g, tg = f"/data/gen/{w}", f"/data/gt/{w}"
    r = subprocess.run(f"cd {d} && {T} -codegen {g} -config {w}.cfg {mod} && cd {g} && CARGO_TARGET_DIR={tg} cargo build --release",
                       shell=True, capture_output=True, text=True)
    open(f"{OUT}/{w}.build.log", "w").write(r.stdout[-4000:] + r.stderr[-4000:])
    b = [f"{tg}/release/{x}" for x in os.listdir(f"{tg}/release") if x.startswith("tlcgen-") and not x.endswith(".d")] if r.returncode == 0 else []
    return b[0] if b else None
def judge(exp, out):
    got = re.search(r"Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal propert[^.]* violated|No error has been found|^Error: [^.]*", out, re.M)
    got = got.group(0) if got else ""
    if exp == "RECORD": v = "RECORDED"
    elif exp == "HOLDS": v = "OK-HOLDS" if got == "No error has been found" else "MISMATCH"
    else: v = "OK-FIRES" if re.search(f"(Invariant|property) ({exp}) is violated", got) else "MISMATCH"
    m = re.findall(r"[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+", out)
    pr = [l for l in out.splitlines() if l.startswith("progress:")]
    return v, got, (m[-1] if m else (pr[-1][:120] if pr else ""))
def run(w, d, mod, exp, cap):
    left = DEADLINE - time.time()
    if left < 300: log(f"{w:34} NOT-RUN  deadline"); return
    b = build(w, d, mod)
    if not b: log(f"{w:34} BUILD-FAILED (see {w}.build.log)"); return
    cap = min(cap, DEADLINE - time.time()); md = f"{ST}/{w}"; subprocess.run(["rm", "-rf", md]); t0 = time.time(); why = ""
    with open(f"{OUT}/{w}.out", "w") as o:
        p = subprocess.Popen([b, "-workers", "192", "-checkpoint", "0", "-metadir", md, "-fpmem", "32000", "-queue-mem", "200000",
                              "-config", f"{w}.cfg", mod], cwd=d, stdout=o, stderr=subprocess.STDOUT)
        try: p.wait(timeout=max(1, cap))
        except subprocess.TimeoutExpired: p.kill(); p.wait(); why = "CAPPED"
    subprocess.run(["rm", "-rf", md])
    v, got, size = judge(exp, open(f"{OUT}/{w}.out").read())
    if why: v = "RECORDED-CAP" if exp == "RECORD" else "UNDECIDED-CAP"
    log(f"{w:34} {v:14} exp={exp:40} | {got[:58]:58} | {size} | {round(time.time()-t0)}s cap {round(cap)}s")
def size3(name, wr, mint, ui, bar, rst):
    w = f"LeanP1Size3{name}W{len(wr.split(','))}"
    s = open(f"{LF}/LeanP1Holds1p3b.cfg").read()
    for k, v in (("Writers", "{" + wr + "}"), ("MaxMint", mint), ("MaxUI", ui), ("MaxBarriers", bar), ("MaxRestarts", rst), ("MaxSyncs", 0), ("MaxCopies", 1)):
        s = re.sub(rf"^  {k} = .*$", f"  {k} = {v}", s, flags=re.M)
    open(f"{LF}/{w}.cfg", "w").write(s); return w
log(f"start {time.strftime('%FT%TZ', time.gmtime())}; deadline {time.strftime('%FT%TZ', time.gmtime(DEADLINE))}; LeanP1.tla md5 "
    + subprocess.run(f"md5sum {LF}/LeanP1.tla", shell=True, capture_output=True, text=True).stdout[:8]
    + "; ForgeSyncKeptSet.tla md5 " + subprocess.run(f"md5sum {FK}/ForgeSyncKeptSet.tla", shell=True, capture_output=True, text=True).stdout[:8])
# 1. The RECORD worlds (no expected result; recorded as they come out).
run("LeanP1CollectorGreedy", LF, "LeanP1.tla", "RECORD", 7200)
run("LeanP1NoConvergence", LF, "LeanP1.tla", "RECORD", 7200)
# 2. OPEN3's undecided forge worlds (WORLDS.tsv: both must fire).
run("ForgeSyncKeptSetProbeCovered", FK, "ForgeSyncKeptSet.tla", "ProbeKeptSetDropsCovered", 10800)
run("ForgeSyncKeptSetVsOriginal", FK, "ForgeSyncKeptSet.tla", "Inv_(AckedIsDurable|LandedPackComplete)", 10800)
# 3. Three syncers (Holds1p3b's invariants), rungs L3 and L4 of the Mac ladder; L4's two-syncer twin for the ratio.
run(size3("L3", "A, B, C", 3, 1, 2, 0), LF, "LeanP1.tla", "HOLDS", 5400)
run(size3("L4", "A, B", 3, 1, 3, 0), LF, "LeanP1.tla", "HOLDS", 1800)
run(size3("L4", "A, B, C", 3, 1, 3, 0), LF, "LeanP1.tla", "HOLDS", 7200)
log(f"JOBSDONE {time.strftime('%FT%TZ', time.gmtime())}")
