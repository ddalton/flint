# pNFS performance: improvement & benchmarking plan

**Date**: 2026-07-04
**Status**: Proposed (Phase 0 landed)
**Prior art**: ADR 0002 (first baseline), ADR 0003 (write-perf deep dive),
`docs/plans/pnfs-production-readiness-design-spec.md` (pipelining spec)

## Where we stand (re-baselined 2026-07-04)

`tests/lima/pnfs/bench.sh`, single Mac host, 2 DSes, kernel client in lima,
`numjobs=4 × 256 MiB × 1M blocks`:

| Workload | single-server | pNFS (2 DS) | ratio |
|---|---:|---:|---:|
| WRITE aggregate | 347.2 MiB/s | 339.7 MiB/s | 0.98× |
| READ aggregate | 225.1 MiB/s | 279.9 MiB/s | 1.24× |

This supersedes ADR 0002/0003's 1.6–2.1× write win. Most of that win was
an artifact of the SEQUENCE slot bug fixed in 73e23f2: the server
advertised a 1-slot session, capping every client at one in-flight RPC
per session, so `nconnect=4` did nothing and single-server was
artificially slow (133 MiB/s then vs 347 now). On one host, one server
saturates the shared disk; pNFS's remaining single-host edge is reads.

**The load-bearing claim — aggregate bandwidth scales ~linearly with DS
count when each DS has its own NIC and disk — has never been measured.**
Everything below exists to make it true and then prove it.

## Phase 0 — landed 2026-07-04

- SEQUENCE slot-table fix (73e23f2): clients can pipeline up to the
  negotiated slot count; `nconnect` now works. (Server-side dispatch is
  still serial per connection — see Phase 2.)
- Per-file stripe rotation via `nfl_first_stripe_index` (6502bdf):
  without it, every file < 8 MiB lived entirely on DS[0] — the ML
  small-file workload serialized onto one DS by construction. Validated
  e2e: 16 small files spread across both DSes, checksums intact.

## Phase 1 — DS data-path quick wins — LANDED 2026-07-04

Each item was a measured change: `tests/lima/pnfs/bench-sweep.sh`
before/after; keep if ≥5% aggregate improvement or clearly neutral+correct.

1. **DS fd cache** — DONE. `pnfs/ds/io.rs` opened the backing file on
   *every* READ, WRITE, **and** COMMIT, and used seek+read (unsafe to
   share an fd). Now: a filehandle-keyed `DashMap` fd cache (cap 512,
   arbitrary eviction) + positioned I/O (`read_at`/`write_all_at`).
   Hits also skip the per-op filehandle→path resolution. COMMIT reuses
   the cached fd (ADR 0003 item 2, DS side).
2. **COMMIT fd reuse / write-mutex removal (standalone)** — found
   ALREADY DONE in `ioops.rs` (landed since ADR 0003 was written).
   What remained: the standalone READ path still opened per-op — now
   it reuses/populates the same stateid-keyed cache (write-only
   entries get a `writable` flag; READ falls back to read-only opens).
   Bonus correctness: special stateids (all-zero/all-one `other`) are
   no longer cacheable (they aliased different files to one key), and
   cache hits now require a path match with the presented filehandle.
3. **Per-op `info!` logging demoted to `debug!`** on READ/WRITE/COMMIT
   hot paths (both servers) — default level is info, so every RPC was
   formatting + writing log lines.
4. **bench-sweep read phase fixed** — fio `--name` is now shared per
   numjobs so read variants find the write-phase files. Reads measure
   real NFS reads for the first time; ADR 0002/0003 read rows are void.

**Result.** The 1M-block sequential sweep could not resolve the change
(±15% run-to-run swings in both directions, including untouched paths —
one open(2) amortized over a 1 MiB transfer is below this rig's noise
floor). A 4k direct-I/O A/B/A/B microbench (old/new binaries
interleaved, per-RPC cost dominant, old↔old variance ~2.5%) is
decisive:

| 4k direct, 4 jobs × QD16, 2 DSes | old | new | delta |
|---|---:|---:|---:|
| randread IOPS | 36,426 / 37,322 | 44,016 / 43,347 | **+18%** |
| randwrite IOPS | 346 / 334 | 862 / 720 | **+2.1–2.5×** |

