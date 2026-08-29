//! Reusable READ buffers.
//!
//! # Why this exists
//!
//! The READ path used to do `vec![0u8; count]` per request. At a 1 MiB
//! rsize that is an mmap per READ: the kernel must find pages, zero
//! them, fault them in one at a time on first touch, then munmap the
//! region when the reply is dropped — for memory whose every byte
//! `read_at` overwrites immediately.
//!
//! The cost is not only CPU. **mmap and munmap take the process-wide
//! `mmap_lock` for WRITE**, so every concurrent READ serialises on it.
//! That is why the server got *less* efficient as concurrency rose,
//! which is the opposite of what a server should do.
//!
//! Measured on lima (2 vCPU, aarch64, nconnect=4, client O_DIRECT,
//! server cache warm) before this pool existed:
//!
//! | concurrency | flint MiB/s | cpu-ms/GiB | knfsd MiB/s | cpu-ms/GiB |
//! |---|---|---|---|---|
//! | 1 | 2151 | 520 | 4830 | 280 |
//! | 4 | 2860 | 570 | 7474 | 210 |
//! | 8 | 2737 | 600 | 7728 | 235 |
//!
//! flint plateaus and grows dearer per byte; knfsd scales and grows
//! cheaper. A profile at concurrency 8 showed
//! `rwsem_down_write_slowpath` and `mt_find` (maple-tree VMA lookup) —
//! the `mmap_lock` and the VMA bookkeeping behind it.
//!
//! # Why it is still zero-copy
//!
//! A pool normally forces a copy out, because the pool must keep the
//! allocation. `Bytes::from_owner` avoids that: the reply's `Bytes`
//! borrows the pooled buffer and returns it to the pool when the last
//! reference drops. No copy on the way out, and no allocation on the
//! way in once the pool is warm.

use bytes::Bytes;
use std::sync::Mutex;

/// Buffers retained at rest. Bounded because the tokio blocking pool is
/// not: it defaults to 512 threads, and a per-thread buffer would let
/// the resident set follow thread count rather than concurrency.
const POOL_MAX: usize = 64;

/// Buffers larger than this are dropped rather than retained. One rsize
/// plus slack; an outsized read must not permanently inflate the pool.
const POOL_BUF_CAP: usize = 2 * 1024 * 1024;

static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

fn take(count: usize) -> Vec<u8> {
    let mut buf = POOL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop()
        .unwrap_or_default();
    buf.clear();
    // On a pooled buffer with capacity this is a memset in resident
    // memory: no mmap, no faults, no mmap_lock. On a cold one it is the
    // allocation that used to happen every request.
    buf.resize(count, 0);
    buf
}

fn give(mut buf: Vec<u8>) {
    if buf.capacity() > POOL_BUF_CAP {
        return;
    }
    buf.clear();
    let mut p = POOL.lock().unwrap_or_else(|e| e.into_inner());
    if p.len() < POOL_MAX {
        p.push(buf);
    }
}

/// Owner handed to [`Bytes::from_owner`]: exposes the filled prefix and
/// returns the allocation to the pool when the last `Bytes` drops.
struct Pooled {
    buf: Option<Vec<u8>>,
    len: usize,
}

impl AsRef<[u8]> for Pooled {
    fn as_ref(&self) -> &[u8] {
        // `buf` is Some for the whole life of the owner; only Drop takes it.
        &self.buf.as_ref().expect("owner outlives its Bytes")[..self.len]
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        if let Some(b) = self.buf.take() {
            give(b);
        }
    }
}

