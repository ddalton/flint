#!/usr/bin/env python3
"""Model-shaped pNFS benchmark: what a HuggingFace checkpoint actually does.

    pnfs-model-bench.py write --dir /data --shards 8 --shard-gib 4
    pnfs-model-bench.py read  --dir /data --mode mmap|stream --workers 1

WHY NOT fio. Every pNFS number flint has ever recorded came from fio with
`--direct=1` — O_DIRECT, 1 MiB blocks, 128 requests in flight. That is a
storage-array benchmark. Loading a checkpoint is none of those things:

  * safetensors `from_pretrained` **mmaps** the shard and the framework
    copies tensors out of the mapping. Page faults and kernel readahead
    drive the I/O, not an application queue depth.
  * `save_pretrained` writes each shard with ordinary buffered writes.
  * A checkpoint is a handful of BIG files (2-10 GiB shards), usually
    walked one at a time — not 4 files hammered by 32 concurrent requests.

Those differences decide whether striping helps. O_DIRECT with iodepth 32
keeps every data server busy by construction. A single-threaded mmap walk
has ONE page-fault stream, and whether that spreads across five servers
depends entirely on readahead reaching past the stripe unit. This is the
workload that can actually show a 5-DS fleet behaving like one server.

Reports wall-clock and GiB/s per phase, plus the number that a user
recognises: seconds to load the whole checkpoint.

CACHES MUST BE DROPPED before a read run or this measures RAM — the client
here has 96 GiB and a 32 GiB checkpoint fits entirely. The wrapper script
does it; `--check-cache` warns if the read ran suspiciously fast.
"""
import argparse
import mmap
import os
import resource
import sys
import time
from concurrent.futures import ProcessPoolExecutor

GIB = 1024 ** 3
CHUNK = 8 * 1024 * 1024   # 8 MiB, the granularity a framework copies at


# PROCESSES, NOT THREADS. Copying out of an mmap is a memcpy performed with
# the GIL held, so threaded shard loaders serialise. Measured against tmpfs
# on the client — the instrument's own ceiling, no storage involved:
#     1 thread  1934 MiB/s
#     8 threads 1769 MiB/s   <- concurrency made it SLOWER
# A threaded run therefore could not have distinguished "pNFS delivered
# 904 MiB/s" from "the benchmark capped at 904". Processes have no shared
# GIL, so the number that comes back is the storage system's.
def _write_one(arg):
    return write_shard(arg[0], arg[1])


def _read_one(arg):
    return (read_mmap if arg[1] == "mmap" else read_stream)(arg[0])


def paths(d, n):
    # Real shard naming, so nothing here can accidentally read or clobber a
    # fio file left on the same volume.
    return [os.path.join(d, f"model-{i:05d}-of-{n:05d}.safetensors")
            for i in range(n)]


def write_shard(path, nbytes):
    buf = os.urandom(CHUNK)          # incompressible, like real weights
    written = 0
    with open(path, "wb", buffering=0) as fh:
        while written < nbytes:
            take = min(CHUNK, nbytes - written)
            fh.write(buf[:take])
            written += take
        fh.flush()
        os.fsync(fh.fileno())
    return written


def read_stream(path):
    """Plain buffered sequential read — torch.load, or a download-to-disk."""
    total = 0
    view = memoryview(bytearray(CHUNK))
    with open(path, "rb") as fh:
        while True:
            got = fh.readinto(view)
            if not got:
                break
            total += got
    return total


def read_mmap(path):
    """mmap and copy out of the mapping — what safetensors does."""
    total = 0
    with open(path, "rb") as fh:
        size = os.fstat(fh.fileno()).st_size
        if size == 0:
            return 0
        with mmap.mmap(fh.fileno(), size, prot=mmap.PROT_READ) as mm:
            try:
                mm.madvise(mmap.MADV_SEQUENTIAL)
            except (AttributeError, OSError):
                pass          # older kernels/pythons: the walk still faults
            off = 0
            while off < size:
                end = min(off + CHUNK, size)
                # The slice copies, which is what forces the page faults.
                total += len(mm[off:end])
                off = end
    return total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("phase", choices=["write", "read"])
    ap.add_argument("--dir", required=True)
    ap.add_argument("--shards", type=int, default=8)
    ap.add_argument("--shard-gib", type=float, default=4.0)
    ap.add_argument("--mode", choices=["mmap", "stream"], default="mmap")
    ap.add_argument("--workers", type=int, default=1,
                    help="shards loaded concurrently; 1 = the honest default "
                         "for from_pretrained, >1 = accelerate/vLLM style")
    a = ap.parse_args()

    files = paths(a.dir, a.shards)
    nbytes = int(a.shard_gib * GIB)
    total_expected = nbytes * a.shards

    if a.phase == "write":
        os.makedirs(a.dir, exist_ok=True)
        t0 = time.time()
        if a.workers > 1:
            with ProcessPoolExecutor(a.workers) as ex:
                done = sum(ex.map(_write_one, [(p, nbytes) for p in files]))
        else:
            done = sum(write_shard(p, nbytes) for p in files)
        dt = time.time() - t0
    else:
        missing = [p for p in files if not os.path.exists(p)]
        if missing:
            print(f"ERROR: {len(missing)} shards missing — run write first",
                  file=sys.stderr)
            return 2
        fn = read_mmap if a.mode == "mmap" else read_stream
        t0 = time.time()
        if a.workers > 1:
            with ProcessPoolExecutor(a.workers) as ex:
                done = sum(ex.map(_read_one, [(p, a.mode) for p in files]))
        else:
            done = sum(fn(p) for p in files)
        dt = time.time() - t0

    # CPU accounting for the reader itself, self and children, so a
    # single-stream number can be read as "waiting on the fleet" or "burning
    # a core in the fault/copy path". Raising readahead 4.3x above the
    # default bought only 13% here, which makes the difference the whole
    # question.
    ru = resource.getrusage(resource.RUSAGE_SELF)
    rc = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu = (ru.ru_utime + ru.ru_stime + rc.ru_utime + rc.ru_stime)
    gib = done / GIB
    print(f"RESULT phase={a.phase} mode={a.mode} workers={a.workers} "
          f"shards={a.shards} gib={gib:.1f} seconds={dt:.1f} "
          f"gibps={gib/dt:.3f} mibps={done/1048576/dt:.0f} "
          f"cpu_s={cpu:.1f} cores_busy={cpu/dt:.2f}")
    if a.phase == "read" and done and (done / dt) > 6 * GIB:
        print("WARNING: faster than any network path here — page cache was "
              "almost certainly NOT dropped; this number is RAM, not pNFS",
              file=sys.stderr)
    if done != total_expected and a.phase == "read":
        print(f"NOTE: read {done} bytes, expected {total_expected}",
              file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
