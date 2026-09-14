//! A bytes-in-flight bound for the UPLOAD path — the write side's mirror
//! of the checkout window's `fetch_inflight_max_bytes`.
//!
//! An upload part is read WHOLE into memory before its PUT (`s3.rs`
//! `compose_one_part`), and a whole-object body likewise (lean's
//! `upload_one`). Until this existed the only bound on what the upload
//! path held at once was a COUNT — `upload_fanout` objects, times
//! `part_parallelism` parts of each — so peak RSS was
//! `min(large_objects, fanout) x part_parallelism x part_size`, a product
//! of three numbers nobody sets together: 8-wide parts across 32 large
//! objects at 64 MiB a part is 16 GiB. That product is why the measured
//! 3.6x of `part_parallelism = 8` shipped as an opt-in in v1.51.0. This
//! bounds the BYTES, so the width can default to 8.
//!
//! Semantics, chosen so nothing can deadlock:
//!
//! - a request larger than the whole budget is CLAMPED to the budget, so
//!   a single body bigger than the window still proceeds — alone, having
//!   taken all of it — instead of waiting for permits that can never
//!   exist (the same rule as the read path's `FETCH_UNIT` clamp);
//! - permits are granted FIFO (tokio's semaphore): a large request at
//!   the head of the queue is not starved by small ones behind it;
//! - a permit is released on drop, so every error path releases.
//!
//! The counters are the TEST ORACLE. `high_water()` is the most bytes
//! ever held at once; a test asserts it never exceeded the budget on a
//! tree wide enough to blow far past it, while a control with a budget
//! too large to bind reports the unbounded mark on the same tree. The
//! counters carry the TRUE bytes of every holder, so a clamped request
//! shows its real size above the budget — alone, which is the point.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Permit granularity. Bytes are charged in whole units, rounded UP, so
/// the gate is conservative by less than one unit per holder and the
/// budget fits the semaphore's `u32` permit count up to 16 TiB.
const UNIT: u64 = 4096;

struct Counters {
    held: AtomicU64,
    high_water: AtomicU64,
}

/// The gate. Cheap to clone-by-`Arc`; one per store.
pub struct ByteGate {
    sem: Arc<Semaphore>,
    ctr: Arc<Counters>,
    max_units: u32,
    max_bytes: u64,
}

/// Bytes charged to a [`ByteGate`]; released on drop.
pub struct BytePermit {
    _permit: OwnedSemaphorePermit,
    bytes: u64,
    ctr: Arc<Counters>,
}

impl std::fmt::Debug for ByteGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByteGate")
            .field("max_bytes", &self.max_bytes)
            .field("held", &self.held())
            .field("high_water", &self.high_water())
            .finish()
    }
}

impl ByteGate {
    /// A gate over `max_bytes` (rounded up to a whole unit; at least one
    /// unit, so a zero budget is a one-at-a-time gate rather than a
    /// closed door).
    pub fn new(max_bytes: u64) -> ByteGate {
        let max_units = max_bytes.div_ceil(UNIT).clamp(1, u32::MAX as u64) as u32;
        ByteGate {
            sem: Arc::new(Semaphore::new(max_units as usize)),
            ctr: Arc::new(Counters { held: AtomicU64::new(0), high_water: AtomicU64::new(0) }),
            max_units,
            max_bytes: max_units as u64 * UNIT,
        }
    }

    /// The budget, as enforced (unit-rounded).
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Hold `bytes` until the returned permit drops. A request above the
    /// budget clamps to the whole budget and proceeds once it has it.
    pub async fn acquire(&self, bytes: u64) -> BytePermit {
        let want = bytes.div_ceil(UNIT).clamp(1, self.max_units as u64) as u32;
        let permit = self
            .sem
            .clone()
            .acquire_many_owned(want)
            .await
            .expect("the upload byte gate's semaphore is never closed");
        let now = self.ctr.held.fetch_add(bytes, Ordering::SeqCst) + bytes;
        self.ctr.high_water.fetch_max(now, Ordering::SeqCst);
        BytePermit { _permit: permit, bytes, ctr: self.ctr.clone() }
    }

