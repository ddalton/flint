//! Per-connection RPC pipelining.
//!
//! Spec: `docs/plans/pnfs-production-readiness-design-spec.md`
//! (invariants I1–I5, bounds B1–B4).
//!
//! NFSv4.1 sessions explicitly permit multiple in-flight requests per
//! connection (RFC 8881 §2.10.6, slot tables); the Linux client sends
//! up to `max_session_slots` (default 64) concurrent requests. The
//! historical server loop processed one RPC at a time per connection
//! (read → dispatch → write → next read), so one slow WRITE-with-fsync
//! head-of-line blocked every GETATTR queued behind it.
//!
//! `ConnectionPipeline` removes that: the connection's read loop calls
//! [`ConnectionPipeline::submit`] per decoded frame, which spawns the
//! dispatch+reply as its own task, bounded by a semaphore. When all
//! permits are in use, `submit` blocks — the read loop stops consuming
//! the socket, the TCP receive window fills, and the client is
//! flow-controlled (bound B2's backpressure, sized by B1).
//!
//! Invariants and where they're upheld:
//! - **I1 (wire frame integrity)**: replies are written through the
//!   caller-supplied `write` closure, which every server routes to a
//!   mutex-serialized writer (`BackChannelWriter::send_record`, or the
//!   DS's mutexed writer). Frames never interleave.
//! - **I2 (slot isolation)**: each request dispatches in its own task;
//!   a slow request no longer delays an independent one.
//! - **I3 (replay exactly-once)**: unchanged — the per-session slot
//!   table in `session.rs` detects replays at the SEQUENCE op inside
//!   dispatch, wherever that dispatch runs.
//! - **I4 (back-channel coexistence)**: CB frames share the same
//!   serialized writer; inbound CB replies are routed by the read
//!   loop before `submit` is ever called.
//! - **I5 (graceful degradation)**: `max_inflight == 0` dispatches
//!   inline and awaits the write before returning — byte-for-byte the
//!   old sequential loop.
//!
//! Replies go out in completion order, not arrival order; RPC clients
//! match replies by xid (RFC 5531 §9), and per-slot FIFO would just
//! re-introduce head-of-line blocking (spec, open question 1).

use bytes::Bytes;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::warn;

/// F55 (runam 2026-07-28): a SIGTERM that exits while a reply is mid-write
/// truncates the RPC frame on the wire, and the client turns that into an
/// instant EIO on the in-flight fsync/COMMIT — postgres PANICs (fsyncgate)
/// and its abort wedges on the dying mount. Dropping connections is only
/// safe BETWEEN complete frames: an unreplied request is retransmitted by
/// the client against the next instance; a truncated reply is poison.
///
/// The gate makes shutdown frame-atomic: `begin()` stops new dispatches
/// (submit refuses; read loops close at their next frame boundary), and
/// `drain()` waits — bounded — for every reply already past dispatch to
/// finish flushing. The bound matters more than completeness: the F33b
/// prompt-exit obligation (lazy-umount data loss at kubelet's grace
/// deadline) caps how long shutdown may linger, so an expired deadline
/// exits anyway and reverts that one reply to pre-F55 behavior.
pub struct DrainGate {
    draining: AtomicBool,
    inflight: AtomicU64,
}

impl DrainGate {
    pub const fn new() -> Self {
        Self { draining: AtomicBool::new(false), inflight: AtomicU64::new(0) }
    }

    /// The process-wide gate every production pipeline uses.
    pub fn global() -> &'static DrainGate {
        static GATE: DrainGate = DrainGate::new();
        &GATE
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Stop admitting new dispatches. Idempotent.
    pub fn begin(&self) {
        self.draining.store(true, Ordering::Release);
    }

