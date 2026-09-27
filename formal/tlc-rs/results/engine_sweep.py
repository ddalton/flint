#!/usr/bin/env python3
"""Every decidable gate entry through the closure engine and a generated
checker; compare verdict and distinct count with the interpreter's run.

  engine_sweep.py ROOT GATE_SWEEP.jsonl OUT.jsonl
"""
import json, re, subprocess, sys, time, os

ROOT, SRC, OUT = sys.argv[1], sys.argv[2], sys.argv[3]
CRATE = f"{ROOT}/formal/tlc-rs"
BIN = f"{CRATE}/target/release/tlc-rs"
GEN = f"{CRATE}/gen/sweep"
TARGET = f"{CRATE}/gen/target"


def run(cmd, cwd, timeout=120, env=None):
    t0 = time.time()
    try:
        p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout, env=env)
        return p.stdout + p.stderr, p.returncode, time.time() - t0
    except subprocess.TimeoutExpired:
        return "TIMEOUT", -1, time.time() - t0


def parse(out):
    m = re.findall(r"(\d+) distinct states found", out)
    v = re.search(r"Invariant (\S+) is violated", out)
    return (int(m[-1]) if m else None), (v.group(1) if v else None)


env = dict(os.environ, CARGO_TARGET_DIR=TARGET)
with open(OUT, "w") as f:
    for line in open(SRC):
        r = json.loads(line)
        if r["status"] != "agree":
            continue
        cwd = f"{ROOT}/{r['dir']}"
        base = ["-workers", "8", "-config", r["cfg"], r["module"] + ".tla"]
        out, rc, s = run([BIN, "-engine", "interp"] + base, cwd)
        r["interp"] = dict(rc=rc, secs=round(s, 3), distinct=parse(out)[0], violated=parse(out)[1])
        out, rc, s = run([BIN, "-engine", "closure"] + base, cwd)
        r["closure"] = dict(rc=rc, secs=round(s, 3), distinct=parse(out)[0], violated=parse(out)[1])
        d = f"{GEN}/{r['cfg'][:-4]}"
        out, rc, s = run([BIN, "-codegen", d, "-config", r["cfg"], r["module"] + ".tla"], cwd)
        gen = dict(codegen_rc=rc)
        if rc == 0:
            out, rc, s = run(["cargo", "build", "--release", "--offline"], d, timeout=600, env=env)
            gen["build_rc"], gen["build_secs"] = rc, round(s, 1)
            if rc != 0:
                gen["build_err"] = [l for l in out.splitlines() if l.startswith("error")][:3]
            else:
                pkg = "tlcgen-" + re.sub(r"[^a-z0-9]", "-", r["module"].lower())
                out, rc, s = run([f"{TARGET}/release/{pkg}"] + base, cwd)
                gen.update(rc=rc, secs=round(s, 3), distinct=parse(out)[0], violated=parse(out)[1])
        else:
            gen["codegen_err"] = out.strip().splitlines()[-1][:200]
        r["gen"] = gen
        f.write(json.dumps(r) + "\n")
        f.flush()
        print(r["cfg"], r["interp"]["distinct"], r["closure"]["distinct"], gen.get("distinct"), gen.get("build_rc"), flush=True)
