//! Zero-copy READ staging via `splice(2)`.
//!
//! # Why a pipe, and not file -> socket directly
//!
//! The READ path re-consults the tier AFTER the read
//! (`v4::operations::ioops` -> `tier::evict::read_window_intact`). That
//! check exists because an eviction can land between the consult and the
//! read; without it the server serves a truncated stub as if it were file
//! content. It was caught live by the chaos drill — git once read an
//! empty `.git/config`.
//!
//! Splicing straight to the socket would put the bytes on the wire BEFORE
//! that check can run, which makes the guarantee unenforceable. So the
//! payload is staged in a pipe first, where it is still retractable:
//!
//! ```text
//! file --splice--> pipe            nothing on the wire yet
//! caller runs read_window_intact
//!   fail -> drop(Staged)           retract; the copy path answers DELAY
//!   pass -> drain_to(socket)       second splice, still zero-copy
//! ```
//!
//! The pipe is here to PRESERVE an existing correctness guarantee, not
//! for throughput.
//!
//! # Why it is worth it
//!
//! S0 microbench, lima 2 vCPU aarch64, page-cache warm, 5 interleaved
//! reps, paired per-rep ratios, VM verified 100% idle:
//!
//! | chunk | pread+write cpu-ns/MiB | splice cpu-ns/MiB | ratio |
//! |---|---|---|---|
//! | 16 KiB | 343750 | 103188 | 0.300 |
//! | 64 KiB | 218781 | 73156 | 0.334 |
//! | 256 KiB | 210219 | 60547 | 0.284 |
//! | 1 MiB | 187516 | 53078 | 0.283 |
//!
//! **~72% less CPU at rsize.** A falsifiability arm (pread + an extra
//! memcpy + write) measured 1.03-1.10x worse than the baseline at 16 KiB
//! and above, proving the rig could actually resolve one copy — without
//! that arm a favourable ratio would not have been evidence.
//!
//! Note the win is CPU, NOT wall throughput: the same run gained only
//! ~21% MiB/s, and in 2 of 5 reps splice's wall time was WORSE. Anything
//! gating this change on MiB/s will read ~1.0 and conclude "no effect".

/// Largest payload this module will stage. Matches both the default
/// `/proc/sys/fs/pipe-max-size` (1 MiB, verified on the test VM) and the
/// server's rsize. A larger READ falls back to the copy path rather than
/// splicing in fragments — fragments would defeat the retract window.
pub const MAX_STAGE: usize = 1024 * 1024;

#[cfg(target_os = "linux")]
mod imp {
    use super::MAX_STAGE;
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// Pipes retained at rest. A pipe costs TWO fds, and the test VM's
    /// `ulimit -n` is 1024 — the same budget that produced EMFILE when
    /// the DS O_DIRECT change doubled its fd footprint. Live pipes are
    /// bounded by in-flight READs, not by this; this only caps what is
    /// held idle. 32 pipes = 64 fds + up to 32 MiB of kernel buffer.
    const POOL_MAX: usize = 32;

    static POOL: Mutex<Vec<Pipe>> = Mutex::new(Vec::new());

    /// Latches once a pipe cannot be grown to `MAX_STAGE`. Without it, a
    /// host whose `pipe-max-size` is below rsize would create and destroy
    /// a pipe on EVERY read — a slow path that looks like a leak.
    static UNDERSIZED: AtomicBool = AtomicBool::new(false);

    struct Pipe {
        r: RawFd,
        w: RawFd,
        cap: usize,
    }

    impl Pipe {
        fn new() -> std::io::Result<Pipe> {
            let mut fds = [0i32; 2];
            // O_CLOEXEC: these must not leak into a forked child.
            let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Grow to MAX_STAGE so a whole payload fits: the retract
            // window requires the ENTIRE payload staged before the
            // caller's check runs. F_SETPIPE_SZ returns the size it
            // actually installed, which the kernel rounds up — and
            // which an unprivileged process cannot push past
            // pipe-max-size. Take what we get and record it; a payload
            // larger than `cap` is refused rather than fragmented.
            let got = unsafe { libc::fcntl(fds[1], libc::F_SETPIPE_SZ, MAX_STAGE as libc::c_int) };
            let cap = if got > 0 { got as usize } else { 64 * 1024 };
            Ok(Pipe { r: fds[0], w: fds[1], cap })
        }
    }

