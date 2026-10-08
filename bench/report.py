#!/usr/bin/env python3
"""Summarize run-fio.sh results: one or more result directories in, a
markdown table (stdout) and a long-form CSV out.

    ./report.py results/flint-r3 results/mayastor-r3 --csv summary.csv

Every number is the MEDIAN across repetitions with the [min-max] range
beside it (plan §5: a difference inside overlapping ranges is "no
difference"). Storage CPU is the sum, over all nodes, of the cores used by
the processes the run's CPU_PATTERNS matched, measured inside fio's
window; IOPS/core divides total IOPS by it.

A rep whose fio.json is empty or unparseable (a test still in flight) is
left out. A rep where any node's CPU sample is empty or truncated keeps
its fio numbers but drops its CPU figures: storage cores is a sum over
nodes, so a partial sum would undercount. Both are warned on stderr, and a
cell built from fewer reps than the row says is marked "(n/m reps)".
"""
import argparse
import csv
import json
import statistics
import sys
from pathlib import Path


def load_env(d):
    env = {}
    for line in (d / "env.yaml").read_text().splitlines():
        k, _, v = line.partition(": ")
        env[k] = v
    return env


def fio_metrics(path):
    job = json.loads(path.read_text())["jobs"][0]
    m = {}
    tot_iops = 0.0
    tot_bw = 0.0
    for side in ("read", "write"):
        s = job[side]
        if s["total_ios"] == 0:
            continue
        tot_iops += s["iops"]
        tot_bw += s["bw_bytes"] / 2**20
        pct = s["clat_ns"].get("percentile", {})
        for key, label in (("50.000000", "p50"), ("99.000000", "p99"), ("99.900000", "p99.9")):
            if key in pct:
                m[f"{side}_{label}_us"] = pct[key] / 1000
    m["iops"] = tot_iops
    m["mib_s"] = tot_bw
    return m


def warn(msg):
    sys.stderr.write(f"report.py: WARNING: {msg}\n")


def cpu_metrics(rep_dir):
    """The rep's CPU figures, or None when any node's sample is missing,
    empty or truncated (a partial sum over nodes would undercount)."""
    storage = 0.0
    busy = 0.0
    n = 0
    for f in sorted(rep_dir.glob("cpu-*.json")):
        text = f.read_text().strip()
        try:
            o = json.loads(text)
            b = float(o["node_busy_cores"])
            c = sum(float(p["cores"]) for p in o["procs"].values())
        except (ValueError, KeyError, TypeError, AttributeError) as e:
            why = "empty" if not text else f"truncated or malformed ({e.__class__.__name__})"
            warn(f"{f}: {why} sampler output (see {f.with_suffix('.err').name}); CPU figures for {rep_dir} dropped")
            return None
        busy += b
        storage += c
        n += 1
    if n == 0:
        warn(f"{rep_dir}: no sampler output; CPU figures dropped")
        return None
    out = {"storage_cores": storage, "node_busy_cores": busy}
    # Reactor busy share (REACTOR_TICKS): "<busy0> <idle0> <n> <busy1> <idle1> <n>"
    # per node; report the busiest node's reactors, the one that would bind.
    shares = []
    for f in sorted(rep_dir.glob("reactor-*.txt")):
        v = f.read_text().split()
        if len(v) == 6:
            db, di = int(v[3]) - int(v[0]), int(v[4]) - int(v[1])
            if db + di > 0:
                shares.append(100.0 * db / (db + di))
    if shares:
        out["reactor_busy_max_pct"] = max(shares)
    return out


def label(d, env):
    """The driver label, plus the result dir when it says something more
    (an experiment reuses one volume, so one driver label, across arms)."""
    drv = env.get("driver") or "?"
    return drv if d.name.endswith(drv) else f"{drv} ({d.name})"


