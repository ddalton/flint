#!/usr/bin/env python3
"""Held-open O_DIRECT re-writer for the fence rig (FENCE=1).

One process, one fd held OPEN for the whole run: closing the file would
LAYOUTRETURN the layout and drop the grant row, so a per-write dd-loop
leaves no grant to fence between passes. Rewrites a page-aligned 1 MiB
buffer at offset 0 (O_DIRECT keeps every write on the raw layout path;
the grant row and committed extents stay live).

On the fence the pwrite errors — EIO / reservation conflict, in ~5 s
thanks to the session's fast_io_fail_tmo, never a D-state park — and we
record the errno and exit 0. Usage: rig-writer.py <path> <done-file>.
"""
import mmap
import os
import sys

path, done = sys.argv[1], sys.argv[2]
# Optional write cap (argv[3]). The FENCE drill fences within seconds, so
# the 200k default (~200 GB) never mattered there — the SWEEP drill waits
# out a 90 s lease against a page-cache-backed lvol at ~3 GB/s, which
# EXHAUSTED the default mid-drill (the rig's "a zombie, not a corpse"
# failure) — it passes a bigger cap.
cap = int(sys.argv[3]) if len(sys.argv) > 3 else 200000
# O_CREAT: the FENCE/SWEEP drills re-write the base flow's existing
# data.bin, but the ZOMBIE drill's writer is the file's first toucher.
fd = os.open(path, os.O_WRONLY | os.O_DIRECT | os.O_CREAT, 0o644)
buf = mmap.mmap(-1, 1 << 20)  # anonymous mmap is page-aligned for O_DIRECT
buf.write(b"\0" * (1 << 20))
buf.seek(0)

n = 0
try:
    while n < cap:
        os.pwrite(fd, buf, 0)
        n += 1
except OSError as e:
    with open(done, "w") as f:
        f.write("EXIT %d after %d writes\n" % (e.errno, n))
    sys.exit(0)

with open(done, "w") as f:
    f.write("EXIT 0 (exhausted after %d writes)\n" % n)