/// Read `count` bytes at `offset` into a pooled buffer.
///
/// The returned `Bytes` owns the buffer until dropped, at which point it
/// goes back to the pool. `read` is any positioned reader — the caller
/// keeps its own tier/eviction checks around this.
pub fn read_at_pooled<F>(count: usize, read: F) -> std::io::Result<Bytes>
where
    F: FnOnce(&mut [u8]) -> std::io::Result<usize>,
{
    let mut buf = take(count);
    match read(&mut buf) {
        Ok(n) => Ok(Bytes::from_owner(Pooled { buf: Some(buf), len: n })),
        Err(e) => {
            give(buf);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool is a process-wide static, so these tests would otherwise
    /// interfere with each other under the default parallel runner —
    /// one test's `clear()` lands inside another's count. Same reason
    /// `tier::capture::test_exclusive()` exists.
    static EXCLUSIVE: Mutex<()> = Mutex::new(());
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pool_len() -> usize {
        POOL.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// The point of the whole module: the allocation comes back.
    #[test]
    fn a_dropped_reply_returns_its_buffer_to_the_pool() {
        let _x = exclusive();
        POOL.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let before = pool_len();
        {
            let b = read_at_pooled(4096, |buf| {
                buf[..4].copy_from_slice(b"data");
                Ok(4)
            })
            .unwrap();
            assert_eq!(&b[..], b"data");
            assert_eq!(
                pool_len(),
                before,
                "the buffer must still be OUT while the Bytes is alive"
            );
        }
        assert_eq!(pool_len(), before + 1, "dropping the Bytes must return it");
    }

    /// A short read must expose only what was read — the rest of the
    /// buffer is whatever the previous request left in it, and serving
    /// that would leak one client's bytes to another.
    #[test]
    fn only_the_bytes_actually_read_are_visible() {
        let _x = exclusive();
        let _ = read_at_pooled(64, |buf| {
            buf.fill(b'X');
            Ok(64)
        })
        .unwrap();
        let b = read_at_pooled(64, |buf| {
            buf[..3].copy_from_slice(b"abc");
            Ok(3)
        })
        .unwrap();
        assert_eq!(b.len(), 3, "a short read must not expose the tail");
        assert_eq!(&b[..], b"abc");
    }

    /// A reused buffer must arrive zeroed, not carrying the last
    /// request's contents into the region a short read leaves untouched.
    #[test]
    fn a_reused_buffer_is_zeroed_before_reuse() {
        let _x = exclusive();
        let b1 = read_at_pooled(32, |buf| {
            buf.fill(0xAB);
            Ok(32)
        })
        .unwrap();
        drop(b1);
        let seen = read_at_pooled(32, |buf| Ok(buf.iter().filter(|&&x| x == 0xAB).count()))
            .unwrap();
        assert_eq!(seen.len(), 0, "handed back a zeroed buffer, so nothing to read");
    }

    /// An error must not lose the allocation — otherwise a failing
    /// volume drains the pool and every later read allocates again.
    #[test]
    fn a_failed_read_still_returns_its_buffer() {
        let _x = exclusive();
        POOL.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let r = read_at_pooled(128, |_| {
            Err(std::io::Error::other("boom"))
        });
        assert!(r.is_err());
        assert_eq!(pool_len(), 1, "the buffer must return on the error path too");
    }

    /// The pool is bounded: the blocking pool is not, and a buffer per
    /// thread would size the resident set by thread count.
    #[test]
    fn the_pool_does_not_grow_without_bound() {
        let _x = exclusive();
        POOL.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let mut held = Vec::new();
        for _ in 0..(POOL_MAX + 16) {
            held.push(read_at_pooled(16, |b| Ok(b.len())).unwrap());
        }
        drop(held);
        assert_eq!(pool_len(), POOL_MAX, "must cap at POOL_MAX");
    }

    /// An outsized buffer is dropped rather than retained.
    #[test]
    fn an_outsized_buffer_is_not_retained() {
        let _x = exclusive();
        POOL.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let b = read_at_pooled(POOL_BUF_CAP + 1, |buf| Ok(buf.len())).unwrap();
        drop(b);
        assert_eq!(pool_len(), 0, "a buffer above the cap must not inflate the pool");
    }
}
