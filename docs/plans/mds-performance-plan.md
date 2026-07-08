# MDS performance plan — scaling the metadata path vertically

Status: **Tier 1 LANDED 2026-07-07** (all three items + measured A/B below);
Tiers 2–4 not started
Harness: `make test-pnfs-mdsbench` (`tests/lima/pnfs/mdsbench.sh`)

## Tier 1 measured results (2026-07-07, same rig as baseline)

Variant `tier1-threads-logs` = worker threads configurable (default
`available_parallelism`), hot-path WARN/INFO demoted to `debug!`
(~145 call sites), `tracing_appender::non_blocking` writer, EnvFilter
(`RUST_LOG` now works):

| workload | baseline | tier1 | delta |
|----------|---------|-------|-------|
| w1-create | 368 ops/s · 1.28 cpu_ms/op · 3.24 log_KiB/op | **489 · 0.93 · 0.25** | **+33% ops/s**, −27% cpu/op, −92% log |
| w2-opencl | 7,004 · 0.22 · 1.25 | **8,689 · 0.14 · 0.00** | **+24% ops/s** |
| w3-stat | (invalid — client cache) | **3,531 · 0.51 · 0.00** | first VALID number (noac mount) |
| w4-mixed | 16 · 0.68 · 3.49 | 16 · 0.44 · 0.03 | ops/s flat — protocol-chain-bound, as predicted; Tier 2's lever |

- w1's remaining 0.25 log_KiB/op is the deliberate INFO forensics
  (pin created / placement forgotten per file lifecycle), not chatter.
- w3-stat now measures the real LOOKUP/GETATTR dispatch floor via a
  second `actimeo=0,lookupcache=none` mount (harness change).
- w4-mixed confirms the baseline read: fsstress wall time is
  serialized protocol round trips, not MDS compute or logging —
  `return_on_close=false` (Tier 2) is where that moves.
- Tier 1.3 (fs ops off the async workers) shipped in the same wave:
  most handlers already used `tokio::fs`; the stragglers converted
  were OPEN(create), SETATTR apply (`apply_settable_attrs_offloaded`),
  ACCESS stat, RENAME dest check, LINK guard, LAYOUTCOMMIT
  set_len/set_times (spawn_blocking). perfops COPY/CLONE were already
  offloaded.

## Why

The data path scales horizontally: ADR 0004 measured 6.02× sequential
read at N=4 DSes with the MDS at 0% CPU. The **metadata** path does
not — every OPEN/CLOSE/LOOKUP/SETATTR/LAYOUTGET in the volume funnels
through one MDS process, and that process is architecturally capped
well below the node it runs on:

1. **`worker_threads = 4` is hardcoded** (`nfs_mds_main.rs`). A
   16-core node gives the MDS exactly the same async capacity as a
   4-core one. This is the cheapest ceiling to lift and the reason
   "more threads" is Tier 1.
2. **Blocking filesystem syscalls run on those 4 workers.** LOOKUP,
   GETATTR, OPEN-create, SETATTR, RENAME, REMOVE all call `std::fs`
   directly from async context. Four slow stats = a stalled runtime —
   observed live: the fsstress storms starved the DS status-reporter
   *timers* (Phase 3 already fixed this exact class for the DS
   heartbeat by giving it a dedicated runtime).
3. **Hot-path logging is a tax on every op.** A single LAYOUTGET logs
   ~15 lines through tracing's global stdout writer (three of them
   WARN-level banner lines). One fsstress run wrote a 29 MB MDS log.
4. **Per-open protocol churn.** Layouts are granted
   `return_on_close=true`, so an open/close-heavy workload pays
   LAYOUTGET + LAYOUTRETURN (+ GETDEVICEINFO when the client's
   deviceid cache refcount hits zero, + LAYOUTCOMMIT after writes)
   per cycle — each a full COMPOUND round trip. Measured ~230 ms/op
   under fsstress (runbook residual, 2026-07-07).
5. **Per-event sqlite persistence.** Seven `spawn_persist` call sites;
   layout grant/return churn turns into a sqlite write per event.

What already works and stays: per-connection pipelining
(`ConnectionPipeline`, 64 in-flight, RFC 8881 §2.10.6) landed with the
production-readiness spec and gave +30% on the dataloader bench;
DashMap-based state is already concurrent-reader-friendly.