The randwrite jump is the old path's per-WRITE open/close forcing
writeback on close of a dirty file; the cached fd avoids it. Integrity
drill: 32 × 1 MiB concurrent-writer files through the pNFS mount,
32/32 md5s intact after client cache drop, files spread 13/19 across
the DSes (rotation + cache coexist).

**Not done here (deliberate)**: DS I/O still runs blocking file ops on
the tokio workers and dispatch is serial per connection — that's the
Phase 2 pipelining refactor. DS write verifier is still a fixed
`[0u8; 8]` (client can't detect DS reboot / lost unstable writes) —
folded into Phase 4 durability gates.

## Phase 2 — RPC pipelining — LANDED 2026-07-05

Implemented per `pnfs-production-readiness-design-spec.md` (see the
implementation note there for the two deltas: semaphore instead of
mpsc+writer-task, and the DS brought INTO scope). One
`nfs/pipeline.rs` serves all three roles — standalone, MDS, DS.
`FLINT_NFS_MAX_INFLIGHT` (default 64 = B1; 0 = sequential fallback,
I5). DS blocking file I/O restructured for concurrency: fsync paths
on `spawn_blocking`, >64 KiB transfers via `block_in_place`, small
transfers inline.

**What the 4k-direct A/B taught (each step measured, old↔old ~2.5%):**

| DS I/O strategy under pipelining | randread | randwrite |
|---|---:|---:|
| Phase 1 serial loop (reference) | ~45.3k IOPS | ~810 |
| `spawn_blocking` everything | −14% | +15% |
| `block_in_place` everything | −10% | ~par |
| + adaptive inline dispatch | −12% | +25% |
| + ≤64 KiB I/O runs inline (**shipped**) | **−3%** | **+26%** |

Two lessons now encoded in the code: (1) a per-op cross-thread
handoff (`spawn_blocking`, and even `block_in_place`'s queue
migration) costs more than a µs-scale page-cache read; (2) task
fan-out only pays when requests actually overlap — so `submit()`
dispatches INLINE unless the read buffer holds more input or other
dispatches are in flight (spec open-question 2, option B). The −3%
residual is a QD-1 latency probe on a 2-vCPU client VM (DS at ~24%
CPU — latency-bound, not throughput-bound); concurrent workloads are
where pipelining pays:

- 1M sweep, pNFS reads: j4 +18.6%, j8 +10.4% (first build; final
  numbers below).
- 4k randwrite (fsync-heavy): +26%.

**Gates on the shipping build**: pynfs **171/171**, `test-pnfs-smoke`
✓, `test-pnfs-recall` ✓ (I4/T5), sequential-fallback smoke ✓ (I5; an
initial failure was stale client state from four back-to-back server
restarts on one VM — passes in isolation), 609 unit tests incl.
pipeline T1/T2/T4/T6 + inline-fast-path + write-poisoning; T3 replay
covered by the existing session slot tests + pynfs.

**Final-build sweep** (Phase 1 → Phase 2, same rig, 1M blocks; rows
that reproduced across two independent Phase-2 sweeps — the fs=0 j1
page-cache-fill cells swing ±30% run-to-run and are excluded):

| Workload | Phase 1 | Phase 2 | delta |
|---|---:|---:|---:|
| pNFS read j4 | 230.9 | 270.8 | **+17.3%** (was +18.6% on first build) |
| pNFS read j8 | 254.3 | 271.4 | **+6.7%** (was +10.4%) |
| pNFS write j8 fs=1 | 295.4 | 307.5 | +4.1% (was +11.6%) |
| single write j8 fs=1 | 307.4 | 334.0 | +8.7% (was +15.0%) |
| everything else | — | — | within rig noise (±5-15%) |

Note the spec's B4 (≥220 MiB/s at `numjobs=4 fsync=1`) predates the
slot fix; it was already exceeded before this phase. The honest B4
successor is the concurrency behavior above.

**Known limits of this rig's evidence**: one 2-vCPU client VM over
loopback can neither saturate the pipelined server nor exercise
multi-client contention — the workloads pipelining exists for.
Phase 3's cross-host bench is where the structural win must show up;
if it doesn't, `FLINT_NFS_MAX_INFLIGHT=0` is the one-knob rollback.

## Phase 3 — the cross-host benchmark — DONE 2026-07-05, ALL GATES PASS

**Results are in ADR 0004** (`docs/decisions/0004-pnfs-cross-host-scaling.md`).
Headlines: aggregate seq read 328.5 → 695.7 → **1978.3 MiB/s** at
N=1/2/4 (6.02×, gate was ≥3.2×); writes exactly 4.00×; 4k randread
4.68× (134.6k IOPS at N=4); small-file spread 48.4/51.6% at N=2; MDS
0% CPU at every N; single client reads ~1 GiB/s from 4 DSes.
Cross-host pipelining A/B: **+30% on the small-file dataloader
shape**, +7.6% on aggregate seq read, −12% on deep-QD 4k random
(follow-up below). Bench found a P1: **DS basename collision** (the
MDS-issued-FH fallback rebases to `<data-dir>/<basename>`; same-named
files in different dirs share one backing file) — fix by keying
DS-local storage on MDS file-id; added to Phase 4.

**Follow-up (perf)**: per-op-cost-adaptive dispatch — inline µs-scale
ops even when the connection has backlog (deep-QD 4k is where spawn
overhead still loses); consider a per-connection dispatch-time EMA.

The original plan for reference:

Runs on a disposable extension of the standing runk cluster (trove
project 28): keep the on-demand CP, add spot workers as DS nodes
(i4i.large NVMe — same recipe as the replica-drill workers).

**Topology**: 1 MDS pod (pinned to its own node), N DS pods (one per
node, DaemonSet, local NVMe-backed dir instead of emptyDir for the
bench), M client pods spread across remaining nodes.

**Matrix** (fio in each client pod, page cache dropped between phases):

| Axis | Values |
|---|---|
| DS count N | 1, 2, 4 |
| Client pods M | 1, 4 |
| Sequential 1M, fsync=1 | read, write |
| Random 4k, iodepth=32 | read |
| Small-file dataloader | 16k files × 1 MiB, readers shuffled |

**Pass criteria** (recorded as ADR 0004 regardless of outcome):
- Aggregate sequential read at N DSes ≥ 0.8·N × the N=1 number.
- Small-file per-DS byte spread within 40–60% at N=2 (rotation working
  at scale).
- MDS CPU and per-op latency flat as N grows (metadata path not the
  bottleneck).
- Honest caveats section: NIC saturation points, client-side limits.

**Effort**: ~1 day of cluster time once Phase 1 lands; harness is
`bench.sh` with the three changes ADR 0002 §"Re-running on real
hardware" already lists.

## Phase 4 — productionization gates (decide before advertising pNFS)

Not perf work, but perf claims are moot without them:

1. **DS durability story.** DSes write to `emptyDir` — node loss = data
   loss, no replication. Either back DSes with flint block volumes
   (replicated lvols) or ship pNFS explicitly as ephemeral scratch tier.
   This decision shapes whether Phase 3 numbers are marketable.
0. **DS basename collision (P1, found by the Phase 3 bench).** The
   MDS-issued-FH fallback in `pnfs/ds/io.rs filehandle_to_path`
   rebases every file to `<data-dir>/<basename>`; files with equal
   basenames in different directories silently share one backing
   file (data corruption for any multi-directory namespace). Fix by
   keying DS-local storage on the MDS file-id (the pNFS v2
   filehandle already carries it). Must land before any external use.
   Related: the DS write verifier is a fixed `[0u8; 8]`
   (`pnfs/ds/io.rs generate_verifier`), so clients cannot detect a DS
   restart and will not retransmit lost UNSTABLE writes — must become
   a boot-time value before any durability claim.
2. **MDS export size check**: bench flags ~1 GiB *apparent* size on the
   MDS export ("should be ~0") — confirm with `du` it's sparse
   LAYOUTCOMMIT EOF metadata, not clients falling back to MDS-proxy I/O.
3. **DS-death drills** under load (CB_LAYOUTRECALL path exists; drill it
   the way the RWX teardown drills were done).
4. **Stripe unit tuning**: 8 MiB is hardcoded in the LAYOUTGET arm;
   evaluate 1–4 MiB for small-file-heavy datasets once Phase 3 gives a
   measurement rig.

## Non-goals

- Flexfiles / block layouts (pynfs FF*/BLOCK* families): different
  layout types, not needed by the Linux client for files-layout striping.
- Delegations/backchannel for the data path (tracked separately;
  disabled by default as of 1da22d5).
