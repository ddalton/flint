#!/usr/bin/env python3
"""tables.py <read.tsv> [write.tsv] — the five tables for door-drill-2026-09-12.md.

Every cell is a RANGE over reps (min-max in seconds), never a mean. A
one-rep arm prints as a single figure. The 2026-09-10 figures for the
delta table are copied from door-drill-2026-09-10.md verbatim.
"""
import sys
from collections import defaultdict

def load(path):
    rows = defaultdict(list)  # (w, arm) -> [(rep, wall, fetch, bytes, files, ranged)]
    if not path:
        return rows
    for line in open(path):
        f = line.rstrip("\n").split("\t")
        if len(f) != 8 or f[3] == "FAIL":
            continue
        rows[(f[1], f[2])].append(f)
    return rows

def rng(rows, w, arm, col=3, scale=1000.0, fmt="{:.2f}"):
    r = rows.get((w, arm))
    if not r:
        return "—"
    vals = [float(x[col]) / scale for x in r if x[col] != "-"]
    if not vals:
        return "—"
    lo, hi = min(vals), max(vals)
    if len(vals) == 1:
        return fmt.format(lo) + " s (n=1)"
    if len(vals) < 3:
        return fmt.format(lo) + "-" + fmt.format(hi) + f" s (n={len(vals)})"
    return fmt.format(lo) + "-" + fmt.format(hi) + " s"

def bounds(rows, w, arm, col=3):
    r = rows.get((w, arm))
    if not r:
        return None
    vals = [float(x[col]) / 1000.0 for x in r if x[col] != "-"]
    return (min(vals), max(vals)) if vals else None

def ratio(old, new):
    """old vs new as a bracket: old_min/new_max .. old_max/new_min. A bracket
    that straddles 1 is 'no effect the reps can resolve'."""
    if not old or not new:
        return "—"
    lo, hi = old[0] / new[1], old[1] / new[0]
    if lo < 1 < hi:
        return f"{lo:.2f}-{hi:.2f}x (straddles 1: unresolved)"
    if hi < 1:
        return f"{1/hi:.2f}-{1/lo:.2f}x SLOWER"
    return f"{lo:.2f}-{hi:.2f}x faster"

W = ["big", "small", "mixed"]
read = load(sys.argv[1])
write = load(sys.argv[2]) if len(sys.argv) > 2 else {}
out = {}

# 1. read
t = ["| workload | lean shipped (wall) | lean shipped (fetch only) | passthrough no-cache | passthrough `--cache`, cold | `aws s3 cp` 32-way |",
     "|---|---|---|---|---|---|"]
for w in W:
    t.append(f"| `{w}` | {rng(read,w,'L-ship')} | {rng(read,w,'L-ship',4)} | {rng(read,w,'P-32')} | {rng(read,w,'PC-32')} | {rng(read,w,'S-32')} |")
t += ["", "The lean variants, wall (fetch):", "",
      "| workload | `L-ship` | `L-raw` | `L-0910` | `L-slow` (rep 1) |", "|---|---|---|---|---|"]
for w in W:
    cells = []
    for a in ["L-ship", "L-raw", "L-0910", "L-slow"]:
        cells.append(f"{rng(read,w,a)} ({rng(read,w,a,4)})" if (w, a) in read else "—")
    t.append(f"| `{w}` | " + " | ".join(cells) + " |")
t += ["", "Controls:", ""]
ctl = read.get(("-", "ctl"), [])
if ctl:
    cold = [float(x[3])/1000 for x in ctl]; warm = [float(x[4])/1000 for x in ctl]
    t.append(f"- **cache drop** — local NVMe 6 GiB tree cold {min(cold):.2f}-{max(cold):.2f} s vs warm {min(warm):.3f}-{max(warm):.3f} s ({min(cold)/max(warm):.0f}x at the least). \"Cold\" means cold.")
if ("small", "P-1") in read:
    t.append(f"- **fan-out (passthrough)** — `small` 1-wide {rng(read,'small','P-1')} vs 32-wide {rng(read,'small','P-32')}.")
if ("small", "L-slow") in read:
    t.append(f"- **fan-out (lean)** — `small` fan-out 1 {rng(read,'small','L-slow')} vs shipped {rng(read,'small','L-ship')}; on `big` {rng(read,'big','L-slow')} vs {rng(read,'big','L-ship')} — ranges make per-object fan-out moot there, as 2026-09-10 found.")
out["TABLE-READ"] = "\n".join(t)

# 2. warm
t = ["| workload | passthrough no-cache: cold, then warm | passthrough `--cache`: cold, then warm | local tree warm (lean's re-read) |", "|---|---|---|---|"]
for w in W:
    local = f"{min(warm):.3f}-{max(warm):.3f} s" if (w == "big" and ctl) else "— (not measured; a local tree)"
    t.append(f"| `{w}` | {rng(read,w,'P-32')} then {rng(read,w,'Pw-32')} | {rng(read,w,'PC-32')} then {rng(read,w,'PCw-32')} | {local} |")
out["TABLE-WARM"] = "\n".join(t)

# 3. meta
t = ["| workload | `find -type f` through the no-cache mount |", "|---|---|"]
for w in W:
    t.append(f"| `{w}` | {rng(read,w,'P-meta',3,1000.0,'{:.3f}')} |")
out["TABLE-META"] = "\n".join(t)

