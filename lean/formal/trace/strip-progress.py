#!/usr/bin/env python3
"""Make a prepared trace-check directory runnable by tla-mc.

Usage: python3 strip-progress.py <outdir>

TraceCore.tla's `TraceProgress` prints the furthest point a trace reached
through TLC's TLCSet/TLCGet registers, which tla-mc does not have. It is a
diagnostic only: the verdict is `TraceIncomplete`'s (violated = the whole
trace was followed). This drops the printer from the run's copy of
TraceCore.tla and its invariant from TraceCore.cfg; the sources are not
touched. Each removal must match exactly once, or nothing is run.
"""
import re
import sys

d = sys.argv[1]
tla = open(f"{d}/TraceCore.tla").read()
tla, n = re.subn(r"ASSUME TLCSet\(1, 0\)\n.*?/\\ TLCSet\(1, l\)\n", "", tla, flags=re.S)
if n != 1:
    sys.exit(f"strip-progress: the TraceProgress block matched {n} times in {d}/TraceCore.tla")
cfg = open(f"{d}/TraceCore.cfg").read()
if cfg.count("INVARIANT TraceProgress\n") != 1:
    sys.exit(f"strip-progress: no single TraceProgress invariant in {d}/TraceCore.cfg")
open(f"{d}/TraceCore.tla", "w").write(tla)
open(f"{d}/TraceCore.cfg", "w").write(cfg.replace("INVARIANT TraceProgress\n", ""))
