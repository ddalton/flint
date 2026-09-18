# What the inode guard costs

**2026-09-18 · 10.0.0.249 · 200 x (create, write 4K, fsync, close), identical workload per arm**

## Answer: +1 `statx` per data RPC

| arm | `statx` | `openat` | `pwrite` |
|---|---|---|---|
| unfixed | **3808** | 600 | 1800 |
| fix (stats twice on a miss) | **4008** | 600 | 1800 |
| fix (stats once) | **4008** | 600 | 1800 |

+200 statx over 200 writes = **+1 per WRITE**, a 5.3% rise in statx for
this workload. `openat` and `pwrite` are identical across arms, which is
what says the workload itself was deterministic and the +200 is signal.

In wall time it does not resolve: the three arms overlap
(`miss` 1.72-1.98s across all of them, ~7% run-to-run variance). A ~1us
syscall disappears against an NFS RPC round-trip. Timing is the wrong
instrument for this question.

The baseline is already stat-heavy — 3808 statx for 200 files is ~19 per
file operation — so this is +5% of an already-large number rather than +1
on a lean path. That cuts both ways and is worth knowing.

## Why not `stat_cache`

The obvious optimisation is the repo's counter-validated `stat_cache`,
which exists for exactly this grind. It is **not safe here**: RENAME
calls `stat_cache::forget(&source_path)` and never forgets the
DESTINATION — and the destination is precisely the name whose meaning
changed. Serving the guard from that cache would return the OLD inode for
the replaced name and hand back the stale descriptor, reinstating the
defect this guard exists to stop.

(The missing dest-forget looks like a latent staleness bug in its own
right, independent of this work. Recorded, not fixed here.)

## Two rig corrections, both of which gave a confident wrong answer first

1. **Timing measured nothing.** The "hot file" phase ran 2000 ops in
   0.03s — the kernel coalesced them, so no per-op RPC was generated. A
   "no measurable overhead" result from that arm would have been true and
   meaningless.
2. **The first syscall count read `@stat:0` in EVERY arm**, including
   binaries known to stat. Rust's `std::fs::metadata` issues `statx(2)`,
   not `newfstatat(2)`. A zero that agrees across all arms reads as "no
   difference" and actually means "not measuring".

## Note on the dedup arm

`fix (stats once)` matches `fix (stats twice)` at 4008 because this
workload never misses the descriptor cache: OPEN seeds the fd, so WRITE
hits by stateid and `adopt_open_fd` — the second stat — is never reached.
The dedup is still correct; this workload does not demonstrate it, and
the number should not be read as evidence that it does nothing.

Reproduce: `count.sh <binary> <label>` (syscall counts, deterministic),
`perf.sh <binary> <label> [reps]` (wall time, which could not resolve it).
