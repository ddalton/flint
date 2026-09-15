#!/usr/bin/env python3
"""The tree benchmark from INSIDE a tenant pod, through the CSI bind and the
container runtime's mount: what an agent's own writes see. Stdlib only.

    python3 tree-bench-pod.py <dir> <reps> <size_mib> <files> <fsync_secs>

One JSON line per run: workload, rep, secs, and mib_s | files_s | ops_s.
The workloads match loop-tree-bench.sh (buffered and fsync'd sequential
writes, small files by tmp+rename then one sync, 4 KiB writes with an fsync
each), minus the cold read, which needs root to drop caches.
"""
import json
import os
import sys
import time

d, reps, size_mib, files, fsync_secs = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
mib = b"\0" * (1 << 20)
four_k = b"x" * 4096


def emit(workload, rep, secs, key, value):
    print(json.dumps({"workload": workload, "rep": rep, "secs": round(secs, 3), key: value}), flush=True)


def seq(path, fsync):
    t0 = time.monotonic()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    for _ in range(size_mib):
        os.write(fd, mib)
    if fsync:
        os.fsync(fd)
    os.close(fd)
    return time.monotonic() - t0


for rep in range(1, reps + 1):
    p = os.path.join(d, "seq.bin")
    for fsync, name in ((True, "seqw_fsync"), (False, "seqw_buf")):
        t = seq(p, fsync)
        emit(name, rep, t, "mib_s", round(size_mib / t, 1))
        os.unlink(p)
        os.sync()

    root = os.path.join(d, "small")
    os.makedirs(root, exist_ok=True)
    buf = b"x" * 16384
    t0 = time.monotonic()
    for i in range(files):
        sub = os.path.join(root, "d%03d" % (i // 1000))
        if i % 1000 == 0:
            os.makedirs(sub, exist_ok=True)
        tmp = os.path.join(sub, ".f%d.tmp" % i)
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        os.write(fd, buf)
        os.close(fd)
        os.rename(tmp, os.path.join(sub, "f%d" % i))
    os.sync()
    t = time.monotonic() - t0
    emit("small", rep, t, "files_s", round(files / t))
    for dirpath, _, names in os.walk(root, topdown=False):
        for n in names:
            os.unlink(os.path.join(dirpath, n))
        os.rmdir(dirpath)
    os.sync()

    p = os.path.join(d, "rand.bin")
    fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.ftruncate(fd, 64 << 20)
    ops, t0, off = 0, time.monotonic(), 0
    while time.monotonic() - t0 < fsync_secs:
        os.pwrite(fd, four_k, (off * 7919) % ((64 << 20) // 4096) * 4096)
        os.fsync(fd)
        ops, off = ops + 1, off + 1
    t = time.monotonic() - t0
    os.close(fd)
    os.unlink(p)
    emit("randw_sync", rep, t, "ops_s", round(ops / t))
