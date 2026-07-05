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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::warn;

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
}

impl ConnectionPipeline {
    /// `max_inflight == 0` selects the sequential fallback (I5).
    pub fn new(max_inflight: u32) -> Self {
        Self {
            sem: (max_inflight > 0)
                .then(|| Arc::new(Semaphore::new(max_inflight as usize))),
            max_inflight: max_inflight as usize,
            broken: Arc::new(AtomicBool::new(false)),
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
    /// (`broken`). Dispatch itself is infallible (`Bytes` in,
    /// `Bytes` out) by construction of the RPC layer.
    pub async fn submit<D, DF, W, WF>(
        &self,
        request: Bytes,
        more_queued: bool,
        dispatch: D,
        write: W,
    ) -> std::io::Result<()>
    where
        D: FnOnce(Bytes) -> DF + Send + 'static,
        DF: Future<Output = Bytes> + Send + 'static,
        W: FnOnce(Bytes) -> WF + Send + 'static,
        WF: Future<Output = std::io::Result<()>> + Send + 'static,
    {
        if self.broken.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "connection writer failed in an earlier pipelined reply",
            ));
        }

        let Some(sem) = &self.sem else {
            // I5: sequential fallback.
            let reply = dispatch(request).await;
            return write(reply).await;
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
        let others_in_flight = sem.available_permits() < self.max_inflight - 1;
        if !more_queued && !others_in_flight {
            let reply = dispatch(request).await;
            let res = write(reply).await;
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
                |_| async {
                    std::future::pending::<()>().await;
                    unreachable!()
                },
                |_| async { Ok(()) },
            )
            .await
            .unwrap();
        }

        let fifth = p.submit(req(9), true, |r| async { r }, |_| async { Ok(()) });
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
            if p.submit(req(1), true, |r| async move { r }, |_| async { Ok(()) })
                .await
                .is_err()
            {
                poisoned = true;
                break;
            }
        }
        assert!(poisoned, "pipeline must reject submits after a write failure");
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