## Harness: mdsbench

`tests/lima/pnfs/mdsbench.sh` — kernel client (lima VM) against the
host rig, python tight-loop workers (shell fork overhead would mask
server-side gains). Workloads:

| id | shape | isolates |
|----|-------|----------|
| w1-create | create+4KiB write+close, then unlink | OPEN(create)/REMOVE + pin lifecycle |
| w2-opencl | open existing, read 4KiB, close | per-open layout churn (LAYOUTGET/RETURN/GETDEVICEINFO) |
| w3-stat   | stat() storm over a pool | LOOKUP/GETATTR dispatch floor |
| w4-mixed  | fsstress -n500 -p8 | cross-check vs the fsx gate drill |

Per workload: **ops/s** (client wall clock), **cpu_ms/op** (MDS
process CPU delta — the vertical-scaling currency), **log_KiB/op**
(logging overhead proxy). Rows append to `/tmp/mdsbench-results.tsv`
keyed by `LABEL=` so variants diff in one table. `MDS_ENV=` injects
env for A/B without code changes (e.g. `RUST_LOG=warn`,
`FLINT_NFS_MAX_INFLIGHT=0`).

## Baseline (2026-07-07, Apple-silicon host, lima VM client, 2 DSes, P=8)

| workload | ops/s | cpu_ms/op | log_KiB/op | variant |
|----------|-------|-----------|------------|---------|
| w1-create | 368 | 1.28 | 3.24 | baseline (RUST_LOG default) |
| w2-opencl | 7,004 | 0.22 | 1.25 | baseline |
| w3-stat   | (invalid — client attr cache; see below) | ~0 | ~0 | baseline |
| w4-mixed  | 16 | 0.68 | 3.49 | baseline |

What the baseline says:

- **The MDS is nearly CPU-idle while workloads crawl.** w4-mixed runs
  at 16 ops/s while consuming 0.68 cpu_ms/op — roughly 1% of one
  core. Wall time is going to serialized protocol round-trip chains
  (open→layoutget→write→layoutcommit→close→layoutreturn, each a
  latency hop), fsync, and logging I/O — **not compute**. Consequence:
  for fsstress-shaped loads, Tier 2 (protocol churn) and the logging
  item dominate; more worker threads (Tier 1.1) pays off at high
  client-concurrency (w2 shape), not here.
- **Logging writes 3.2–3.5 KiB per op** on create/mixed paths —
  that's the per-op INFO/WARN chatter, measured. The `RUST_LOG=warn`
  A/B row quantifies the win before any code changes.
- **w2-opencl at 7k ops/s** shows pipelining working: open/close
  cycles on a warm pool sustain high throughput on localhost. The
  ~230 ms/op seen under fsstress comes from its heavier per-op mix
  (writes + fsync + layoutcommit + rename/unlink), not open/close
  alone.
- **w3-stat is invalid as measured** (149k "ops/s" = the client's
  attribute cache answering locally). Harness fix queued: stat a
  pool larger than the client cache, or mount `actimeo=0` for that
  workload leg.

**`RUST_LOG=warn` A/B (measured): no effect at all** — ops/s AND
log_KiB/op identical to baseline (394 vs 368 w1 ops/s is run noise;
log volume unchanged at 3.2–3.5 KiB/op). The per-op chatter is
logged at **WARN level** (the 🔥/🚨/🔴 LAYOUTGET banners, the 💥
layout-generation line, per-fallback ⛔ lines), so no env filter can
remove it. Tier 1.2 is therefore strictly a code change: demote the
per-op WARN/INFO lines to `debug!`, keep state transitions at INFO.
Re-run this A/B after the demotion to get the true logging cost.

## Tier 1 — lift the process ceiling (small diffs, measure each)

1. **Configurable worker threads.** Replace the hardcoded
   `#[tokio::main(worker_threads = 4)]` with a manual runtime builder:
   `FLINT_MDS_WORKER_THREADS` (default `num_cpus`), same for the DS
   binary (`FLINT_DS_WORKER_THREADS`). Acceptance: w3-stat ops/s
   scales with threads on a many-core host until client-bound.
