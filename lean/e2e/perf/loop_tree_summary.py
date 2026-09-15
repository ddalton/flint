#!/usr/bin/env python3
"""Summarize loop-tree-bench.sh output: per backing and workload, each
layout's median and range, and the median as a fraction of plain's.

    python3 loop_tree_summary.py results-loop-tree-bench-*.out

Higher is better for every metric (MiB/s, files/s, IOPS). Ranges, not only
medians: two layouts whose ranges overlap are not told apart by this run.
"""
import json
import statistics
import sys
from collections import defaultdict

rows = defaultdict(list)
metric_of = {}
for path in sys.argv[1:]:
    for line in open(path):
        line = line.strip()
        if not line.startswith("{"):
            if line.startswith(("VOID", "SETTLED", "LAZYINIT", "BACKING")):
                print(line)
            continue
        r = json.loads(line)
        m = next(k for k in r if k not in ("backing", "layout", "workload", "rep", "secs"))
        rows[(r["backing"], r["workload"], r["layout"])].append(r[m])
        metric_of[r["workload"]] = m

print()
for backing in sorted({k[0] for k in rows}):
    print(f"## {backing}")
    print(f"| workload | metric | plain | loop (dio off) | loopdio | loop/plain | loopdio/plain |")
    print("|---|---|---|---|---|---|---|")
    for wl in ("seqw_buf", "seqw_fsync", "seqr_cold", "small", "randw_sync"):
        cells, med = [], {}
        for layout in ("plain", "loop", "loopdio"):
            v = rows.get((backing, wl, layout), [])
            if not v:
                cells.append("—")
                continue
            med[layout] = statistics.median(v)
            cells.append(f"{med[layout]:g} ({min(v):g}–{max(v):g}, n={len(v)})")
        ratio = lambda l: f"{med[l] / med['plain']:.2f}" if l in med and med.get("plain") else "—"
        print(f"| {wl} | {metric_of.get(wl, '?')} | {cells[0]} | {cells[1]} | {cells[2]} | {ratio('loop')} | {ratio('loopdio')} |")
    print()