    /// Bytes held right now, by every live permit.
    pub fn held(&self) -> u64 {
        self.ctr.held.load(Ordering::SeqCst)
    }

    /// The most bytes ever held at once.
    pub fn high_water(&self) -> u64 {
        self.ctr.high_water.load(Ordering::SeqCst)
    }
}

impl Drop for BytePermit {
    fn drop(&mut self) {
        self.ctr.held.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// A request larger than the whole budget must still be granted —
    /// with the whole budget — not wait forever for permits that cannot
    /// exist. And a permit released lets the next through.
    #[tokio::test]
    async fn a_request_above_the_budget_clamps_and_proceeds() {
        let g = ByteGate::new(2 * MIB);
        assert_eq!(g.max_bytes(), 2 * MIB);
        let big = tokio::time::timeout(std::time::Duration::from_secs(5), g.acquire(10 * MIB))
            .await
            .expect("a request above the budget deadlocked");
        assert_eq!(g.held(), 10 * MIB, "the counter carries the TRUE bytes");
        // While the clamped holder lives, the budget is fully taken: even
        // one byte waits.
        let waiter = tokio::time::timeout(std::time::Duration::from_millis(100), g.acquire(1)).await;
        assert!(waiter.is_err(), "a clamped holder must own the whole budget");
        drop(big);
        assert_eq!(g.held(), 0);
        let _one = tokio::time::timeout(std::time::Duration::from_secs(5), g.acquire(1))
            .await
            .expect("released bytes were not granted on");
    }

    /// Many concurrent acquirers, each under the budget: the high-water
    /// mark never exceeds it, and the SAME work through a gate too large
    /// to bind reports a far higher mark — the bound, proved load-bearing
    /// by its own control.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn high_water_never_exceeds_the_budget_under_contention() {
        async fn run(gate: Arc<ByteGate>) -> u64 {
            let mut set = tokio::task::JoinSet::new();
            for i in 0..64u64 {
                let g = gate.clone();
                set.spawn(async move {
                    let _p = g.acquire(MIB / 2 + (i % 3) * 100_000).await;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                });
            }
            while set.join_next().await.is_some() {}
            assert_eq!(gate.held(), 0, "every permit releases");
            gate.high_water()
        }
        let bounded = Arc::new(ByteGate::new(3 * MIB));
        let hw = run(bounded.clone()).await;
        assert!(hw <= 3 * MIB, "high-water {hw} exceeded the 3 MiB budget");
        assert!(hw > 0);
        let control = Arc::new(ByteGate::new(1 << 30));
        let chw = run(control.clone()).await;
        assert!(
            chw > 3 * MIB,
            "the control's high-water {chw} did not exceed the budget: the tree is too small \
             to prove the bound load-bearing"
        );
    }

    /// Dropping a permit — on any path, an error's included — gives the
    /// bytes back; a permit dropped by a cancelled future too.
    #[tokio::test]
    async fn permits_release_on_drop_including_cancellation() {
        let g = Arc::new(ByteGate::new(MIB));
        let p = g.acquire(MIB).await;
        assert_eq!(g.held(), MIB);
        // A second acquirer is parked behind the full budget; cancelling
        // it must leave nothing charged.
        let g2 = g.clone();
        let parked = tokio::spawn(async move {
            let _p = g2.acquire(MIB).await;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        parked.abort();
        let _ = parked.await;
        assert_eq!(g.held(), MIB, "a cancelled waiter charged nothing");
        drop(p);
        assert_eq!(g.held(), 0);
        assert_eq!(g.high_water(), MIB);
        // And a permit handed to a task that is aborted mid-hold releases.
        let g3 = g.clone();
        let holder = tokio::spawn(async move {
            let _p = g3.acquire(MIB).await;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(g.held(), MIB);
        holder.abort();
        let _ = holder.await;
        assert_eq!(g.held(), 0, "an aborted holder released its bytes");
        let _again = tokio::time::timeout(std::time::Duration::from_secs(5), g.acquire(MIB))
            .await
            .expect("the budget was not returned");
    }
}
