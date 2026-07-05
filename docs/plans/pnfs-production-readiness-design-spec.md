---
title: "Flint pNFS: RPC Pipelining for Per-Connection Throughput"
status: implemented
type: design-impl-spec
jira: AWC-1808
tags: [pnfs, flint, performance, nfs]
created: 2026-05-13
updated: 2026-07-05
governs:
  - spdk-csi-driver/src/nfs/server_v4.rs
  - spdk-csi-driver/src/nfs/pipeline.rs
  - spdk-csi-driver/src/pnfs/mds/server.rs
  - spdk-csi-driver/src/pnfs/ds/server.rs
  - spdk-csi-driver/src/nfs/v4/dispatcher.rs
---

> **Implementation note (2026-07-05).** Landed as Phase 2 of
> `docs/plans/pnfs-performance-plan.md`, with two deliberate deltas
> from the design below:
> 1. **Semaphore instead of mpsc + writer task.** The spec's own "key
>    insight" holds: `BackChannelWriter`'s mutex already serializes
>    frames (I1/I4), so replies are written directly from the spawned
>    dispatch task and the dedicated writer task/oneshot plumbing is
>    unnecessary. Backpressure comes from a per-connection semaphore
>    the read loop awaits (`FLINT_NFS_MAX_INFLIGHT`, default 64 = B1;
>    0 = sequential fallback, I5). Bounded read-ahead beyond the
>    dispatch bound (B2's extra queue) was dropped — it only added
>    buffered memory, not throughput.
> 2. **The DS is IN scope** (anti-scope item 4 reversed): the DS had
>    the same serial loop, and its inline blocking file I/O moved to
>    `spawn_blocking` so concurrent dispatches can't stall the
>    runtime. Measured on the 4k data path (see performance plan).
>
> Config is the `FLINT_NFS_MAX_INFLIGHT` env var rather than a yaml
> `server.pipeline` section — the standalone server is flag/env
> configured, and one knob serves all three roles.

## TL;DR

Remove the per-connection sequential RPC dispatch bottleneck in `server_v4.rs`. Today each TCP connection processes one RPC at a time (read → await dispatch → write reply → next read). NFSv4.1 slot tables explicitly permit concurrent in-flight requests per session. Pipelining lifts the single-server throughput ceiling by an estimated 30-60%.

This is the last major performance item identified in ADR 0003. CB_LAYOUTRECALL (Phase A) and State Persistence (Phase B) are already implemented and shipping. RPC pipelining is independent of both — it improves the single-server NFS path (the baseline and the MDS metadata path in pNFS mode) without touching the pNFS data-plane or persistence code.

## Context and Background

ADR 0003 measured a 1.6× pNFS write win over single-server NFS and identified the root cause: `handle_tcp_connection` (server_v4.rs:148) processes RPCs strictly sequentially per TCP connection. With Linux's `nconnect=4`, single-server NFS has only 4 parallel RPC slots server-side (one per connection). pNFS gets 2× that by sharding data across 2 DSes.

The sequential loop:

```rust
loop {
    // Read RPC record marker (4 bytes)
    reader.read_exact(&mut marker_buf).await?;
    // ... read frame body ...
    let reply = dispatch_nfsv4(request, ...).await;  // ← blocks next read
    bcw.send_record(reply).await?;
}
```

RFC 8881 §2.10.6 explicitly permits per-session pipelining via slot tables. The kernel NFS client already supports this — it sends multiple requests in parallel over the same connection when `max_session_slots > 1` (default 64 on Linux). But our server processes them one at a time, wasting the parallelism the client offers.

Pipelining would let a single `flint-nfs-server` (or single MDS) saturate available CPU/disk bandwidth. ADR 0003 estimated this is a ~1 week refactor.

## Problem Statement

The single-server NFS path (which is also the metadata path for pNFS operations like OPEN, GETATTR, LOOKUP, LAYOUTGET) has an artificial throughput ceiling caused by per-connection head-of-line blocking. A slow RPC (e.g., a WRITE with fsync) delays all subsequent RPCs on the same connection, even if they're independent GETATTR operations that would complete in microseconds.

This affects:
- Single-server mode throughput (the fallback and baseline comparison)
- MDS metadata operation latency in pNFS mode
- Overall server utilization under concurrent client workloads

## Invariants

| ID | Name | Rule | Violated When |
|----|------|------|---------------|
| I1 | Wire Frame Integrity | ONC RPC record frames on a single TCP connection never interleave, even under concurrent dispatch | Two response frames are partially written (e.g., frame A's marker + first N bytes followed by frame B's marker) on the same connection |
| I2 | Slot Isolation | A request on slot S does not block processing of a request on slot T (S ≠ T) within the same session | Client observes head-of-line blocking: slot 0's slow WRITE delays slot 1's fast GETATTR response |
| I3 | Replay Exactly-Once | NFSv4.1 slot-table replay semantics (identical sequence on same slot returns cached reply without re-dispatch) are preserved under pipelining | A retransmitted COMPOUND on slot S gets dispatched to the handler instead of returning the cached reply |
| I4 | Back-Channel Coexistence | CB_LAYOUTRECALL and CB_RECALL frames interleave correctly with forward-channel replies on shared connections | A CB frame is queued behind a stalled forward reply; or a forward reply corrupts a mid-send CB frame |
| I5 | Graceful Degradation | If pipelining is disabled (`max_slots = 0`), the server falls back to sequential processing without behavior change | Setting max_slots=0 causes a panic, deadlock, or altered semantics |

## Bounds

| ID | Name | Limit | Rationale |
|----|------|-------|-----------|
| B1 | Max Concurrent Slots Per Connection | 64 | Matches Linux kernel NFS client's default `max_session_slots`. Memory cost is ~8KB per in-flight request context. Beyond 64, returns diminish and memory grows linearly. |
| B2 | Dispatch Queue Depth | 128 per connection | Max buffered-but-not-yet-dispatched requests. Prevents OOM from a misbehaving client flooding requests faster than dispatch can process. Backpressure: pause reading from TCP until queue drains below half. |
| B3 | Per-Request Memory Cap | 4 MiB | Already enforced (server_v4.rs:235). Pipelining with B2=128 queued requests × 4MiB each = 512MiB theoretical max per connection. Acceptable given connections are bounded. |
| B4 | Throughput Improvement | ≥ 30% | Single-server `numjobs=4, fsync=1, bs=1M` must improve from ~165 MiB/s to ≥ 220 MiB/s. If not achieved, the refactor hasn't solved the bottleneck. |

## Design

### Data Model

```rust
/// Per-connection pipelining state. Created once per accepted TCP
/// connection; lives alongside the existing BackChannelWriter.
struct ConnectionPipeline {
    /// Reader task sends decoded requests here. Bounded by B2.
    request_tx: mpsc::Sender<PipelinedRequest>,
    request_rx: mpsc::Receiver<PipelinedRequest>,

    /// Tracks in-flight count for backpressure signaling.
    inflight: Arc<AtomicU32>,
    max_inflight: u32,  // B2 = 128
}

struct PipelinedRequest {
    xid: u32,
    payload: Bytes,
    /// Writer task awaits this to get the reply bytes.
    reply_tx: oneshot::Sender<Bytes>,
}
```

The slot-table replay cache already exists in the `SessionManager` (via `SlotEntry` in `session.rs`). Pipelining does NOT need a new per-connection replay cache — the existing per-session slot table handles replay detection at the `SEQUENCE` operation level (the first op in every COMPOUND). The dispatcher already checks slot sequence numbers and returns cached replies for replays.

### Component Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│  Per-TCP-Connection (replaces current sequential loop)               │
│                                                                      │
│  ┌────────────┐        ┌──────────────────┐                         │
│  │ Reader Task│───────▶│ mpsc channel     │                         │
│  │ (read RPC  │        │ (bounded, B2=128)│                         │
│  │  frames)   │        └────────┬─────────┘                         │
│  └────────────┘                 │                                    │
│       │                         ▼                                    │
│       │ CB replies    ┌──────────────────────────────┐               │
│       │ (msg_type=1)  │ Dispatcher Loop              │               │
│       │               │  for each request:           │               │
│       │               │    tokio::spawn {            │               │
│       │               │      reply = dispatch(req)   │               │
│       │               │      reply_tx.send(reply)    │               │
│       │               │    }                         │               │
│       │               └──────────────────────────────┘               │
│       │                              │ oneshot reply                  │
│       │                              ▼                                │
│       │               ┌──────────────────────────────┐               │
│       └──────────────▶│ Writer Task                  │               │
│                       │  • select! over:             │               │
│                       │    - reply oneshots (fwd)    │               │
│                       │    - CB send requests        │               │
│                       │  • bcw.send_record(bytes)    │               │
│                       │    (mutex serializes I1)     │               │
│                       └──────────────────────────────┘               │
└──────────────────────────────────────────────────────────────────────┘
```

**Key insight:** The existing `BackChannelWriter` already serializes all writes through a `tokio::sync::Mutex`. The writer task doesn't need a new lock — it sends forward replies and CB frames through the same `bcw.send_record()` path that already guarantees I1 and I4.

### Sequence / Flow

#### Normal pipelined flow

```
Linux Client          Reader Task       Dispatcher Tasks      Writer Task
    │                     │                    │                    │
    ├─ RPC(xid=1,s=0) ──▶│                    │                    │
    ├─ RPC(xid=2,s=1) ──▶│                    │                    │
    ├─ RPC(xid=3,s=2) ──▶│                    │                    │
    │                     ├── req(xid=1) ─────▶│ spawn              │
    │                     ├── req(xid=2) ─────▶│ spawn              │
    │                     ├── req(xid=3) ─────▶│ spawn              │
    │                     │                    │                    │
    │                     │          xid=2 done (fast GETATTR)      │
    │                     │                    ├── reply(xid=2) ───▶│
    │                     │                    │                    ├─ frame ──▶ Client
    │                     │          xid=3 done                     │
    │                     │                    ├── reply(xid=3) ───▶│
    │                     │                    │                    ├─ frame ──▶ Client
    │                     │          xid=1 done (slow WRITE)        │
    │                     │                    ├── reply(xid=1) ───▶│
    │                     │                    │                    ├─ frame ──▶ Client
```

Replies arrive in completion order, not submission order. This is correct per RFC 5531 — the client matches by xid.

#### Backpressure flow

```
Client (flooding)     Reader Task       Channel (full)
    │                     │                │
    ├─ RPC ──────────────▶│ send ─────────▶│ (queue: 127/128)
    ├─ RPC ──────────────▶│ send ─────────▶│ (queue: 128/128 — FULL)
    ├─ RPC ──────────────▶│ send ─ blocks ─│ (reader paused)
    │                     │                │
    │                     │   ... dispatch completes, queue drains ...
    │                     │                │
    │                     │ send resumes ──▶│ (queue: 64/128)
```

The bounded channel provides natural TCP backpressure: when the channel is full, the reader task blocks on `send().await`, which stops reading from the socket, which fills the TCP receive buffer, which triggers TCP flow control to the client.

#### Replay detection (unchanged)

The existing SEQUENCE operation handler in `dispatcher.rs` already does:
1. Look up slot by `sa_slotid`
2. If `sa_sequenceid == slot.last_sequence` → return cached reply (replay)
3. If `sa_sequenceid == slot.last_sequence + 1` → proceed (new request)
4. Otherwise → `NFS4ERR_SEQ_MISORDERED`

This logic runs inside the spawned task, not in the reader. Under pipelining, two requests on the same slot with the same sequence race — but the client never does this (it's a protocol violation). If it did, both would match case 2 and return the cached reply (idempotent).

## API Surface

| Method | Path / Signature | Input | Output | Errors |
|--------|-----------------|-------|--------|--------|
| (config) | `server.pipeline.enabled` | `bool` (default `true`) | — | — |
| (config) | `server.pipeline.max_inflight` | `u32` (default 128) | — | 0 → sequential fallback (I5) |
| (internal) | `ConnectionPipeline::new(max_inflight: u32)` | Queue depth bound | Pipeline state | — |
| (internal) | `run_reader(reader, pipeline, bcw)` | TCP read half + pipeline + BCW for CB reply routing | Never returns (runs until EOF/error) | io::Error on read failure |
| (internal) | `run_dispatcher(pipeline, dispatcher, bcw)` | Pipeline + CompoundDispatcher + BCW | Never returns | — |

No new external/NFS protocol surface. The change is entirely internal to the server's connection handling.

## Anti-Scope

- Does not implement per-connection reply ordering. Replies go out in completion order (RFC-compliant). Ordering would re-introduce HOL blocking.
- Does not change the NFSv4.1 session/slot negotiation in CREATE_SESSION. The server already accepts the client's `ca_maxrequests` (up to 64); pipelining just honors it.
- Does not implement request prioritization (e.g., GETATTR before WRITE). All requests dispatch at equal priority. Priority would add complexity for minimal gain.
- Does not touch the pNFS data-server (DS) code path. DS connections are already fast (simple WRITE/READ, no COMPOUND overhead).
- Does not change the `BackChannelWriter` or callback infrastructure. Those already work correctly with shared connections.
- Does not implement NFS RDMA or multi-stream TCP.

## Alternatives Considered

| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| `tokio::spawn` per request with bounded channel | Simple; uses existing runtime; natural backpressure via channel; each request gets independent progress | Must collect replies via oneshot channels; spawns many small tasks (acceptable for Tokio) | **Chosen**: minimal structural change, proven async pattern |
| `FuturesUnordered` poll loop | Avoids per-request spawn overhead; single task drives all in-flight requests | More complex lifetime management; doesn't scale as cleanly with varying dispatch latency; harder to add per-request timeouts | Rejected: marginal perf gain not worth complexity |
| Thread pool + crossbeam queue | Avoids Tokio task overhead | Requires bridging async I/O and sync dispatch; adds crossbeam dependency; doesn't integrate with existing async code | Rejected: fights the architecture |
| Increase `nconnect` instead | Zero server-side changes; client opens more TCP connections | Kernel default is already 4; increasing beyond 8 has diminishing returns and increases connection-tracking overhead; doesn't help metadata ops | Rejected: works around the problem rather than fixing it |
| Keep sequential; rely on pNFS for throughput | No risk of introducing bugs | Single-server is the fallback mode; MDS metadata path always goes through the sequential loop even in pNFS; leaves known performance on the table | Rejected: identified in ADR 0003 as worth fixing |

## Implementation Phases

### Phase 1: Reader/Writer Task Split

**Scope:** Split `handle_tcp_connection` into three cooperating tasks: reader, dispatcher, writer. Use a bounded `mpsc` channel between reader and dispatcher. Use `oneshot` channels for reply delivery to the writer. Keep sequential dispatch within the dispatcher task initially (process channel items one at a time) — this validates the structural refactor without changing concurrency semantics.

**Files:**
- `spdk-csi-driver/src/nfs/server_v4.rs` — refactor `handle_tcp_connection`
- New: `spdk-csi-driver/src/nfs/pipeline.rs` — `ConnectionPipeline`, `PipelinedRequest`, task entry points

**Depends on:** Nothing.

**Exit criteria:**
- All existing tests pass (pynfs ≥ 153, `make test-pnfs-smoke`)
- `bench-sweep.sh` shows no regression (still ~165 MiB/s for numjobs=4 fsync=1)
- CB_LAYOUTRECALL still fires correctly on DS death (existing `make test-pnfs-recall`)
- Structural: the three tasks are visible as separate `tokio::spawn` calls

### Phase 2: Parallel Dispatch

**Scope:** Change the dispatcher task from sequential processing to spawning a new task per request. Add backpressure via `AtomicU32` inflight counter. This is where the concurrency win lands.

**Files:**
- `spdk-csi-driver/src/nfs/pipeline.rs` — dispatcher loop spawns per-request tasks
- `spdk-csi-driver/src/nfs/server_v4.rs` — pass inflight counter; add config for max_inflight

**Depends on:** Phase 1.

**Exit criteria:**
- pynfs ≥ 153 (no protocol regression)
- New test: two requests on different slots complete concurrently (I2)
- New test: replay on same slot returns cached reply (I3, exercised via dispatcher re-entry)
- `bench-sweep.sh` single-server `numjobs=4 fsync=1` ≥ 220 MiB/s (B4)
- No interleaved frames (I1) — verified by racing 100 requests in a unit test

### Phase 3: Backpressure and Sequential Fallback

**Scope:** Add the `max_inflight=0 → sequential` fallback path (I5). Add config parsing. Add metrics (optional: in-flight gauge, dispatch latency histogram).

**Files:**
- `spdk-csi-driver/src/nfs/pipeline.rs` — fallback path
- `spdk-csi-driver/src/nfs/server_v4.rs` — config integration
- `config/pnfs.example.yaml` — document `server.pipeline` section

**Depends on:** Phase 2.

**Exit criteria:**
- `max_inflight=0` passes all tests with identical behavior to pre-pipelining code
- Config documented
- No performance regression in the sequential fallback vs the original code

## Test Scenarios

| ID | Name | Tests | Layer |
|----|------|-------|-------|
| T1 | Concurrent Slot Dispatch | I2, B1 | [unit] |
| T2 | Frame Integrity Under Load | I1, B2 | [unit] |
| T3 | Slot Replay Preserved | I3 | [unit] |
| T4 | Backpressure Activation | B2 | [unit] |
| T5 | CB Frame Coexistence | I4 | [integration] |
| T6 | Sequential Fallback | I5 | [unit] |
| T7 | Pipelined Throughput | I2, B4 | [e2e] |
| T8 | Pynfs Conformance Preserved | I3 | [e2e] |

### T1: Concurrent Slot Dispatch

**Tests:** I2, B1

**Setup:** In-process test. Create a `ConnectionPipeline` with `max_inflight=64`. Register a mock dispatcher that sleeps 100ms for slot 0 and returns immediately for slot 1.

**Action:** Submit two requests simultaneously: slot 0 (slow) and slot 1 (fast).

**Assert:** Slot 1's reply arrives before slot 0's reply. Both replies are correct. Total wall-clock time is ~100ms, not ~200ms.

**Why this matters:** Without pipelining, slot 1 is blocked behind slot 0's 100ms sleep. This is the core behavior change that eliminates head-of-line blocking.

### T2: Frame Integrity Under Load

**Tests:** I1, B2

**Setup:** In-process test. 4 concurrent producer tasks each sending 100 requests over the same pipeline. Mock dispatcher returns variable-length replies (1KB to 64KB, randomized).

**Action:** All 400 requests complete.

**Assert:** Capture all bytes written to the mock TCP writer. Parse as ONC RPC record-marked frames. Every frame has a valid 4-byte marker (`0x80000000 | length`), the length field matches the actual payload size, and no frame's bytes appear spliced into another frame's payload.

**Why this matters:** Frame interleaving would cause every connected NFS client to see protocol errors or data corruption. This is the most critical safety property of the refactor.

### T3: Slot Replay Preserved

**Tests:** I3

**Setup:** In-process test with a pipelined dispatcher. Dispatch a COMPOUND containing SEQUENCE(slot=3, seq=7) + GETATTR. Record the reply.

**Action:** Submit an identical COMPOUND: same slot=3, same seq=7.

**Assert:** The dispatcher's GETATTR handler is NOT invoked a second time. The returned reply bytes are byte-identical to the first reply. The slot's `last_sequence` is still 7.

**Why this matters:** NFSv4.1 clients retransmit on timeout. If pipelining breaks the slot-table replay cache, duplicate OPENs could create phantom stateids, or duplicate WRITEs could apply twice.

### T4: Backpressure Activation

**Tests:** B2

**Setup:** Unit test. Create a pipeline with `max_inflight=4`. Register a dispatcher that never completes (blocks forever via a channel that never sends).

**Action:** Submit 4 requests (fills the channel). Attempt to submit a 5th.

**Assert:** The 5th submission blocks (`send().await` does not resolve). The reader task would be suspended at this point. No panic, no OOM, no unbounded growth.

**Why this matters:** A misbehaving client flooding requests must not exhaust server memory. Backpressure is the bound that prevents resource exhaustion under adversarial load.

### T5: CB Frame Coexistence

**Tests:** I4

**Setup:** Integration test. Start a pipelined server connection with a registered back-channel. Concurrently: (a) the client sends a slow WRITE (takes 200ms), and (b) the MDS fires CB_LAYOUTRECALL through the same connection's writer.

**Action:** Both the forward reply and the CB frame are sent.

**Assert:** The CB frame and the WRITE reply are both present in the captured output. Neither is corrupted. The CB frame may arrive before or after the WRITE reply (ordering doesn't matter), but both are complete, valid RPC frames.

**Why this matters:** CB_LAYOUTRECALL is time-critical (it fires on DS death). If pipelining introduced a deadlock or starvation where CB frames can't get through while forward replies are queued, layout recall would be delayed — defeating the purpose of Phase A work.

### T6: Sequential Fallback

**Tests:** I5

**Setup:** Unit test. Create pipeline with `max_inflight=0`.

**Action:** Submit 3 requests sequentially.

**Assert:** Each request is dispatched and its reply sent before the next request is read from the channel. Behavior is identical to pre-pipelining sequential loop. No panic, no error from the `0` configuration.

**Why this matters:** Operators need a way to disable pipelining without code changes (for debugging, for bisecting performance regressions, or on systems where the concurrent dispatch triggers a latent bug).

### T7: Pipelined Throughput

**Tests:** I2, B4

**Setup:** Lima e2e. Same `bench-sweep.sh` harness as ADR 0003. Single-server mode, `numjobs=4, fsync=1, bs=1M`.

**Action:** Run fio benchmark.

**Assert:** Sequential write throughput ≥ 220 MiB/s (vs 165 MiB/s baseline = ≥ 33% improvement).

**Why this matters:** The entire purpose of this work is measurable throughput improvement. If the numbers don't move, the bottleneck has shifted elsewhere and the refactor needs investigation.

### T8: Pynfs Conformance Preserved

**Tests:** I3

**Setup:** Lima e2e. Run `make test-nfs-protocol` (pynfs suite) against the pipelined server.

**Action:** Full pynfs run.

**Assert:** Score ≥ 153 pass. No regressions from previous baseline. Any new failures are investigated and resolved before the phase exits.

**Why this matters:** Pipelining changes when and how COMPOUNDs are dispatched. Subtle protocol violations (wrong sequence tracking, missed replays, out-of-order side effects) would show up as pynfs failures.

## Traceability Matrix

| Invariant/Bound | Verified By |
|-----------------|-------------|
| I1 Wire Frame Integrity | T2 |
| I2 Slot Isolation | T1, T7 |
| I3 Replay Exactly-Once | T3, T8 |
| I4 Back-Channel Coexistence | T5 |
| I5 Graceful Degradation | T6 |
| B1 Max Concurrent Slots | T1 |
| B2 Dispatch Queue Depth | T2, T4 |
| B3 Per-Request Memory Cap | (existing enforcement, unchanged) |
| B4 Throughput Improvement | T7 |

## Governed Files

- `spdk-csi-driver/src/nfs/server_v4.rs` — connection handler refactored to reader/writer/dispatcher tasks
- `spdk-csi-driver/src/nfs/pipeline.rs` (new) — ConnectionPipeline, PipelinedRequest, backpressure logic, task entry points
- `spdk-csi-driver/src/nfs/v4/dispatcher.rs` — no changes expected (replay handled at SEQUENCE op level, which is already correct)
- `spdk-csi-driver/src/nfs/mod.rs` — add `pub mod pipeline;`
- `config/pnfs.example.yaml` — document `server.pipeline` section
- `tests/lima/pnfs/bench-sweep.sh` — performance regression gate (no changes, just re-run)

## Open Questions

1. **Reply ordering guarantee** — Should we guarantee that replies within the same slot are ordered? Option A: No ordering (completion order; client matches by xid). Option B: Per-slot FIFO (replies on slot S go out in request order). Recommendation: Option A. Per-slot FIFO would require buffering later-completing replies until earlier ones finish — re-introducing partial HOL blocking. The client already handles out-of-order replies by xid matching (RFC 5531 §9).

2. **Task spawn overhead** — Is `tokio::spawn` per RPC too expensive for small operations (GETATTR ~ 5μs dispatch time)? Option A: Always spawn. Option B: Spawn only if a previous request on this connection is still in-flight; otherwise dispatch inline. Recommendation: Option A for initial implementation. Tokio task spawn is ~200ns; measurable only at millions of ops/sec. Optimize to Option B only if profiling shows spawn overhead > 5% of dispatch time.

3. **Connection-level timeout for stalled dispatches** — Should there be a timeout per spawned task to prevent a leaked task from holding an inflight slot forever? Option A: No timeout (rely on NFS-level operation timeouts inside handlers). Option B: 60s per-task timeout; drop the reply_tx on expiry. Recommendation: Option B as a safety net. A leaked task that holds an inflight slot reduces effective parallelism; 60s is generous enough to never fire on legitimate requests.

## Exit Criteria

- [ ] All invariants (I1–I5) have at least one passing test
- [ ] All bounds (B1–B4) have at least one passing test at boundary
- [ ] `make test-nfs-protocol` (pynfs) score ≥ 153 pass (no regression)
- [ ] `make test-pnfs-smoke` passes
- [ ] `make test-pnfs-recall` still passes (CB_LAYOUTRECALL unbroken)
- [ ] `bench-sweep.sh` single-server `numjobs=4 fsync=1` ≥ 220 MiB/s (B4)
- [ ] Sequential fallback (`max_inflight=0`) passes all tests identically
- [ ] This spec's traceability matrix has no invariant/bound with zero tests
