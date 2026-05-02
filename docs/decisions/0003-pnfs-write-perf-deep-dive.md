# ADR 0003: pNFS write perf — deep dive into the 1.6× win

**Date**: 2026-05-02 (same day, supersedes the surprised reading in ADR 0002)
**Status**: Informational
**Harness**: `tests/lima/pnfs/bench-sweep.sh` (parameterised) and `bench.sh`

## Why this exists

ADR 0002 reported a 2.10× pNFS write win on a single MacBook Air. The
user's reaction — "I am surprised how we got write improvement" — was
the right one. A single-host setup where every server runs on the same
kernel, reads/writes the same APFS volume, and all "wire" traffic is
loopback should not see protocol-level wins of that magnitude. Either
the measurement was biased, or there's a server-side concurrency story
worth understanding before any feature work continues.

A parameterised sweep across `numjobs ∈ {1,4,8}` and `fsync ∈ {0,1}`
plus a deep look at the server's RPC-handling path resolved both.

## Sweep results (write only — read sweep had a bug, see notes)

All measurements: `bs=1M`, `--size=128M` per job, `nconnect=4`,
`rsize=wsize=1 MiB`, `--ioengine=libaio --iodepth=16`, dropping the
client's page cache between phases.

| jobs | fsync | single-server | pNFS         | ratio  |
|-----:|------:|--------------:|-------------:|-------:|
| 1    | 0     | 183.4 MiB/s   | **1662 MiB/s** ⚠️ | 9.06× |
| 1    | 1     | 172.5 MiB/s   | 267.8 MiB/s  | 1.55×  |
| 4    | 0     | 214.9 MiB/s   | 404.4 MiB/s  | 1.88×  |
| 4    | 1     | 167.6 MiB/s   | 273.9 MiB/s  | **1.63×** |
| 8    | 0     | 201.4 MiB/s   | 341.6 MiB/s  | 1.70×  |
| 8    | 1     | 161.0 MiB/s   | 261.6 MiB/s  | 1.62×  |

Plus a targeted bs=4K small-block run:

| Workload                       | single-server | pNFS         | ratio |
|--------------------------------|--------------:|-------------:|------:|
| bs=4K, numjobs=4, fsync=1      | 158.0 MiB/s   | 248.5 MiB/s  | 1.57× |

## What's real and what isn't

### Discard the 9.06× outlier

`numjobs=1, fsync=0`: fio writes 128 MiB and exits without ever flushing
to disk. NFS WRITEs return as soon as the RPC is acknowledged; the
bytes sit in the macOS host's write cache. We're measuring memory
bandwidth + RPC overhead, not actual durability. **Throw this number
out.** It's the fsync=0 measurements at higher numjobs (1.88×, 1.70×)
that are also partly cache-fill but converge as workload size exceeds
cache headroom.

### The honest steady-state win is ~1.6×

`fsync=1` at every numjobs lands in a remarkably tight band: 1.55×,
1.63×, 1.62×. Block-size invariant (1.57× at bs=4K too). This is the
real signal.

The original 2.10× from `bench.sh` is reproducible only loosely — that
bench used numjobs=4, size=256M (twice as much per job) which lands
between the fsync=0 and fsync=1 numbers as the cache fills then drains.
**The advertise-able number is 1.6×, not 2.1×.** ADR 0002's headline
should be tightened.

### Single-server gets *slower* with concurrency

Look at the fsync=1 single-server column: 173 → 168 → 161 MiB/s as
numjobs goes 1 → 4 → 8. **More writers, less throughput.** This is
classic server-side serialization: more concurrent connections compete
for the same per-connection processing slot, fsyncs queue up against
the same APFS journal, etc.

pNFS fs=1 stays flat at 262-274 across numjobs — it's hitting a
different ceiling that doesn't degrade with concurrency.

## Why the win exists (code-level)

The key file is `spdk-csi-driver/src/nfs/server_v4.rs`. Look at
`handle_tcp_connection` (line 149):

```rust
loop {
    // Read RPC record marker (4 bytes)
    reader.read_exact(&mut marker_buf).await?;
    // ... read frame body ...

    // Dispatch synchronously — await before next read
    let compound_resp = dispatcher.dispatch_compound(...).await;

    // Encode + write response
    writer.write_all(&compound_data).await?;
}
```

**Per-TCP-connection, RPCs are processed strictly sequentially.** The
connection reads request, awaits dispatch, sends response, then reads
the next request. There's no pipelining — even though NFSv4.1 sessions
explicitly allow out-of-order request/response with slot tables.