# 4. write
t = ["| workload | lean barrier (`LW`) | `aws s3 cp` 32-way (`SW`) | 32-wide `cp` into a mount (`PW-32`) |", "|---|---|---|---|"]
for w in W:
    t.append(f"| `{w}` | {rng(write,w,'LW')} | {rng(write,w,'SW')} | {rng(write,w,'PW-32')} |")
out["TABLE-WRITE"] = "\n".join(t)

# 5. the doors as deployed (csi read + csi write TSVs, optional)
csir = load(sys.argv[3]) if len(sys.argv) > 3 else {}
csiw = load(sys.argv[4]) if len(sys.argv) > 4 else {}
t = ["| workload | lean as deployed: pod Ready (syncer fetch) | lean, host engine (fetch) | passthrough as deployed, cold / warm | passthrough, host engine cold | lean publish as deployed (`Ld-W`) | passthrough write as deployed (`Pd-W`) |",
     "|---|---|---|---|---|---|---|"]
for w in W:
    # The deployed syncer is the daemon, which prints no phase line: its fetch column is 0 in the TSV and
    # is shown as unavailable, never as "0.00 s".
    ld_fetch = rng(csir,w,'Ld',4)
    if ld_fetch.startswith("0.00"):
        ld_fetch = "no phase line"
    t.append(f"| `{w}` | {rng(csir,w,'Ld')} ({ld_fetch}) | {rng(read,w,'L-ship')} ({rng(read,w,'L-ship',4)}) | {rng(csir,w,'Pd-32')} / {rng(csir,w,'Pdw-32')} | {rng(read,w,'P-32')} | {rng(csiw,w,'Ld-W')} | {rng(csiw,w,'Pd-W')} |")
ce = csir.get(("-", "ctl-exec"), [])
if ce:
    v=[float(x[3])/1000 for x in ce]; t += ["", f"kubectl exec round trip (control): {min(v):.2f}-{max(v):.2f} s per call."]
out["TABLE-CSI"] = "\n".join(t)

# 6. delta vs 2026-09-10 (figures copied from that doc)
OLD = {
    ("big","lean shipped"): (71.33, 71.88), ("small","lean shipped"): (16.92, 19.44), ("mixed","lean shipped"): (57.02, 57.83),
    ("big","lean ranged"):  (21.24, 23.96), ("small","lean ranged"):  (16.40, 17.39), ("mixed","lean ranged"):  (15.30, 18.30),
    ("big","s3 cp"):        (20.16, 21.74), ("small","s3 cp"):        (94.0, 96.1),   ("mixed","s3 cp"):        (23.9, 25.7),
    ("big","passthrough"):  (10.12, 12.90), ("small","passthrough"):  (265.6, 271.9), ("mixed","passthrough"):  (34.3, 35.6),
    ("big","write lean"):   (33.47, 35.54), ("mixed","write lean"):   (24.34, 25.01),
    ("big","write s3 cp"):  (19.40, 19.49), ("mixed","write s3 cp"):  (18.89, 18.93),
}
def old(w, k):
    o = OLD.get((w, k)); return f"{o[0]:.2f}-{o[1]:.2f} s" if o else "—"
t = ["| workload | door | 2026-09-10 | 2026-09-12 | change (bracket of the two ranges) |", "|---|---|---|---|---|"]
for w in W:
    t.append(f"| `{w}` | lean, then-shipped vs now-shipped, wall | {old(w,'lean shipped')} | {rng(read,w,'L-ship')} | {ratio(OLD.get((w,'lean shipped')), bounds(read,w,'L-ship'))} |")
    t.append(f"| `{w}` | lean, 09-10 ranged settings vs the same settings on today's binary, wall | {old(w,'lean ranged')} | {rng(read,w,'L-0910')} | {ratio(OLD.get((w,'lean ranged')), bounds(read,w,'L-0910'))} |")
    t.append(f"| `{w}` | lean, 09-10 ranged wall vs today's shipped FETCH (no fsync in either) | {old(w,'lean ranged')} | {rng(read,w,'L-ship',4)} | {ratio(OLD.get((w,'lean ranged')), bounds(read,w,'L-ship',4))} |")
    t.append(f"| `{w}` | passthrough AS DEPLOYED (09-10 was deployed too) | {old(w,'passthrough')} | {rng(csir,w,'Pd-32')} | {ratio(OLD.get((w,'passthrough')), bounds(csir,w,'Pd-32'))} |")
    t.append(f"| `{w}` | passthrough, host engine (not like-for-like) | {old(w,'passthrough')} | {rng(read,w,'P-32')} | {ratio(OLD.get((w,'passthrough')), bounds(read,w,'P-32'))} |")
    t.append(f"| `{w}` | `aws s3 cp` 32-way | {old(w,'s3 cp')} | {rng(read,w,'S-32')} | {ratio(OLD.get((w,'s3 cp')), bounds(read,w,'S-32'))} |")
    if write:
        t.append(f"| `{w}` | write: lean barrier | {old(w,'write lean')} | {rng(write,w,'LW')} | {ratio(OLD.get((w,'write lean')), bounds(write,w,'LW'))} |")
        t.append(f"| `{w}` | write: `aws s3 cp` 32-way | {old(w,'write s3 cp')} | {rng(write,w,'SW')} | {ratio(OLD.get((w,'write s3 cp')), bounds(write,w,'SW'))} |")
out["TABLE-DELTA"] = "\n".join(t)

for k, v in out.items():
    print(f"<<{k}>>"); print(v); print()