    impl Drop for Pipe {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.r);
                libc::close(self.w);
            }
        }
    }

    /// A payload sitting in a pipe, not yet on the wire.
    ///
    /// Dropping this without a full [`Staged::drain_to`] RETRACTS it —
    /// nothing reaches the socket. That is the whole point of the type.
    pub struct Staged {
        pipe: Option<Pipe>,
        len: usize,
        drained: usize,
    }

    impl Staged {
        /// Bytes staged, i.e. what a successful drain will send.
        pub fn len(&self) -> usize {
            self.len
        }
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }
    }

    /// Stage `count` bytes at `offset` from `file` into a pipe.
    ///
    /// `Ok(None)` means "not spliceable — use the copy path": the payload
    /// exceeds pipe capacity, the filesystem does not support splice, or
    /// staging failed partway. It is deliberately not an error, because
    /// the copy path is a complete fallback that will surface any real
    /// I/O failure itself.
    pub fn stage_read(
        file: &std::fs::File,
        offset: u64,
        count: usize,
    ) -> std::io::Result<Option<Staged>> {
        if count == 0 || count > MAX_STAGE {
            return Ok(None);
        }
        if UNDERSIZED.load(Ordering::Relaxed) {
            return Ok(None);
        }
        // Pop under the lock, construct outside it: Pipe::new() makes two
        // syscalls and must not hold every other reader off the pool.
        let pooled = POOL.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let pipe = match pooled {
            Some(x) => x,
            None => Pipe::new()?,
        };
        if pipe.cap < count {
            // Cannot hold the whole payload, so the retract window cannot
            // be honoured. Refuse rather than fragment, and latch so we
            // stop paying for a pipe we will never be able to use.
            if count <= MAX_STAGE {
                UNDERSIZED.store(true, Ordering::Relaxed);
            }
            return Ok(None);
        }

        let fd = file.as_raw_fd();
        let mut off = offset as libc::loff_t;
        let mut staged = 0usize;
        while staged < count {
            let want = count - staged;
            let n = unsafe {
                libc::splice(
                    fd,
                    &mut off as *mut libc::loff_t,
                    pipe.w,
                    std::ptr::null_mut(),
                    want,
                    libc::SPLICE_F_MOVE,
                )
            };
            if n <= 0 {
                if n < 0 {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                }
                // Short or failed. Anything already in the pipe makes it
                // unsafe to reuse, and `Staged`'s Drop enforces that.
                let s = Staged { pipe: Some(pipe), len: staged, drained: 0 };
                drop(s);
                return Ok(None);
            }
            staged += n as usize;
        }
        Ok(Some(Staged { pipe: Some(pipe), len: staged, drained: 0 }))
    }

    impl Staged {
        /// Move the staged bytes to `stream`, zero-copy.
        ///
        /// Uses tokio's `try_io` so readiness bookkeeping stays correct:
        /// the socket is non-blocking, so `splice` returns EAGAIN and the
        /// reactor must be told to re-arm. Rolling our own readiness on a
        /// resource tokio already registered is how you get a task that
        /// never wakes.
        pub async fn drain_to(&mut self, stream: &tokio::net::TcpStream) -> std::io::Result<()> {
            let pipe_r = match self.pipe.as_ref() {
                Some(p) => p.r,
                None => return Ok(()),
            };
            while self.drained < self.len {
                let want = self.len - self.drained;
                stream.writable().await?;
                let res = stream.try_io(tokio::io::Interest::WRITABLE, || {
                    let n = unsafe {
                        libc::splice(
                            pipe_r,
                            std::ptr::null_mut(),
                            stream.as_raw_fd(),
                            std::ptr::null_mut(),
                            want,
                            libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
                        )
                    };
                    if n < 0 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                });
                match res {
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "splice to socket returned 0",
                        ))
                    }
                    Ok(n) => self.drained += n,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
    }

    impl Drop for Staged {
        fn drop(&mut self) {
            if let Some(p) = self.pipe.take() {
                // A pipe with bytes still in it must NEVER be reused: the
                // residue would prepend itself to a later reply, which is
                // precisely the "frame contains spliced foreign bytes"
                // corruption `nfs::pipeline` already guards against.
                // Retract is rare (an eviction race), so paying a pipe
                // for it is the right trade against that risk.
                if self.drained == self.len {
                    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
                    if pool.len() < POOL_MAX {
                        pool.push(p);
                        return;
                    }
                }
                drop(p); // closes both fds
            }
        }
    }

    #[cfg(test)]
    pub const POOL_MAX_FOR_TEST: usize = POOL_MAX;
    #[cfg(test)]
    pub fn reset_for_test() {
        UNDERSIZED.store(false, Ordering::Relaxed);
        POOL.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    #[cfg(test)]
    pub fn pool_len() -> usize {
        POOL.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    /// Non-Linux stub: `splice` is a Linux syscall, so every caller takes
    /// the copy path. Present so call sites need no `cfg` of their own.
    pub struct Staged(std::convert::Infallible);

    impl Staged {
        pub fn len(&self) -> usize {
            match self.0 {}
        }
        pub fn is_empty(&self) -> bool {
            match self.0 {}
        }
        pub async fn drain_to(&mut self, _s: &tokio::net::TcpStream) -> std::io::Result<()> {
            match self.0 {}
        }
    }

    pub fn stage_read(
        _file: &std::fs::File,
        _offset: u64,
        _count: usize,
    ) -> std::io::Result<Option<Staged>> {
        Ok(None)
    }
}

pub use imp::{stage_read, Staged};

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use tokio::io::AsyncReadExt;

    /// The pipe pool is a process-wide static, same reason
    /// `read_pool`'s tests serialise.
    static EXCLUSIVE: Mutex<()> = Mutex::new(());
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        EXCLUSIVE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// POSITIONAL contents: byte `i` is `(seed + i) % 251`. A uniform
    /// fill would make an off-by-one in the splice offset — the most
    /// likely real defect, since READ uses arbitrary offsets —
    /// completely undetectable.
    fn pattern(seed: usize, off: usize, len: usize) -> Vec<u8> {
        (0..len).map(|i| ((seed + off + i) % 251) as u8).collect()
    }

    fn file_of(dir: &tempfile::TempDir, name: &str, seed: usize, len: usize) -> std::fs::File {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&pattern(seed, 0, len)).unwrap();
        f.sync_all().unwrap();
        std::fs::File::open(&p).unwrap()
    }

    /// A connected loopback pair: (server-side sender, client-side receiver).
    async fn pair() -> (tokio::net::TcpStream, tokio::net::TcpStream) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        let c = tokio::net::TcpStream::connect(a).await.unwrap();
        let (s, _) = l.accept().await.unwrap();
        (s, c)
    }

    /// The basic contract, and the oracle the retract tests rely on: a
    /// drained stage arrives byte-for-byte.
    #[tokio::test]
    async fn a_staged_read_reaches_the_socket_byte_for_byte() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "a.bin", 7, 64 * 1024);
        let (tx, mut rx) = pair().await;

        let mut st = stage_read(&f, 0, 64 * 1024).unwrap().expect("must splice");
        assert_eq!(st.len(), 64 * 1024);
        st.drain_to(&tx).await.unwrap();
        drop(st);

        let mut got = vec![0u8; 64 * 1024];
        rx.read_exact(&mut got).await.unwrap();
        assert_eq!(got, pattern(7, 0, 64 * 1024), "payload must arrive intact");
    }

    /// READ uses arbitrary offsets, so the offset must actually be
    /// honoured. With a positional pattern an off-by-one here is a hard
    /// failure; with a uniform fill it would be invisible.
    #[tokio::test]
    async fn a_staged_read_honours_the_offset() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "a.bin", 3, 256 * 1024);
        let (tx, mut rx) = pair().await;

        let off = 100_001usize; // deliberately not page- or block-aligned
        let len = 40_000usize;
        let mut st = stage_read(&f, off as u64, len).unwrap().expect("must splice");
        st.drain_to(&tx).await.unwrap();
        drop(st);

        let mut got = vec![0u8; len];
        rx.read_exact(&mut got).await.unwrap();
        assert_eq!(got, pattern(3, off, len), "must serve the bytes AT the offset");
    }

    /// Two stages from one pooled pipe must not bleed into each other.
    #[tokio::test]
    async fn consecutive_stages_through_one_pooled_pipe_stay_distinct() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "a.bin", 11, 128 * 1024);
        let (tx, mut rx) = pair().await;

        for (off, len) in [(0usize, 8192usize), (8192, 4096), (33_333, 9_999)] {
            let mut st = stage_read(&f, off as u64, len).unwrap().expect("must splice");
            st.drain_to(&tx).await.unwrap();
            drop(st);
            let mut got = vec![0u8; len];
            rx.read_exact(&mut got).await.unwrap();
            assert_eq!(got, pattern(11, off, len), "stage at {off} len {len} must be exact");
        }
        assert_eq!(super::imp::pool_len(), 1, "one pipe should have served all three");
    }

    /// **The C5 guarantee.** Dropping a stage without draining must put
    /// NOTHING on the wire — that is what lets the tier re-consult still
    /// turn a completed read into DELAY instead of serving stub bytes.
    ///
    /// The sentinel is the anti-vacuity guard: it proves the channel is
    /// live and the reader would have seen the retracted bytes had they
    /// been sent, rather than the test passing because nothing works.
    #[tokio::test]
    async fn a_retracted_stage_puts_nothing_on_the_wire() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "a.bin", 23, 64 * 1024);
        let (mut tx, mut rx) = pair().await;

        let st = stage_read(&f, 0, 64 * 1024).unwrap().expect("must splice");
        drop(st); // retract

        use tokio::io::AsyncWriteExt;
        tx.write_all(b"SENTINEL").await.unwrap();
        tx.flush().await.unwrap();

        let mut got = [0u8; 8];
        rx.read_exact(&mut got).await.unwrap();
        assert_eq!(
            &got, b"SENTINEL",
            "retracted bytes must not precede the sentinel"
        );
    }

    /// A pipe that still holds bytes must never be reused: the residue
    /// would prepend itself to a later reply — the "frame contains
    /// spliced foreign bytes" corruption class.
    #[tokio::test]
    async fn a_retracted_pipe_is_not_reused() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let a = file_of(&d, "a.bin", 23, 32 * 1024);
        let b = file_of(&d, "b.bin", 91, 16 * 1024);

        drop(stage_read(&a, 0, 32 * 1024).unwrap().expect("must splice")); // retract

        let (tx, mut rx) = pair().await;
        let mut st = stage_read(&b, 0, 16 * 1024).unwrap().expect("must splice");
        assert_eq!(st.len(), 16 * 1024, "must stage only the new payload");
        st.drain_to(&tx).await.unwrap();
        drop(st);

        let mut got = vec![0u8; 16 * 1024];
        rx.read_exact(&mut got).await.unwrap();
        assert_eq!(
            got,
            pattern(91, 0, 16 * 1024),
            "no byte of the retracted payload may survive into the next reply"
        );
    }

    /// **The failure mode with the worst blast radius.** Once the record
    /// marker is on the wire a half-written frame cannot be recovered,
    /// so a drain that dies partway must surface as an ERROR the caller
    /// can act on (close the connection) -- never a silent success, and
    /// never a hang.
    ///
    /// A 1 MiB payload cannot fit in the socket buffer, so the drain has
    /// to touch a socket whose peer is already gone rather than
    /// completing into kernel buffering.
    #[tokio::test]
    async fn a_disconnect_mid_drain_errors_and_does_not_hang() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "a.bin", 13, MAX_STAGE);
        let (tx, rx) = pair().await;

        let mut st = stage_read(&f, 0, MAX_STAGE).unwrap().expect("must splice");
        drop(rx); // peer goes away before the payload can move

        let before = super::imp::pool_len();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            st.drain_to(&tx),
        )
        .await
        .expect("drain_to must not hang when the peer is gone");
        assert!(
            r.is_err(),
            "a dead peer must surface as an error, not silent success"
        );
        drop(st);
        assert_eq!(
            super::imp::pool_len(),
            before,
            "a partly-drained pipe still holds bytes and must NOT be pooled"
        );
    }

    /// Exercises the readiness retry loop, which nothing else does.
    ///
    /// The byte-for-byte test uses 64 KiB, which the socket buffer can
    /// swallow in a single splice -- so it never sees EAGAIN and the
    /// `writable().await` / re-arm path is untested by it. A 1 MiB
    /// payload cannot fit, so the drain MUST go around the loop, and
    /// every byte still has to arrive in order.
    #[tokio::test]
    async fn a_payload_larger_than_the_socket_buffer_drains_completely() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "a.bin", 17, MAX_STAGE);
        let (tx, mut rx) = pair().await;

        // FORCE the retry loop. Left to itself the socket buffer
        // auto-tunes, and a single splice can swallow the whole payload
        // -- which would make this test pass without ever reaching the
        // readiness path it exists to cover. Mutation testing showed
        // exactly that: a short-drain mutation was caught by the
        // disconnect test and NOT by this one. An 8 KiB send buffer
        // against a 1 MiB payload guarantees many EAGAIN rounds.
        {
            use std::os::unix::io::AsRawFd;
            let sz: libc::c_int = 8 * 1024;
            let rc = unsafe {
                libc::setsockopt(
                    tx.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    &sz as *const _ as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            assert_eq!(rc, 0, "SO_SNDBUF must be settable or this test proves nothing");
        }

        // Drain the peer concurrently, or the sender wedges on a full
        // socket buffer and the test times out instead of asserting.
        let reader = tokio::spawn(async move {
            let mut got = vec![0u8; MAX_STAGE];
            rx.read_exact(&mut got).await.unwrap();
            got
        });

        let mut st = stage_read(&f, 0, MAX_STAGE).unwrap().expect("must splice");
        st.drain_to(&tx).await.unwrap();
        drop(st);

        // Timeout, not a bare await: a regression that declared the
        // drain done early would leave the reader blocked forever, and a
        // test that hangs is a test nobody can diagnose. Truncation must
        // FAIL here, fast.
        let got = tokio::time::timeout(std::time::Duration::from_secs(10), reader)
            .await
            .expect("drain must deliver the whole payload, not wedge the reader")
            .unwrap();
        assert_eq!(
            got,
            pattern(17, 0, MAX_STAGE),
            "every byte must arrive, in order, across the retry loop"
        );
    }

    /// Above `MAX_STAGE` the retract window cannot be honoured, so the
    /// caller must be told to take the copy path — not handed fragments.
    #[tokio::test]
    async fn a_payload_over_max_stage_falls_back_to_the_copy_path() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "big.bin", 5, MAX_STAGE + 4096);
        assert!(
            stage_read(&f, 0, MAX_STAGE + 1).unwrap().is_none(),
            "oversized payload must fall back, never fragment"
        );
        assert!(
            stage_read(&f, 0, MAX_STAGE).unwrap().is_some(),
            "exactly MAX_STAGE must still splice"
        );
    }

    /// A fully drained pipe is clean, so it goes back to the pool.
    #[tokio::test]
    async fn a_drained_pipe_returns_to_the_pool() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "a.bin", 41, 8192);
        let (tx, mut rx) = pair().await;

        let before = super::imp::pool_len();
        let mut st = stage_read(&f, 0, 8192).unwrap().expect("must splice");
        assert_eq!(super::imp::pool_len(), before, "pipe is OUT while staged");
        st.drain_to(&tx).await.unwrap();
        drop(st);
        assert_eq!(
            super::imp::pool_len(),
            before + 1,
            "a drained pipe must be recycled"
        );
        let mut got = vec![0u8; 8192];
        rx.read_exact(&mut got).await.unwrap();
    }

    /// A retracted pipe is destroyed, not pooled — the inverse of the
    /// test above, and the reason retract is safe.
    #[tokio::test]
    async fn a_retracted_pipe_is_destroyed_not_pooled() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "a.bin", 59, 8192);
        let before = super::imp::pool_len();
        drop(stage_read(&f, 0, 8192).unwrap().expect("must splice"));
        assert_eq!(
            super::imp::pool_len(),
            before,
            "a pipe holding residue must NOT return to the pool"
        );
    }

    /// Bounded, for the same fd reason `ulimit -n` made EMFILE a real
    /// failure mode on this VM: a pipe costs two descriptors.
    #[tokio::test]
    async fn the_pool_does_not_grow_without_bound() {
        let _x = exclusive();
        super::imp::reset_for_test();
        let d = tempfile::tempdir().unwrap();
        let f = file_of(&d, "a.bin", 67, 4096);
        let (tx, mut rx) = pair().await;

        // The pipes must be held LIVE at the same time, then released
        // together. Staging and dropping one at a time would recycle a
        // single pipe and the pool would never approach the bound — the
        // assertion below could not fail, which is exactly how the first
        // version of this test passed while testing nothing.
        let n = super::imp::POOL_MAX_FOR_TEST + 8;
        // Drain the peer concurrently so filling the socket buffer with
        // n * 4 KiB cannot block the sender.
        let sink = tokio::spawn(async move {
            let mut sunk = vec![0u8; 4096 * (super::imp::POOL_MAX_FOR_TEST + 8)];
            rx.read_exact(&mut sunk).await.unwrap();
        });
        let mut held = Vec::new();
        for _ in 0..n {
            let mut st = stage_read(&f, 0, 4096).unwrap().expect("must splice");
            st.drain_to(&tx).await.unwrap();
            held.push(st); // still alive: its pipe is OUT of the pool
        }
        assert_eq!(super::imp::pool_len(), 0, "every pipe must be checked out");
        drop(held); // all n released at once
        assert_eq!(
            super::imp::pool_len(),
            super::imp::POOL_MAX_FOR_TEST,
            "the pool must cap at POOL_MAX, not keep all {n}"
        );
        sink.await.unwrap();
    }
}
