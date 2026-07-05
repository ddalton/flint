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

## Phase 2 — RPC pipelining (~1 week)

Implement `docs/plans/pnfs-production-readiness-design-spec.md` as
written (invariants I1–I5, bounds B1–B4). One dispatch loop change
benefits all three server roles: single-server, MDS metadata ops, and
per-DS throughput. Success bar from the spec: single-server
`numjobs=4, fsync=1, bs=1M` ≥ 220 MiB/s (was ~165 at spec time; the slot
fix has already moved this — re-baseline first, keep the ≥30% spirit).
Now that clients genuinely fill 64 slots, head-of-line blocking on the
serial loop is the top remaining structural bottleneck.

## Phase 3 — the cross-host benchmark (the proof)

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