So the actual server-side parallelism is bounded by the number of TCP
connections × 1 in-flight RPC each. With Linux's `nconnect=4`:

| Setup        | TCPs to "the server" | Parallel RPC slots |
|--------------|---------------------:|-------------------:|
| single-server | 4 to flint-nfs-server | 4 |
| pNFS          | 4 to MDS (metadata only) + 4 to DS1 + 4 to DS2 | 8 (data path) |

**For write workloads where data goes to the DSes, pNFS effectively has
2× the parallel server-side RPC slots vs single-server.** The
measurement 1.6× is the practical realisation of that 2× capacity, with
the gap eaten by APFS journal serialization on the shared physical
disk.

This *is* a real protocol-shaped win, but the mechanism is "shard the
server" — not magical pNFS striping efficiency. A multi-host pNFS
deployment with each DS on its own NIC and physical disk would
generalize this directly: N DSes × 4 connections each = 4N parallel
slots, no shared disk = ~N× scaling.

## Implementation findings worth noting

While digging, three concrete inefficiencies surfaced. None are bugs
— things work — but each leaves perf on the table.

### 1. Per-connection RPC serialization (`server_v4.rs:176`)

The single-loop-per-connection design is the actual single-host
bottleneck. RFC 8881 §2.10.6 explicitly permits per-session pipelining
via slot tables; we're not using it.

A pipelined design — read RPCs continuously, dispatch each via
`tokio::spawn`, write responses as they complete — would let a single
flint-nfs-server saturate available CPU/disk bandwidth and likely
close most of the gap to pNFS *for single-host workloads*. Estimate:
medium-sized refactor (~1 week). Not on the critical path for the
pNFS feature; documented here as a single-server optimization.

### 2. COMMIT reopens the file (`ioops.rs:880`)

```rust
let file = match std::fs::OpenOptions::new()
    .write(true)
    .open(&path)
{
    Ok(f) => f,
    ...
};
file.sync_all()?;
```

Each COMMIT does a fresh `open` instead of reusing the WRITE-side
cached fd from `self.fd_cache`. On every fsync we pay an extra
namespace lookup + open syscall. Modest but measurable in fsync-heavy
workloads. **One-line fix when convenient.**

### 3. Per-file mutex on writes (`ioops.rs:782`)

```rust
let file = file_arc.lock().unwrap();
file.write_at(&data_clone, offset)?;
```

`write_at` (positioned I/O) is safe to call concurrently from multiple
threads — no mutex needed. The mutex serializes writes to the same
file from different threads. For bench workloads with one job per
file, this isn't the bottleneck (one stateid → one mutex → one
writer). For workloads with multiple writers to the same file (rare
in NFS), it'd be a real cap.

Removing the mutex needs careful audit (file metadata reads, etc.)
but is technically correct per Unix `pwrite(2)` semantics.

## What this means for the project

1. **The 1.6× write win is real, modest, and reproducible.** It's a
   shippable feature on its own (a 60% throughput improvement for
   fsync-heavy parallel-writer workloads is genuinely useful), but
   "2× faster" was an overreach.

2. **The cross-host story remains the load-bearing claim.** On real
   hardware with N independent DSes, the win should scale with N,
   limited by per-DS NIC and disk rather than shared APFS journals.
   Re-bench when we have that hardware.

3. **The single-server NFS path has known headroom.** A pipelined
   handler would lift it. If we ever care about "is single-server
   *good enough* for your workload before pNFS?" — yes, with that
   refactor.

4. **CSI integration (PR 1 just landed) is still the right next move.**
   It exposes the win to users; the deep-dive doesn't change priorities.

## Reproducing

```bash
# Sweep across numjobs × fsync × rw:
tests/lima/pnfs/bench-sweep.sh

# Targeted single config (the 1.55-1.63× number):
tests/lima/pnfs/bench.sh
```

## Caveats / known harness bugs

- `bench-sweep.sh` reads return 0.0 MiB/s. Bug: each variant uses a
  unique `--name=` for fio, so reads can't find the previous variant's
  files. Doesn't affect write conclusions. Fix when needed: share
  `--name=` per (server, numjobs) pair so write→read see the same files.
- All numbers are single-host; loopback TCP and shared APFS limit the
  ceiling. Treat as a *floor* for what pNFS can do, not an estimate
  of cross-host perf.
- macOS APFS commit semantics are different from Linux ext4/xfs.
  Reproducing on a Linux host would give different absolute numbers
  (probably higher) but likely a similar ratio.