def collect(d):
    env = load_env(d)
    rows = {}
    for test_dir in sorted(p for p in d.iterdir() if p.is_dir() and p.name != "idle"):
        reps = []
        for rep in sorted(test_dir.glob("rep*")):
            try:
                m = fio_metrics(rep / "fio.json")
            except (OSError, ValueError, KeyError, IndexError) as e:
                warn(f"{rep}: no usable fio.json ({e.__class__.__name__}); rep left out")
                continue
            c = cpu_metrics(rep)
            if c is not None:
                m.update(c)
                m["iops_per_core"] = m["iops"] / m["storage_cores"] if m["storage_cores"] > 0 else float("nan")
            reps.append(m)
        if reps:
            rows[test_dir.name] = reps
    return env, rows


def summarize(reps, key):
    vals = [r[key] for r in reps if key in r]
    if not vals:
        return None
    return statistics.median(vals), min(vals), max(vals), len(vals)


def fmt(s, digits=0):
    if s is None:
        return "–"
    med, lo, hi, _ = s
    f = f"{{:,.{digits}f}}"
    return f"{f.format(med)} [{f.format(lo)}–{f.format(hi)}]"


COLUMNS = [
    ("iops", "IOPS", 0),
    ("mib_s", "MiB/s", 0),
    ("read_p50_us", "read p50 µs", 0),
    ("read_p99_us", "read p99 µs", 0),
    ("read_p99.9_us", "read p99.9 µs", 0),
    ("write_p50_us", "write p50 µs", 0),
    ("write_p99_us", "write p99 µs", 0),
    ("write_p99.9_us", "write p99.9 µs", 0),
    ("storage_cores", "storage cores", 2),
    ("reactor_busy_max_pct", "reactor busy % (max node)", 0),
    ("iops_per_core", "IOPS/core", 0),
]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("dirs", nargs="+", type=Path)
    ap.add_argument("--csv", type=Path)
    a = ap.parse_args()

    runs = [(d, *collect(d)) for d in a.dirs]
    tests = []
    for _, _, rows in runs:
        tests += [t for t in rows if t not in tests]

    long_rows = []
    out = sys.stdout
    out.write("\n### idle (no I/O)\n\n| driver | SC | storage cores | node busy cores (sum) |\n|---|---|---|---|\n")
    for d, env, _ in runs:
        if (d / "idle").is_dir():
            c = cpu_metrics(d / "idle")
            if c is None:
                out.write(f"| {label(d, env)} | {env.get('storageclass')} | – | – |\n")
            else:
                out.write(f"| {label(d, env)} | {env.get('storageclass')} | {c['storage_cores']:.2f} | {c['node_busy_cores']:.2f} |\n")
    for t in tests:
        out.write(f"\n### {t}\n\n")
        present = [(k, h, dg) for k, h, dg in COLUMNS
                   if any(t in rows and summarize(rows[t], k) for _, _, rows in runs)]
        out.write("| driver | SC | reps | " + " | ".join(h for _, h, _ in present) + " |\n")
        out.write("|---|---|---|" + "---|" * len(present) + "\n")
        for d, env, rows in runs:
            if t not in rows:
                continue
            reps = rows[t]
            cells = []
            for k, _, dg in present:
                s = summarize(reps, k)
                cell = fmt(s, dg)
                if s and s[3] < len(reps):
                    cell += f" ({s[3]}/{len(reps)} reps)"
                cells.append(cell)
                if s:
                    long_rows.append({"driver": env.get("driver"), "storageclass": env.get("storageclass"),
                                      "test": t, "metric": k, "median": s[0], "min": s[1], "max": s[2],
                                      "reps": s[3], "dir": str(d)})
            out.write(f"| {label(d, env)} | {env.get('storageclass')} | {len(reps)} | " + " | ".join(cells) + " |\n")

    if a.csv:
        with a.csv.open("w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=list(long_rows[0].keys()) if long_rows else ["driver"])
            w.writeheader()
            w.writerows(long_rows)


if __name__ == "__main__":
    main()