    /// Enter the in-flight section, or `None` once draining: the caller
    /// must then tear the connection down WITHOUT dispatching (the frame
    /// stays unreplied, which the client handles by retransmitting).
    fn enter(&'static self) -> Option<InflightReply> {
        if self.is_draining() {
            return None;
        }
        self.inflight.fetch_add(1, Ordering::AcqRel);
        // Order matters: the count is visible before the second draining
        // check, so a drain that begins between the two either sees this
        // entry or this entry sees the drain and backs out.
        if self.is_draining() {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(InflightReply { gate: self })
    }

    /// Wait for in-flight replies to reach zero, at most `deadline`.
    /// Returns `(remaining, clean)` — `clean` when everything flushed.
    pub async fn drain(&self, deadline: std::time::Duration) -> (u64, bool) {
        let start = tokio::time::Instant::now();
        loop {
            let n = self.inflight.load(Ordering::Acquire);
            if n == 0 {
                return (0, true);
            }
            if start.elapsed() >= deadline {
                return (n, false);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// RAII in-flight token: alive from just before dispatch until the reply
/// write resolves (or the task dies — the Drop keeps the count honest
/// even on panic/cancellation).
struct InflightReply {
    gate: &'static DrainGate,
}

impl Drop for InflightReply {
    fn drop(&mut self) {
        self.gate.inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Default maximum concurrently-dispatching requests per connection
/// (bound B1). Matches the Linux kernel client's default
/// `max_session_slots` (64) — the most the client will ever usefully
/// pipeline on one session. Combined with the existing 4 MiB
/// per-request cap (B3), worst case is 256 MiB in flight per
/// connection.
pub const DEFAULT_MAX_INFLIGHT: u32 = 64;

/// Environment knob: `FLINT_NFS_MAX_INFLIGHT`.
/// Unset → [`DEFAULT_MAX_INFLIGHT`]; `0` → sequential fallback (I5).
const MAX_INFLIGHT_ENV: &str = "FLINT_NFS_MAX_INFLIGHT";

/// Per-connection pipelining state. Create one per accepted TCP
/// connection; the connection's read loop calls [`submit`] per frame.
///
/// [`submit`]: ConnectionPipeline::submit
pub struct ConnectionPipeline {
    /// Permits = max concurrent dispatches. `None` in sequential mode.
    sem: Option<Arc<Semaphore>>,
    /// Permit count at rest, for the inline fast-path check.
    max_inflight: usize,
    /// Set when a spawned task's reply write fails: the connection is
    /// dead, so the read loop should stop feeding it.
    broken: Arc<AtomicBool>,
    /// F55 shutdown gate; the global one in production, leaked locals in
    /// tests so gate state never crosses concurrently-running tests.
    gate: &'static DrainGate,
}

impl ConnectionPipeline {
    /// `max_inflight == 0` selects the sequential fallback (I5).
    pub fn new(max_inflight: u32) -> Self {
        Self::with_gate(max_inflight, DrainGate::global())
    }

    pub fn with_gate(max_inflight: u32, gate: &'static DrainGate) -> Self {
        Self {
            sem: (max_inflight > 0)
                .then(|| Arc::new(Semaphore::new(max_inflight as usize))),
            max_inflight: max_inflight as usize,
            broken: Arc::new(AtomicBool::new(false)),
            gate,
        }
    }

    /// Build from `FLINT_NFS_MAX_INFLIGHT` (read per call so servers
    /// and tests can differ; connection setup is not a hot path).
    pub fn from_env() -> Self {
        let max_inflight = std::env::var(MAX_INFLIGHT_ENV)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(DEFAULT_MAX_INFLIGHT);
        Self::new(max_inflight)
    }

    /// Dispatch one request and send its reply.
    ///
    /// `more_queued` is the caller's backlog hint: `true` when the
    /// connection's read buffer already holds more input (the client
    /// is genuinely pipelining). Spawning a task per request costs
    /// real per-op latency (~10–25µs measured), which only pays off
    /// when requests actually overlap — so the request runs INLINE
    /// (the old serial loop, zero overhead) unless the client is
    /// pipelining or other dispatches from this connection are
    /// already in flight (spec, open question 2 / option B). A slow
    /// inline op can delay the reader once; the backlog that builds
    /// behind it flips the next submits back to spawning.
    ///
    /// Sequential mode (`max_inflight == 0`): always inline (I5).
    ///
    /// Returns `Err` when the connection should be torn down: an
    /// inline write failed, or an earlier spawned write failed
    /// (`broken`). Dispatch itself is infallible (`Bytes` in, reply
    /// `R` out) by construction of the RPC layer.
    ///
    /// The reply type `R` is the caller's: the pipeline only carries it
    /// from `dispatch` to `write`, so a server whose replies are a list
    /// of wire segments (the DS keeps a READ payload as its own `Bytes`
    /// instead of flattening ~1 MiB through three encoder copies) uses
    /// the same pipeline as one whose replies are a single flat buffer.
    pub async fn submit<R, D, DF, W, WF>(
        &self,
        request: Bytes,
        more_queued: bool,
        must_spawn: bool,
        dispatch: D,
        write: W,
    ) -> std::io::Result<()>
    where
        R: Send + 'static,
        D: FnOnce(Bytes) -> DF + Send + 'static,
        DF: Future<Output = R> + Send + 'static,
        W: FnOnce(R) -> WF + Send + 'static,
        WF: Future<Output = std::io::Result<()>> + Send + 'static,
    {
        if self.broken.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "connection writer failed in an earlier pipelined reply",
            ));
        }

        // F55: once draining, never START a reply — the caller tears the
        // connection down and the client retransmits this frame against
        // the next instance. (A reply already past this point finishes;
        // that is exactly what the drain waits for.)
        let Some(token) = self.gate.enter() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "server draining for shutdown",
            ));
        };

        let Some(sem) = &self.sem else {
            // I5: sequential fallback. `must_spawn` cannot be honoured
            // here — there is no semaphore to spawn under — so a
            // back-channel connection with FLINT_NFS_MAX_INFLIGHT=0
            // still self-blocks on a callback. That configuration is the
            // debugging one; the default path is covered below.
            let reply = dispatch(request).await;
            let res = write(reply).await;
            drop(token);
            return res;
        };

        // Backpressure (B2): once max_inflight dispatches are running,
        // this blocks, the read loop stops draining the socket, and
        // TCP flow control pushes back on the client.
        let permit = Arc::clone(sem)
            .acquire_owned()
            .await
            .expect("connection pipeline semaphore is never closed");

        // Inline fast path: nothing else in flight and no backlog —
        // request/response ping-pong, where task fan-out is pure
        // per-op overhead.
        // `must_spawn`: this connection is a session's back-channel, so CB
        // replies land on THIS socket and only this read loop can route
        // them. Dispatching inline would park the reader inside a compound
        // that is itself awaiting one of those replies — the call then
        // times out and the whole connection is head-of-line blocked for
        // the duration (audit R2). Pay the task fan-out instead.
        let others_in_flight = sem.available_permits() < self.max_inflight - 1;
        if !must_spawn && !more_queued && !others_in_flight {
            let reply = dispatch(request).await;
            let res = write(reply).await;
            drop(token);
            drop(permit);
            if res.is_err() {
                self.broken.store(true, Ordering::Release);
            }
            return res;
        }

        let broken = Arc::clone(&self.broken);
        tokio::spawn(async move {
            let reply = dispatch(request).await;
            if let Err(e) = write(reply).await {
                warn!("pipelined reply write failed (connection dying): {}", e);
                broken.store(true, Ordering::Release);
            }
            drop(token);
            drop(permit);
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::time::{sleep, timeout, Instant};

    fn req(tag: u8) -> Bytes {
        Bytes::from(vec![tag])
    }

    /// T1 (I2, B1): a slow request must not block a fast one — the
    /// fast reply lands first and total wall-clock is ~one sleep, not
    /// two.
    #[tokio::test]
    async fn t1_concurrent_slot_dispatch() {
        let p = ConnectionPipeline::new(64);
        let order: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let start = Instant::now();

        let o = Arc::clone(&order);
        p.submit(
            req(0),
            true,
                false,
            |r| async move {
                sleep(Duration::from_millis(100)).await;
                r
            },
            move |r| async move {
                o.lock().unwrap().push(r[0]);
                Ok(())
            },
        )
        .await
        .unwrap();

        let o = Arc::clone(&order);
        p.submit(
            req(1),
            true,
                false,
            |r| async move { r },
            move |r| async move {
                o.lock().unwrap().push(r[0]);
                Ok(())
            },
        )
        .await
        .unwrap();

        // Wait until both replies have been written.
        timeout(Duration::from_secs(2), async {
            while order.lock().unwrap().len() < 2 {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both replies must complete");

        assert_eq!(
            *order.lock().unwrap(),
            vec![1, 0],
            "fast request must complete before the slow one"
        );
        assert!(
            start.elapsed() < Duration::from_millis(190),
            "requests must overlap (~100ms total), got {:?}",
            start.elapsed()
        );
    }

    /// T2 (I1, B2): 4 concurrent producers × 100 requests with
    /// variable-size replies through the real `BackChannelWriter`
    /// over TCP. Every frame on the wire must be a complete,
    /// correctly-marked ONC RPC record with no foreign bytes spliced
    /// in.
    #[tokio::test]
    async fn t2_frame_integrity_under_load() {
        use tokio::io::{AsyncReadExt, BufWriter};
        use tokio::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server_stream, _) = listener.accept().await.unwrap();

        let (_read_half, write_half) = server_stream.into_split();
        let bcw = crate::nfs::v4::back_channel::BackChannelWriter::new(
            BufWriter::with_capacity(64 * 1024, write_half),
        );

        let p = Arc::new(ConnectionPipeline::new(64));

        fn frame_len(id: u16) -> usize {
            1024 + ((id as usize * 7919) % (63 * 1024))
        }

        let mut producers = Vec::new();
        for prod in 0..4u16 {
            let p = Arc::clone(&p);
            let bcw = Arc::clone(&bcw);
            producers.push(tokio::spawn(async move {
                for i in 0..100u16 {
                    let id = prod * 100 + i;
                    let bcw_w = Arc::clone(&bcw);
                    p.submit(
                        Bytes::from(id.to_be_bytes().to_vec()),
                        true,
                false,
                        move |r| async move {
                            let id = u16::from_be_bytes([r[0], r[1]]);
                            let mut reply = vec![(id % 251) as u8; frame_len(id)];
                            reply[0] = r[0];
                            reply[1] = r[1];
                            Bytes::from(reply)
                        },
                        move |reply| async move { bcw_w.send_record(reply).await },
                    )
                    .await
                    .unwrap();
                }
            }));
        }

        let mut rd = client;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..400 {
            let mut marker = [0u8; 4];
            rd.read_exact(&mut marker).await.unwrap();
            let m = u32::from_be_bytes(marker);
            assert!(m & 0x8000_0000 != 0, "last-fragment bit must be set");
            let len = (m & 0x7FFF_FFFF) as usize;

            let mut payload = vec![0u8; len];
            rd.read_exact(&mut payload).await.unwrap();
            let id = u16::from_be_bytes([payload[0], payload[1]]);
            assert_eq!(
                len,
                frame_len(id),
                "frame {} marker length doesn't match its payload",
                id
            );
            let fill = (id % 251) as u8;
            assert!(
                payload[2..].iter().all(|&b| b == fill),
                "frame {} contains spliced foreign bytes",
                id
            );
            assert!(seen.insert(id), "duplicate frame {}", id);
        }
        for t in producers {
            t.await.unwrap();
        }
        assert_eq!(seen.len(), 400);
    }

    /// T4 (B2): with all permits held by never-completing dispatches,
    /// the next submit must block (backpressure), not panic or grow
    /// without bound.
    #[tokio::test]
    async fn t4_backpressure_activation() {
        let p = ConnectionPipeline::new(4);

        for i in 0..4 {
            p.submit(
                req(i),
                true,
                false,
                |_| async {
                    std::future::pending::<()>().await;
                    unreachable!()
                },
                |_| async { Ok(()) },
            )
            .await
            .unwrap();
        }

        let fifth = p.submit(req(9), true, false, |r| async { r }, |_| async { Ok(()) });
        assert!(
            timeout(Duration::from_millis(100), fifth).await.is_err(),
            "5th submit must block while 4 dispatches are in flight"
        );
    }

    /// T6 (I5): max_inflight=0 must behave exactly like the old
    /// sequential loop — each request fully dispatched and written
    /// before the next submit runs.
    #[tokio::test]
    async fn t6_sequential_fallback() {
        let p = ConnectionPipeline::new(0);
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        for i in 0..3u8 {
            let l1 = Arc::clone(&log);
            let l2 = Arc::clone(&log);
            p.submit(
                req(i),
                true,
                false,
                move |r| async move {
                    l1.lock().unwrap().push(format!("dispatch:{}", r[0]));
                    r
                },
                move |r| async move {
                    l2.lock().unwrap().push(format!("write:{}", r[0]));
                    Ok(())
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "dispatch:0",
                "write:0",
                "dispatch:1",
                "write:1",
                "dispatch:2",
                "write:2"
            ],
            "sequential mode must fully complete each request before the next"
        );
    }

    /// The inline fast path: with no backlog hint and nothing in
    /// flight, each request completes fully inside submit — strict
    /// dispatch/write interleaving even in pipelined mode. This is
    /// what keeps QD-1 latency identical to the pre-pipelining loop.
    #[tokio::test]
    async fn inline_fast_path_when_idle() {
        let p = ConnectionPipeline::new(64);
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        for i in 0..3u8 {
            let l1 = Arc::clone(&log);
            let l2 = Arc::clone(&log);
            p.submit(
                req(i),
                false,
                false,
                move |r| async move {
                    l1.lock().unwrap().push(format!("dispatch:{}", r[0]));
                    r
                },
                move |r| async move {
                    l2.lock().unwrap().push(format!("write:{}", r[0]));
                    Ok(())
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "dispatch:0",
                "write:0",
                "dispatch:1",
                "write:1",
                "dispatch:2",
                "write:2"
            ],
            "idle connection must take the zero-overhead inline path"
        );
    }

    /// A failed pipelined write must poison the pipeline so the read
    /// loop tears the connection down instead of feeding a dead
    /// writer forever.
    #[tokio::test]
    async fn write_failure_breaks_pipeline() {
        let p = ConnectionPipeline::new(8);

        p.submit(
            req(0),
            true,
                false,
            |r| async move { r },
            |_| async {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "peer gone",
                ))
            },
        )
        .await
        .unwrap();

        // The failure lands asynchronously; poll until visible.
        let mut poisoned = false;
        for _ in 0..100 {
            sleep(Duration::from_millis(2)).await;
            if p.submit(req(1), true, false, |r| async move { r }, |_| async { Ok(()) })
                .await
                .is_err()
            {
                poisoned = true;
                break;
            }
        }
        assert!(poisoned, "pipeline must reject submits after a write failure");
    }

    fn leaked_gate() -> &'static DrainGate {
        Box::leak(Box::new(DrainGate::new()))
    }

    /// F55: drain must WAIT for a reply already past dispatch to finish
    /// flushing — the truncated-frame hazard is exactly a write cut off
    /// mid-flight.
    #[tokio::test]
    async fn f55_drain_waits_for_inflight_reply_write() {
        let gate = leaked_gate();
        let p = ConnectionPipeline::with_gate(8, gate);
        let wrote: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

        let w = Arc::clone(&wrote);
        p.submit(
            req(0),
            true,
                false, // force the spawned path
            |r| async move {
                sleep(Duration::from_millis(80)).await;
                r
            },
            move |_| async move {
                sleep(Duration::from_millis(40)).await;
                w.store(true, Ordering::Release);
                Ok(())
            },
        )
        .await
        .unwrap();

        gate.begin();
        let (remaining, clean) = gate.drain(Duration::from_secs(2)).await;
        assert!(clean, "drain must complete cleanly, {} left", remaining);
        assert!(
            wrote.load(Ordering::Acquire),
            "the in-flight reply must have finished writing before drain returned"
        );
    }

    /// F55: once draining, submit must REFUSE new dispatches — an
    /// unreplied request is retransmitted by the client; starting a reply
    /// during shutdown re-opens the truncation window.
    #[tokio::test]
    async fn f55_submit_refused_once_draining() {
        let gate = leaked_gate();
        let p = ConnectionPipeline::with_gate(8, gate);
        gate.begin();
        let dispatched: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let d = Arc::clone(&dispatched);
        let res = p
            .submit(
                req(0),
                true,
                false,
                move |r| async move {
                    d.store(true, Ordering::Release);
                    r
                },
                |_| async { Ok(()) },
            )
            .await;
        assert!(res.is_err(), "draining pipeline must refuse the frame");
        assert!(!dispatched.load(Ordering::Acquire), "and must not have dispatched it");
        // Nothing entered, so the drain is instant and clean.
        let (remaining, clean) = gate.drain(Duration::from_millis(50)).await;
        assert_eq!((remaining, clean), (0, true));
    }

    /// F55: the deadline is a hard bound (the F33b prompt-exit
    /// obligation) — a wedged write reports dirty instead of hanging
    /// shutdown past kubelet's grace.
    #[tokio::test]
    async fn f55_drain_deadline_expires_dirty() {
        let gate = leaked_gate();
        let p = ConnectionPipeline::with_gate(8, gate);
        p.submit(
            req(0),
            true,
                false,
            |r| async move { r },
            |_| async {
                std::future::pending::<()>().await;
                unreachable!()
            },
        )
        .await
        .unwrap();
        gate.begin();
        let start = Instant::now();
        let (remaining, clean) = gate.drain(Duration::from_millis(100)).await;
        assert!(!clean, "a wedged reply must expire the deadline");
        assert_eq!(remaining, 1);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "drain must return promptly at the deadline"
        );
    }

    /// Sequential fallback propagates write errors synchronously (the
    /// old loop's behavior).
    #[tokio::test]
    async fn sequential_write_error_is_synchronous() {
        let p = ConnectionPipeline::new(0);
        let res = p
            .submit(
                req(0),
                false,
                false,
                |r| async move { r },
                |_| async {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "peer gone",
                    ))
                },
            )
            .await;
        assert!(res.is_err());
    }
}
