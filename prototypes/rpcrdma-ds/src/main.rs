//! rpcrdma-ds — flint DS prototype speaking RPC-over-RDMA v1 (RFC 8166).
//!
//! M1 scope: accept the kernel client's rdma-cm connection on 20049 and
//! serve the DS session ops (EXCHANGE_ID / CREATE_SESSION / SEQUENCE /
//! RECLAIM_COMPLETE / DESTROY_*) as inline RDMA_MSG payloads. Data ops
//! (chunked READ/WRITE) are M2. A direct
//!   mount -t nfs4 -o proto=rdma,port=20049,vers=4.1 <ip>:/ /mnt
//! is the M1 harness: the mount ultimately fails at PUTROOTFH (NOTSUPP,
//! deliberate), but by then the kernel has exercised the entire
//! RPC-over-RDMA request/reply path against us — which is the question.

use std::os::raw::{c_int, c_uchar, c_uint, c_ushort};

extern "C" {
    fn rshim_listen(port: c_ushort) -> c_int;
    fn rshim_accept() -> c_int;
    fn rshim_wait_recv(buf: *mut *mut c_uchar, len: *mut c_uint) -> c_int;
    fn rshim_repost(idx: c_int) -> c_int;
    fn rshim_send(msg: *const c_uchar, len: c_uint) -> c_int;
}