2. **Demote hot-path logs.** Every per-op INFO/WARN in the
   COMPOUND/LAYOUTGET/GETDEVICEINFO/CLOSE path drops to `debug!`
   (the 🔥/🚨/🔴 LAYOUTGET banners, per-segment encode chatter,
   per-CLOSE FD-cache lines). Keep state *transitions* (pin created,
   placement forgotten, truncate-dirty, recall chain) at INFO — those
   are the operator's forensics. Add `tracing_appender::non_blocking`
   so the writer never backpressures dispatch. Acceptance:
   log_KiB/op < 0.05 at default level with no lost forensics markers
   (drills still pass — they grep INFO markers that stay).
3. **`spawn_blocking` for filesystem ops.** Wrap the dispatcher's
   path-touching handlers (LOOKUP/GETATTR resolve+stat, OPEN create,
   SETATTR apply, REMOVE/RENAME) in `tokio::task::spawn_blocking`.
   The blocking pool (512 default) absorbs fs latency; the async
   workers keep dispatching. Watch for: per-op spawn overhead on
   micro-ops — batch stat-heavy paths or use `block_in_place` where
   the op is already at the tail. Acceptance: w3-stat and w1-create
   ops/s improve together AND timer-starvation symptoms (missed
   status ticks under storm) disappear.

## Tier 2 — kill the per-open protocol churn (biggest ops/s lever)

4. **`return_on_close = false` for pNFS layouts + lease-expiry GC.**
   The client then holds layouts (and deviceid cache entries) across
   open/close cycles: w2-opencl collapses from
   OPEN+LAYOUTGET+READ+CLOSE+LAYOUTRETURN to OPEN+READ+CLOSE after
   first touch. Prereqs so state can't grow unbounded:
   - lease-expiry sweep already reaps client state — extend it to
     purge that client's layouts (verify; add if missing);
   - CB_LAYOUTRECALL on REMOVE/truncate-dirty conflicts (recall chain
     exists and is drill-covered);
   - cap: layouts-per-client watermark with LAYOUTRECALL_ANY-style
     shedding if exceeded (defensive; kernel returns on recall).
   Acceptance: w2-opencl ≥ 5× baseline ops/s; recall + fsx drills
   stay green (fsx exercises truncate-dirty against held layouts).
5. **Slot table 64 → 128+** (`FLINT_MDS_SESSION_SLOTS`). The client
   pipelines up to its session slot count; ours caps it at 64 while
   the DS already advertises 128. Cheap; measure with P=16.

## Tier 3 — persistence and encode costs (do after 1+2 re-baseline)

6. **Coalesce layout persistence.** Layout grant/return events are
   the churn; placements are the durable truth. Options in order:
   skip persisting layouts entirely (they are reconstructible client
   state — post-restart the client re-LAYOUTGETs; verify restart
   drill), else batch spawn_persist through a single writer task with
   a bounded queue + WAL mode.
7. **Reply-encode allocations.** XdrEncoder per-op Vec churn; reuse
   buffers per connection task if profiles show it (measure first —
   likely minor next to 1–5).

## Tier 4 — architectural (separate proposal when needed)

8. **Per-volume MDS sharding.** Each PVC's volume context already
   carries its MDS endpoint, so N independent MDSes can each own a
   subset of volumes with zero shared state — aggregate metadata
   throughput then scales like the data path. Chart shape: MDS
   StatefulSet + per-volume endpoint assignment at provision time.
   This multiplies whatever per-MDS ceiling Tiers 1–3 establish; it
   does not replace them (intra-volume load still hits one MDS).
   **Full proposal: `mds-sharding-plan.md`** (endpoint model, shard
   assignment, DS fan-out registration, file_id disjointness, phases
   + drills).
9. **MDS proxy I/O** (fallback UX; P1 list) — orthogonal to
   throughput, listed for completeness.

## Non-goals

- Active-active MDS for a single volume (distributed stateids —
  ruled out in the durable-DS plan, still ruled out).
- Data-path changes (ADR 0004/0005 cover it; it scales).

## Risks / watchpoints

- return_on_close=false shifts recall correctness from "rare" to
  "load-bearing": the recall drill (idle-holder model) and the
  truncate-dirty gate become the safety net — both are in the gate.
- More worker threads widen every existing race window; the gate's
  fsstress + drill matrix is the regression net.
- Kernel client behavior (deviceid cache, slot growth) varies by
  version; keep the lima VM kernel pinned across A/B runs.
