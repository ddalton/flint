//! rpcrdma-ds — flint DS prototype speaking RPC-over-RDMA v1 (RFC 8166).
//!
//! M1 (0e08ab8): the kernel client's session establishment, inline.
//! M2: the DS data path — SEQUENCE/PUTFH/READ/WRITE/COMMIT, with READ
//! payloads RDMA-Written into the client's Write chunk and WRITE
//! payloads RDMA-Read from its Read chunk (RFC 8267). Files live under
//! the store dir, one per DS filehandle (named by its hex).
//!
//! It is driven through a real flint MDS: a DS endpoint configured as
//! `rdma://<ip>:20049` makes GETDEVICEINFO advertise netid "rdma", and
//! the kernel opens that DS connection through xprtrdma while the MDS
//! connection stays TCP. See tests/lima/pnfs/rdma-drill.sh.
//!
//!   rpcrdma-ds [port=20049] [store=/var/tmp/rpcrdma-ds]

mod proto;

#[cfg(target_os = "linux")]
mod verbs {
    use std::os::raw::{c_int, c_uchar, c_uint, c_ulong, c_ulonglong, c_ushort};

    extern "C" {
        pub fn rshim_listen(port: c_ushort) -> c_int;
        pub fn rshim_accept() -> c_int;
        pub fn rshim_wait_recv(buf: *mut *mut c_uchar, len: *mut c_uint) -> c_int;
        pub fn rshim_repost(idx: c_int) -> c_int;
        pub fn rshim_send(msg: *const c_uchar, len: c_uint) -> c_int;
        pub fn rshim_data_buf(buf: *mut *mut c_uchar, len: *mut c_uint);
        pub fn rshim_rdma(is_read: c_int, local_off: c_ulong, len: c_uint, rkey: c_uint, raddr: c_ulonglong) -> c_int;
        pub fn rshim_drain() -> c_int;
    }

    /// The live connection's registered data buffer + send queue.
    pub struct Verbs {
        data: &'static mut [u8],
    }

    impl Verbs {
        /// Valid only between rshim_accept() and the next accept.
        pub unsafe fn for_connection() -> Self {
            let mut p = std::ptr::null_mut();
            let mut n = 0;
            rshim_data_buf(&mut p, &mut n);
            Verbs { data: std::slice::from_raw_parts_mut(p, n as usize) }
        }
    }

    impl crate::proto::Rdma for Verbs {
        fn data(&mut self) -> &mut [u8] {
            self.data
        }
        fn write(&mut self, local: usize, len: u32, s: &crate::proto::Seg) -> Result<(), String> {
            match unsafe { rshim_rdma(0, local as c_ulong, len, s.handle, s.offset) } {
                0 => Ok(()),
                rc => Err(format!("post RDMA Write failed {rc}")),
            }
        }
        fn read(&mut self, local: usize, s: &crate::proto::Seg) -> Result<(), String> {
            match unsafe { rshim_rdma(1, local as c_ulong, s.length, s.handle, s.offset) } {
                0 => Ok(()),
                rc => Err(format!("post RDMA Read failed {rc}")),
            }
        }
        fn drain(&mut self) -> Result<(), String> {
            match unsafe { rshim_drain() } {
                0 => Ok(()),
                rc => Err(format!("RDMA transfer failed, wc status {}", -rc)),
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    use verbs::*;
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|a| a.parse().ok()).unwrap_or(20049);
    let store = args.next().unwrap_or_else(|| "/var/tmp/rpcrdma-ds".into());
    let mut ds = proto::Ds::new(store.clone().into());
    unsafe {
        assert_eq!(rshim_listen(port), 0, "listen failed (root? rxe up? nfsd holding 20049?)");
        eprintln!("🎧 rpcrdma-ds listening on rdma-cm port {port}, store {store}");
        loop {
            let r = rshim_accept();
            if r != 0 {
                eprintln!("accept error {r}");
                continue;
            }
            eprintln!("⚡ RDMA connection ESTABLISHED");
            let mut v = Verbs::for_connection();
            serve_conn(&mut v, &mut ds);
            eprintln!("🔌 connection closed — {:?}", ds.counters);
        }
    }
}

#[cfg(target_os = "linux")]
unsafe fn serve_conn(v: &mut verbs::Verbs, ds: &mut proto::Ds) {
    use verbs::*;
    let mut last = proto::Counters::default();
    loop {
        let mut buf = std::ptr::null_mut();
        let mut len = 0;
        let idx = rshim_wait_recv(&mut buf, &mut len);
        if idx < 0 {
            return;
        }
        let msg = std::slice::from_raw_parts(buf, len as usize);
        let reply = proto::handle(msg, v, ds);
        // The receive is fully consumed (the reply is owned): repost
        // BEFORE sending, so the credit the reply returns is backed by a
        // posted receive when the client spends it.
        rshim_repost(idx);
        match reply {
            Ok(Some(out)) => {
                if rshim_send(out.as_ptr(), out.len() as u32) != 0 {
                    eprintln!("send failed");
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => {
                // No reply: the client retransmits / resets the
                // connection, which is loud. A made-up reply would not be.
                eprintln!("✗ {e}");
            }
        }
        let c = ds.counters;
        if c.reads + c.writes + c.commits != last.reads + last.writes + last.commits {
            if (c.reads + c.writes) % 64 == 0 {
                eprintln!("📊 {c:?}");
            }
            // RPCRDMA_STATS: the drill reads what the DS delivered from
            // here, not from the log. Written whole, then renamed in.
            if let Ok(path) = std::env::var("RPCRDMA_STATS") {
                let tmp = format!("{path}.tmp");
                let body = format!(
                    "reads={} writes={} commits={} rdma_written={} rdma_read={} inline_read={} inline_written={}\n",
                    c.reads, c.writes, c.commits, c.rdma_written, c.rdma_read, c.inline_read, c.inline_written
                );
                if std::fs::write(&tmp, body).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }
        last = c;
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("rpcrdma-ds needs Linux verbs; only the protocol tests build here (cargo test)");
    std::process::exit(2);
}