// ── XDR helpers ──────────────────────────────────────────────────────
struct Dec<'a> { b: &'a [u8], p: usize }
impl<'a> Dec<'a> {
    fn new(b: &'a [u8]) -> Self { Dec { b, p: 0 } }
    fn u32(&mut self) -> Option<u32> {
        let v = self.b.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_be_bytes(v.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(((self.u32()? as u64) << 32) | self.u32()? as u64)
    }
    fn opaque(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        let v = self.b.get(self.p..self.p + n)?;
        self.p += (n + 3) & !3;
        Some(v)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        if self.p + n > self.b.len() { return None; }
        self.p += n;
        Some(())
    }
}

#[derive(Default)]
struct Enc { b: Vec<u8> }
impl Enc {
    fn u32(&mut self, v: u32) { self.b.extend_from_slice(&v.to_be_bytes()) }
    fn u64(&mut self, v: u64) { self.b.extend_from_slice(&v.to_be_bytes()) }
    fn opaque(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.b.extend_from_slice(v);
        for _ in 0..((4 - v.len() % 4) % 4) { self.b.push(0) }
    }
    fn raw(&mut self, v: &[u8]) { self.b.extend_from_slice(v) }
}

// NFS4 op numbers / statuses we speak
const OP_EXCHANGE_ID: u32 = 42;
const OP_CREATE_SESSION: u32 = 43;
const OP_DESTROY_SESSION: u32 = 44;
const OP_SEQUENCE: u32 = 53;
const OP_DESTROY_CLIENTID: u32 = 57;
const OP_RECLAIM_COMPLETE: u32 = 58;
const NFS4_OK: u32 = 0;
const NFS4ERR_NOTSUPP: u32 = 10004;

const SESSION_ID: [u8; 16] = *b"rpcrdma-proto-01";
const MAX_SLOTS: u32 = 16;

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(20049);
    unsafe {
        assert_eq!(rshim_listen(port), 0, "listen failed (root? rxe up?)");
        eprintln!("🎧 rpcrdma-ds listening on rdma-cm port {port}");
        loop {
            let r = rshim_accept();
            if r != 0 { eprintln!("accept error {r}"); continue; }
            eprintln!("⚡ RDMA connection ESTABLISHED");
            serve_conn();
            eprintln!("🔌 connection closed");
        }
    }
}

unsafe fn serve_conn() {
    loop {
        let mut buf: *mut c_uchar = std::ptr::null_mut();
        let mut len: c_uint = 0;
        let idx = rshim_wait_recv(&mut buf, &mut len);
        if idx < 0 { return; }
        let msg = std::slice::from_raw_parts(buf, len as usize);
        if let Some(reply) = handle_rpcrdma(msg) {
            let rc = rshim_send(reply.as_ptr(), reply.len() as c_uint);
            if rc != 0 { eprintln!("send failed {rc}"); return; }
        }
        rshim_repost(idx);
    }
}

/// Parse one RPC-over-RDMA message; build the full reply (transport
/// header + inline RPC reply) or None to stay silent.
fn handle_rpcrdma(msg: &[u8]) -> Option<Vec<u8>> {
    let mut d = Dec::new(msg);
    let xid = d.u32()?;
    let vers = d.u32()?;
    let _credits = d.u32()?;
    let proc_ = d.u32()?;
    if vers != 1 {
        eprintln!("✗ rdma vers {vers} (want 1)");
        return None;
    }
    // Chunk lists. M1 handles the empty shapes; log loudly otherwise.
    let mut nread = 0;
    while d.u32()? == 1 {
        d.skip(4 + 4 + 4 + 8)?; // position + handle + length + offset
        nread += 1;
    }
    let mut nwrite = 0;
    while d.u32()? == 1 {
        let segs = d.u32()?;
        d.skip(segs as usize * 16)?;
        nwrite += 1;
    }
    let reply_chunk = d.u32()? == 1;
    if reply_chunk { let segs = d.u32()?; d.skip(segs as usize * 16)?; }
    if nread > 0 || nwrite > 0 {
        eprintln!("ℹ️ chunks in request (read {nread} write {nwrite}) — M2 territory");
    }
    if proc_ != 0 {
        eprintln!("✗ rdma proc {proc_} (only RDMA_MSG at M1)");
        return None;
    }

    // Inline RPC call
    let rxid = d.u32()?;
    let mtype = d.u32()?;
    let _rpcvers = d.u32()?;
    let prog = d.u32()?;
    let _pvers = d.u32()?;
    let rproc = d.u32()?;
    if mtype != 0 || prog != 100003 || rxid != xid {
        eprintln!("✗ rpc frame: mtype {mtype} prog {prog}");
        return None;
    }
    d.u32()?; let clen = d.u32()? as usize; d.skip((clen + 3) & !3)?; // cred
    d.u32()?; let vlen = d.u32()? as usize; d.skip((vlen + 3) & !3)?; // verf

    let mut rpc = Enc::default();
    rpc.u32(xid);
    rpc.u32(1); // REPLY
    rpc.u32(0); // MSG_ACCEPTED
    rpc.u32(0); rpc.u32(0); // verf AUTH_NONE
    rpc.u32(0); // SUCCESS

    match rproc {
        0 => eprintln!("✅ NULL ping over RDMA"),
        1 => encode_compound_reply(&mut d, &mut rpc)?,
        p => { eprintln!("✗ rpc proc {p}"); return None; }
    }

    // Transport header: RDMA_MSG, credits granted, empty chunk lists.
    let mut out = Enc::default();
    out.u32(xid);
    out.u32(1);          // vers
    out.u32(MAX_SLOTS.max(32)); // credits
    out.u32(0);          // RDMA_MSG
    out.u32(0); out.u32(0); out.u32(0); // empty read/write/reply lists
    out.raw(&rpc.b);
    Some(out.b)
}

fn encode_compound_reply(d: &mut Dec, rpc: &mut Enc) -> Option<()> {
    let _tag = d.opaque()?;
    let _minor = d.u32()?;
    let numops = d.u32()?;
    let mut results: Vec<(u32, u32, Vec<u8>)> = Vec::new(); // (op, status, body)
    let mut overall = NFS4_OK;

    for _ in 0..numops {
        let op = d.u32()?;
        let (st, body) = match op {
            OP_EXCHANGE_ID => {
                let verifier = { let mut v = [0u8; 8]; v.copy_from_slice(d.b.get(d.p..d.p + 8)?); d.skip(8)?; v };
                let owner = d.opaque()?.to_vec();
                let flags = d.u32()?;
                let sp = d.u32()?; // state_protect: SP4_NONE expected
                if sp != 0 { d.skip(0)?; }
                eprintln!(
                    "📥 EXCHANGE_ID over RDMA: owner={:?} flags={flags:#x} verf={:02x?}",
                    String::from_utf8_lossy(&owner), &verifier[..4]
                );
                let mut e = Enc::default();
                e.u64(0x1234_5678_9abc_def0); // clientid
                e.u32(1);                     // sequenceid
                e.u32(0x0002_0000);           // EXCHGID4_FLAG_USE_PNFS_DS
                e.u32(0);                     // SP4_NONE
                e.u64(0);                     // server_owner minor
                e.opaque(b"rpcrdma-ds-proto"); // server_owner major
                e.opaque(b"rpcrdma-ds-proto"); // server_scope
                e.u32(0);                     // impl_id: empty array
                (NFS4_OK, e.b)
            }
            OP_CREATE_SESSION => {
                let clientid = d.u64()?;
                let seq = d.u32()?;
                let _flags = d.u32()?;
                // fore + back channel attrs: 6 u32 + ird array each
                for _ in 0..2 {
                    d.skip(6 * 4)?;
                    let ird = d.u32()?;
                    d.skip(ird as usize * 4)?;
                }
                eprintln!("📥 CREATE_SESSION over RDMA: clientid={clientid:#x} seq={seq}");
                let mut e = Enc::default();
                e.raw(&SESSION_ID);
                e.u32(seq);
                e.u32(0); // flags
                // fore channel attrs
                e.u32(0); e.u32(1 << 20); e.u32(1 << 20); e.u32(4096);
                e.u32(16); e.u32(MAX_SLOTS); e.u32(0);
                // back channel attrs
                e.u32(0); e.u32(4096); e.u32(4096); e.u32(0);
                e.u32(2); e.u32(4); e.u32(0);
                (NFS4_OK, e.b)
            }
            OP_SEQUENCE => {
                let mut sid = [0u8; 16];
                sid.copy_from_slice(d.b.get(d.p..d.p + 16)?); d.skip(16)?;
                let seq = d.u32()?;
                let slot = d.u32()?;
                let _highest = d.u32()?;
                let _cache = d.u32()?;
                eprintln!("📥 SEQUENCE over RDMA: slot={slot} seq={seq}");
                let mut e = Enc::default();
                e.raw(&sid);
                e.u32(seq);
                e.u32(slot);
                e.u32(MAX_SLOTS - 1); // highest
                e.u32(MAX_SLOTS - 1); // target highest
                e.u32(0);             // status flags
                (NFS4_OK, e.b)
            }
            OP_RECLAIM_COMPLETE => {
                d.u32()?; // one_fs bool
                eprintln!("📥 RECLAIM_COMPLETE over RDMA");
                (NFS4_OK, Vec::new())
            }
            OP_DESTROY_SESSION => {
                d.skip(16)?;
                eprintln!("📥 DESTROY_SESSION over RDMA");
                (NFS4_OK, Vec::new())
            }
            OP_DESTROY_CLIENTID => {
                d.u64()?;
                eprintln!("📥 DESTROY_CLIENTID over RDMA");
                (NFS4_OK, Vec::new())
            }
            other => {
                eprintln!("🛑 op {other} → NOTSUPP (M1 boundary; mount fails here by design)");
                overall = NFS4ERR_NOTSUPP;
                results.push((other, NFS4ERR_NOTSUPP, Vec::new()));
                break;
            }
        };
        if st != NFS4_OK { overall = st; }
        results.push((op, st, body));
        if st != NFS4_OK { break; }
    }

    // Rewrite RPC accept body: we already wrote SUCCESS; now compound
    // status + tag + results.
    rpc.u32(overall);
    rpc.opaque(b"");
    rpc.u32(results.len() as u32);
    for (op, st, body) in results {
        rpc.u32(op);
        rpc.u32(st);
        rpc.raw(&body);
    }
    Some(())
}
