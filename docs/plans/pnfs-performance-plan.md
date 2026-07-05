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

## Phase 1 — DS data-path quick wins (~1–2 days)

Each item is a measured change: run `tests/lima/pnfs/bench-sweep.sh`
before/after; keep if ≥5% aggregate improvement or clearly neutral+correct.

1. **DS fd cache.** `pnfs/ds/io.rs` opens the backing file on *every*
   READ and WRITE. Mirror the standalone server's stateid-keyed fd cache
   (`ioops.rs fd_cache`) or an LRU keyed by filehandle.
2. **COMMIT fd reuse** (ADR 0003 item 2): COMMIT re-opens the file to
   fsync instead of using the cached write fd. One-line once (1) exists.
3. **Drop the per-file write mutex** (ADR 0003 item 3): `write_at` is
   thread-safe positioned I/O; the mutex serializes concurrent writers
   to the same file. Audit metadata reads before removal.
4. **Fix bench-sweep read phase** (known harness bug: unique `--name=`
   per variant means reads never find write-phase files).

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
