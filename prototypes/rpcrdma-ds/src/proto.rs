//! RPC-over-RDMA v1 (RFC 8166) framing and the NFSv4.1 DS ops (RFC 8267
//! upper-layer binding), with no verbs dependency: every RDMA transfer
//! goes through the [`Rdma`] trait, so the chunk logic is unit-tested
//! against a mock "client memory" on any OS.
//!
//! The two DDP-eligible items a DS moves (RFC 8267 §4.2):
//! - READ reply `data<>`: the length stays inline, the bytes go by RDMA
//!   Write into the client's Write chunk, and neither the bytes nor
//!   their XDR pad appear in the inline reply.
//! - WRITE call `data<>`: the length is inline, the bytes are pulled by
//!   RDMA Read from the Read chunk whose position is the offset of the
//!   item in the call's XDR stream; the inline stream omits bytes + pad.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

// ── XDR ──────────────────────────────────────────────────────────────
pub struct Dec<'a> {
    pub b: &'a [u8],
    pub p: usize,
}
impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Dec { b, p: 0 }
    }
    pub fn u32(&mut self) -> Option<u32> {
        let v = self.b.get(self.p..self.p + 4)?;
        self.p += 4;
        Some(u32::from_be_bytes(v.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        Some(((self.u32()? as u64) << 32) | self.u32()? as u64)
    }
    pub fn fixed(&mut self, n: usize) -> Option<&'a [u8]> {
        let v = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(v)
    }
    pub fn opaque(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        let v = self.b.get(self.p..self.p + n)?;
        self.p += (n + 3) & !3;
        Some(v)
    }
    pub fn skip(&mut self, n: usize) -> Option<()> {
        if self.p + n > self.b.len() {
            return None;
        }
        self.p += n;
        Some(())
    }
}

#[derive(Default)]
pub struct Enc {
    pub b: Vec<u8>,
}
impl Enc {
    pub fn u32(&mut self, v: u32) {
        self.b.extend_from_slice(&v.to_be_bytes())
    }
    pub fn u64(&mut self, v: u64) {
        self.b.extend_from_slice(&v.to_be_bytes())
    }
    pub fn opaque(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.b.extend_from_slice(v);
        self.pad(v.len());
    }
    pub fn pad(&mut self, n: usize) {
        for _ in 0..((4 - n % 4) % 4) {
            self.b.push(0)
        }
    }
    pub fn raw(&mut self, v: &[u8]) {
        self.b.extend_from_slice(v)
    }
}

// ── RPC-over-RDMA transport header ───────────────────────────────────
pub const RDMA_MSG: u32 = 0;
pub const RDMA_NOMSG: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Seg {
    pub handle: u32,
    pub length: u32,
    pub offset: u64,
}

#[derive(Debug)]
pub struct Hdr {
    pub xid: u32,
    #[allow(dead_code)] // the client's request; we grant CREDITS
    pub credits: u32,
    pub proc_: u32,
    /// (position, segment) — segments sharing a position form one chunk.
    pub read_list: Vec<(u32, Seg)>,
    pub write_list: Vec<Vec<Seg>>,
    pub reply_chunk: Option<Vec<Seg>>,
}

fn seg(d: &mut Dec) -> Option<Seg> {
    Some(Seg { handle: d.u32()?, length: d.u32()?, offset: d.u64()? })
}

/// Parse a transport header; returns it and the offset where the RPC
/// message begins (for RDMA_MSG).
pub fn parse_hdr(msg: &[u8]) -> Option<(Hdr, usize)> {
    let mut d = Dec::new(msg);
    let xid = d.u32()?;
    let vers = d.u32()?;
    if vers != 1 {
        return None;
    }
    let credits = d.u32()?;
    let proc_ = d.u32()?;
    let mut read_list = Vec::new();
    while d.u32()? == 1 {
        let pos = d.u32()?;
        read_list.push((pos, seg(&mut d)?));
    }
    let mut write_list = Vec::new();
    while d.u32()? == 1 {
        let n = d.u32()?;
        write_list.push((0..n).map(|_| seg(&mut d)).collect::<Option<Vec<_>>>()?);
    }
    let reply_chunk = if d.u32()? == 1 {
        let n = d.u32()?;
        Some((0..n).map(|_| seg(&mut d)).collect::<Option<Vec<_>>>()?)
    } else {
        None
    };
    Some((Hdr { xid, credits, proc_, read_list, write_list, reply_chunk }, d.p))
}

// ── RDMA seam ────────────────────────────────────────────────────────
/// The transfers a Responder issues. `data()` is the registered buffer
/// both directions stage through.
pub trait Rdma {
    fn data(&mut self) -> &mut [u8];
    /// Post an RDMA Write of data()[local..local+len] to `seg`.
    fn write(&mut self, local: usize, len: u32, seg: &Seg) -> Result<(), String>;
    /// Post an RDMA Read of `seg` into data()[local..].
    fn read(&mut self, local: usize, seg: &Seg) -> Result<(), String>;
    /// Wait for every posted transfer.
    fn drain(&mut self) -> Result<(), String>;
}

/// RDMA Read a whole chunk (all its segments, in order) into data()[0..].
fn pull_chunk(rdma: &mut dyn Rdma, segs: &[Seg]) -> Result<usize, String> {
    let mut off = 0usize;
    for s in segs {
        if off + s.length as usize > rdma.data().len() {
            return Err(format!("read chunk {} bytes exceeds the data buffer", off + s.length as usize));
        }
        rdma.read(off, s)?;
        off += s.length as usize;
    }
    rdma.drain()?;
    Ok(off)
}

/// RDMA Write data()[0..len] across `segs`; returns the per-segment
/// lengths actually written (0 for segments past the payload).
fn push_chunk(rdma: &mut dyn Rdma, len: usize, segs: &[Seg]) -> Result<Vec<u32>, String> {
    let cap: usize = segs.iter().map(|s| s.length as usize).sum();
    if len > cap {
        return Err(format!("{len} bytes do not fit a {cap}-byte write chunk"));
    }
    let mut off = 0usize;
    let mut used = Vec::with_capacity(segs.len());
    for s in segs {
        let n = (len - off).min(s.length as usize);
        if n > 0 {
            rdma.write(off, n as u32, s)?;
        }
        used.push(n as u32);
        off += n;
    }
    Ok(used)
}

// ── DS state ─────────────────────────────────────────────────────────
const OP_PUTFH: u32 = 22;
const OP_READ: u32 = 25;
const OP_WRITE: u32 = 38;
const OP_COMMIT: u32 = 5;
const OP_GETATTR: u32 = 9;
const OP_EXCHANGE_ID: u32 = 42;
const OP_CREATE_SESSION: u32 = 43;
const OP_DESTROY_SESSION: u32 = 44;
const OP_SEQUENCE: u32 = 53;
const OP_DESTROY_CLIENTID: u32 = 57;
const OP_RECLAIM_COMPLETE: u32 = 58;
pub const NFS4_OK: u32 = 0;
// RFC 8881 §18.35.3
const EXCHGID4_FLAG_USE_PNFS_DS: u32 = 0x0004_0000;
const NFS4ERR_IO: u32 = 5;
const NFS4ERR_INVAL: u32 = 22;
const NFS4ERR_NOFILEHANDLE: u32 = 10020;
pub const NFS4ERR_NOTSUPP: u32 = 10004;
const NFS4ERR_BADXDR: u32 = 10036;

const SESSION_ID: [u8; 16] = *b"rpcrdma-proto-01";
const MAX_SLOTS: u32 = 16;
/// Credits granted; must not exceed the receives the shim posts.
pub const CREDITS: u32 = 32;
/// The client's inline receive threshold (xprtrdma's default
/// rdma_max_inline_read). A reply larger than this needs a Reply chunk.
const CLIENT_INLINE_RECV: usize = 4096;

#[derive(Default, Debug, Clone, Copy)]
pub struct Counters {
    pub reads: u64,
    pub writes: u64,
    pub commits: u64,
    /// Payload moved by RDMA Write (READ data) / RDMA Read (WRITE data).
    pub rdma_written: u64,
    pub rdma_read: u64,
    /// Payload that travelled inline (below the client's chunk threshold).
    pub inline_read: u64,
    pub inline_written: u64,
}

pub struct Ds {
    root: PathBuf,
    files: HashMap<Vec<u8>, File>,
    verf: [u8; 8],
    pub counters: Counters,
}

impl Ds {
    pub fn new(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).expect("create DS store dir");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        Ds { root, files: HashMap::new(), verf: nanos.to_be_bytes(), counters: Counters::default() }
    }

    /// One backing file per DS filehandle, named by its hex.
    fn file(&mut self, fh: &[u8]) -> std::io::Result<&File> {
        if !self.files.contains_key(fh) {
            let name: String = fh.iter().map(|b| format!("{b:02x}")).collect();
            let f = OpenOptions::new().read(true).write(true).create(true).open(self.root.join(name))?;
            self.files.insert(fh.to_vec(), f);
        }
        Ok(&self.files[fh])
    }

    #[cfg(test)]
    pub fn path_of(&self, fh: &[u8]) -> PathBuf {
        self.root.join(fh.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }
}

// ── one call ─────────────────────────────────────────────────────────
/// Handle one inbound RPC-over-RDMA message; returns the complete reply
/// message (transport header + RPC reply), or None to stay silent.
/// RDMA Writes for READ data are POSTED here; the caller's Send must
/// follow on the same QP (RC ordering places them first).
pub fn handle(msg: &[u8], rdma: &mut dyn Rdma, ds: &mut Ds) -> Result<Option<Vec<u8>>, String> {
    // The counters say what was DELIVERED: a call whose reply cannot be
    // returned must not leave its bytes counted.
    let before = ds.counters;
    let r = handle_inner(msg, rdma, ds);
    if r.is_err() {
        ds.counters = before;
    }
    r
}

fn handle_inner(msg: &[u8], rdma: &mut dyn Rdma, ds: &mut Ds) -> Result<Option<Vec<u8>>, String> {
    let (hdr, body_at) = parse_hdr(msg).ok_or("unparseable transport header")?;

    // The call: inline (RDMA_MSG), or wholly in the position-zero Read
    // chunk (RDMA_NOMSG — the client's choice when it does not fit).
    let call: Vec<u8> = match hdr.proc_ {
        RDMA_MSG => msg[body_at..].to_vec(),
        RDMA_NOMSG => {
            let segs: Vec<Seg> = hdr.read_list.iter().filter(|(p, _)| *p == 0).map(|(_, s)| *s).collect();
            if segs.is_empty() {
                return Err("RDMA_NOMSG without a position-zero read chunk".into());
            }
            let n = pull_chunk(rdma, &segs)?;
            rdma.data()[..n].to_vec()
        }
        p => return Err(format!("rdma proc {p} not supported")),
    };

    let mut d = Dec::new(&call);
    let rxid = d.u32().ok_or("short call")?;
    let mtype = d.u32().ok_or("short call")?;
    d.u32();
    let prog = d.u32().ok_or("short call")?;
    d.u32();
    let rproc = d.u32().ok_or("short call")?;
    if mtype != 0 || prog != 100003 || rxid != hdr.xid {
        return Err(format!("not an NFS call: mtype {mtype} prog {prog}"));
    }
    for _ in 0..2 {
        d.u32().ok_or("short cred")?;
        d.opaque().ok_or("short cred")?;
    }

    let mut rpc = Enc::default();
    rpc.u32(hdr.xid);
    rpc.u32(1); // REPLY
    rpc.u32(0); // MSG_ACCEPTED
    rpc.u32(0);
    rpc.u32(0); // verf AUTH_NONE
    rpc.u32(0); // SUCCESS

    // Write chunks the reply used: index → per-segment lengths.
    let mut used_writes: Vec<Option<Vec<u32>>> = vec![None; hdr.write_list.len()];
    match rproc {
        0 => {}
        1 => compound(&mut d, &hdr, &mut rpc, rdma, ds, &mut used_writes)?,
        p => return Err(format!("rpc proc {p}")),
    }

    // Transport header for the reply. Every Write chunk the client
    // offered is echoed with the lengths actually written (0 = unused).
    let header = |proc_: u32, reply: Option<(&[Seg], usize)>| {
        let mut h = Enc::default();
        h.u32(hdr.xid);
        h.u32(1);
        h.u32(CREDITS);
        h.u32(proc_);
        h.u32(0); // read list: never in a reply
        for (i, chunk) in hdr.write_list.iter().enumerate() {
            h.u32(1);
            h.u32(chunk.len() as u32);
            for (j, s) in chunk.iter().enumerate() {
                let n = used_writes[i].as_ref().map_or(0, |u| u[j]);
                h.u32(s.handle);
                h.u32(n);
                h.u64(s.offset);
            }
        }
        h.u32(0);
        match reply {
            None => h.u32(0),
            Some((segs, len)) => {
                h.u32(1);
                h.u32(segs.len() as u32);
                let mut left = len;
                for s in segs {
                    let n = left.min(s.length as usize);
                    h.u32(s.handle);
                    h.u32(n as u32);
                    h.u64(s.offset);
                    left -= n;
                }
            }
        }
        h.b
    };

    let inline_hdr = header(RDMA_MSG, None);
    if inline_hdr.len() + rpc.b.len() <= CLIENT_INLINE_RECV {
        let mut out = inline_hdr;
        out.extend_from_slice(&rpc.b);
        return Ok(Some(out));
    }
    // Too big to send inline: RDMA Write it into the Reply chunk.
    let segs = hdr.reply_chunk.as_ref().ok_or(format!(
        "reply of {} bytes exceeds the client's inline threshold and no reply chunk was offered",
        rpc.b.len()
    ))?;
    if rpc.b.len() > rdma.data().len() {
        return Err("reply larger than the data buffer".into());
    }
    rdma.data()[..rpc.b.len()].copy_from_slice(&rpc.b);
    push_chunk(rdma, rpc.b.len(), segs)?;
    Ok(Some(header(RDMA_NOMSG, Some((segs, rpc.b.len())))))
}

fn compound(
    d: &mut Dec,
    hdr: &Hdr,
    rpc: &mut Enc,
    rdma: &mut dyn Rdma,
    ds: &mut Ds,
    used_writes: &mut [Option<Vec<u32>>],
) -> Result<(), String> {
    let _tag = d.opaque().ok_or("short compound")?;
    let _minor = d.u32().ok_or("short compound")?;
    let numops = d.u32().ok_or("short compound")?;
    let mut results: Vec<(u32, u32, Vec<u8>)> = Vec::new();
    let mut overall = NFS4_OK;
    let mut cfh: Option<Vec<u8>> = None;
    let mut next_write_chunk = 0usize;

    for _ in 0..numops {
        let op = d.u32().ok_or("short op")?;
        let r = match op {
            OP_SEQUENCE => op_sequence(d),
            OP_PUTFH => match d.opaque() {
                Some(fh) => {
                    cfh = Some(fh.to_vec());
                    Some((NFS4_OK, Vec::new()))
                }
                None => None,
            },
            OP_READ => op_read(d, cfh.as_deref(), hdr, rdma, ds, &mut next_write_chunk, used_writes)?,
            OP_WRITE => op_write(d, cfh.as_deref(), hdr, rdma, ds)?,
            OP_COMMIT => op_commit(d, cfh.as_deref(), ds),
            OP_EXCHANGE_ID => op_exchange_id(d),
            OP_CREATE_SESSION => op_create_session(d),
            OP_RECLAIM_COMPLETE => d.u32().map(|_| (NFS4_OK, Vec::new())),
            OP_DESTROY_SESSION => d.skip(16).map(|_| (NFS4_OK, Vec::new())),
            OP_DESTROY_CLIENTID => d.u64().map(|_| (NFS4_OK, Vec::new())),
            other => {
                eprintln!("🛑 op {other} → NOTSUPP");
                Some((NFS4ERR_NOTSUPP, Vec::new()))
            }
        };
        let (st, body) = r.unwrap_or((NFS4ERR_BADXDR, Vec::new()));
        results.push((op, st, body));
        if st != NFS4_OK {
            overall = st;
            break;
        }
    }
    let _ = OP_GETATTR;

    rpc.u32(overall);
    rpc.opaque(b"");
    rpc.u32(results.len() as u32);
    for (op, st, body) in results {
        rpc.u32(op);
        rpc.u32(st);
        rpc.raw(&body);
    }
    Ok(())
}

fn op_sequence(d: &mut Dec) -> Option<(u32, Vec<u8>)> {
    let sid = d.fixed(16)?.to_vec();
    let seq = d.u32()?;
    let slot = d.u32()?;
    let _highest = d.u32()?;
    let _cache = d.u32()?;
    let mut e = Enc::default();
    e.raw(&sid);
    e.u32(seq);
    e.u32(slot);
    e.u32(MAX_SLOTS - 1);
    e.u32(MAX_SLOTS - 1);
    e.u32(0);
    Some((NFS4_OK, e.b))
}

fn op_exchange_id(d: &mut Dec) -> Option<(u32, Vec<u8>)> {
    d.skip(8)?; // verifier
    let owner = d.opaque()?.to_vec();
    let flags = d.u32()?;
    let sp = d.u32()?;
    if sp != 0 {
        return Some((NFS4ERR_INVAL, Vec::new()));
    }
    // client_impl_id<1>
    if d.u32()? == 1 {
        d.opaque()?;
        d.opaque()?;
        d.skip(12)?;
    }
    eprintln!("📥 EXCHANGE_ID owner={:?} flags={flags:#x}", String::from_utf8_lossy(&owner));
    let mut e = Enc::default();
    e.u64(0x1234_5678_9abc_def0); // clientid
    e.u32(1); // sequenceid
    // The DS role. M1 sent 0x0002_0000 labelled "USE_PNFS_DS" — that is
    // USE_PNFS_MDS, which a direct mount accepts, but the pNFS client
    // drops a data server that does not claim the DS role and silently
    // falls back to I/O through the MDS.
    e.u32(EXCHGID4_FLAG_USE_PNFS_DS);
    e.u32(0); // SP4_NONE
    e.u64(0); // server_owner minor
    e.opaque(b"rpcrdma-ds-proto");
    e.opaque(b"rpcrdma-ds-proto");
    e.u32(0); // impl_id: empty
    Some((NFS4_OK, e.b))
}

fn op_create_session(d: &mut Dec) -> Option<(u32, Vec<u8>)> {
    let _clientid = d.u64()?;
    let seq = d.u32()?;
    let _flags = d.u32()?;
    // channel_attrs4: headerpadsize, maxrequestsize, maxresponsesize,
    // maxresponsesize_cached, maxoperations, maxrequests, rdma_ird<1>.
    let mut chan = [[0u32; 6]; 2];
    for c in chan.iter_mut() {
        for v in c.iter_mut() {
            *v = d.u32()?;
        }
        let ird = d.u32()?;
        d.skip(ird as usize * 4)?;
    }
    let [fore, back] = chan;
    eprintln!("📥 CREATE_SESSION fore={fore:?} back={back:?}");
    let mut e = Enc::default();
    e.raw(&SESSION_ID);
    e.u32(seq);
    e.u32(0);
    // NEGOTIATE, never exceed: the client refuses a session whose
    // attributes are larger than it asked for (RFC 8881 §18.36.3; the
    // kernel's nfs4_verify_fore_channel_attrs answers EINVAL). An M2
    // draft that "added headroom" above the client's request broke the
    // handshake exactly there.
    e.u32(0); // headerpadsize
    e.u32(fore[1]);
    e.u32(fore[2]);
    e.u32(fore[3].min(4096));
    e.u32(fore[4]);
    e.u32(fore[5].min(MAX_SLOTS));
    e.u32(0); // rdma_ird: none
    // Back channel: we never call back; echo the client's.
    e.u32(0);
    for v in &back[1..] {
        e.u32(*v);
    }
    e.u32(0);
    Some((NFS4_OK, e.b))
}

type OpResult = Result<Option<(u32, Vec<u8>)>, String>;

fn op_read(
    d: &mut Dec,
    cfh: Option<&[u8]>,
    hdr: &Hdr,
    rdma: &mut dyn Rdma,
    ds: &mut Ds,
    next_write_chunk: &mut usize,
    used_writes: &mut [Option<Vec<u32>>],
) -> OpResult {
    let (Some(_stateid), Some(offset), Some(count)) = (d.fixed(16), d.u64(), d.u32()) else {
        return Ok(None);
    };
    let Some(fh) = cfh else { return Ok(Some((NFS4ERR_NOFILEHANDLE, Vec::new()))) };
    let count = (count as usize).min(rdma.data().len());
    let n = {
        let f = match ds.file(fh) {
            Ok(f) => f.try_clone().map_err(|e| e.to_string())?,
            Err(_) => return Ok(Some((NFS4ERR_IO, Vec::new()))),
        };
        let buf = &mut rdma.data()[..count];
        let mut got = 0;
        while got < count {
            match f.read_at(&mut buf[got..], offset + got as u64) {
                Ok(0) => break,
                Ok(k) => got += k,
                Err(_) => return Ok(Some((NFS4ERR_IO, Vec::new()))),
            }
        }
        got
    };
    let size = ds.file(fh).and_then(|f| f.metadata()).map(|m| m.len()).unwrap_or(0);
    let eof = offset + n as u64 >= size;
    ds.counters.reads += 1;

    let mut e = Enc::default();
    e.u32(eof as u32);
    e.u32(n as u32);
    if let Some(segs) = hdr.write_list.get(*next_write_chunk) {
        // DDP: bytes by RDMA Write, count inline, no bytes, no pad.
        used_writes[*next_write_chunk] = Some(push_chunk(rdma, n, segs)?);
        *next_write_chunk += 1;
        ds.counters.rdma_written += n as u64;
    } else {
        e.raw(&rdma.data()[..n]);
        e.pad(n);
        ds.counters.inline_read += n as u64;
    }
    Ok(Some((NFS4_OK, e.b)))
}

fn op_write(d: &mut Dec, cfh: Option<&[u8]>, hdr: &Hdr, rdma: &mut dyn Rdma, ds: &mut Ds) -> OpResult {
    let (Some(_stateid), Some(offset), Some(stable)) = (d.fixed(16), d.u64(), d.u32()) else {
        return Ok(None);
    };
    // The data item starts at the length word; a Read chunk for it is
    // positioned where its BYTES would begin, just past that word.
    let Some(len) = d.u32() else { return Ok(None) };
    let len = len as usize;
    let chunk: Vec<Seg> = hdr
        .read_list
        .iter()
        .filter(|(p, _)| *p != 0 && *p as usize == d.p)
        .map(|(_, s)| *s)
        .collect();
    let via_chunk = !chunk.is_empty();
    if via_chunk {
        let got = pull_chunk(rdma, &chunk)?;
        if got < len {
            return Err(format!("read chunk carried {got} bytes for a {len}-byte WRITE"));
        }
    } else {
        let Some(bytes) = d.fixed(len) else { return Ok(None) };
        if len > rdma.data().len() {
            return Ok(Some((NFS4ERR_INVAL, Vec::new())));
        }
        rdma.data()[..len].copy_from_slice(bytes);
        d.skip((4 - len % 4) % 4);
    }
    let Some(fh) = cfh else { return Ok(Some((NFS4ERR_NOFILEHANDLE, Vec::new()))) };
    let f = match ds.file(fh) {
        Ok(f) => f.try_clone().map_err(|e| e.to_string())?,
        Err(_) => return Ok(Some((NFS4ERR_IO, Vec::new()))),
    };
    if f.write_all_at(&rdma.data()[..len], offset).is_err() {
        return Ok(Some((NFS4ERR_IO, Vec::new())));
    }
    // Never claim FILE_SYNC4: a split pNFS DS cannot make the file's
    // SIZE durable — it lives on the MDS, which learns it only from the
    // client's LAYOUTCOMMIT, and the Linux files-layout client skips
    // LAYOUTCOMMIT for a FILE_SYNC DS write (filelayout_set_layoutcommit).
    // An M2 draft answered FILE_SYNC: the MDS kept a ~4 KiB size, clamped
    // every READ layout to it, and reads crawled one page per DS
    // connection. flint's own DS learned the same (ds/server.rs, fsx).
    // DATA_SYNC4 is the honest answer for any stable write.
    let (synced, committed) = match stable {
        0 => (Ok(()), 0),
        _ => (f.sync_data(), 1),
    };
    if synced.is_err() {
        return Ok(Some((NFS4ERR_IO, Vec::new())));
    }
    ds.counters.writes += 1;
    if via_chunk {
        ds.counters.rdma_read += len as u64;
    } else {
        ds.counters.inline_written += len as u64;
    }
    let mut e = Enc::default();
    e.u32(len as u32);
    e.u32(committed);
    e.raw(&ds.verf);
    Ok(Some((NFS4_OK, e.b)))
}

fn op_commit(d: &mut Dec, cfh: Option<&[u8]>, ds: &mut Ds) -> Option<(u32, Vec<u8>)> {
    d.u64()?;
    d.u32()?;
    let Some(fh) = cfh else { return Some((NFS4ERR_NOFILEHANDLE, Vec::new())) };
    let ok = ds.file(fh).and_then(|f| f.sync_all()).is_ok();
    if !ok {
        return Some((NFS4ERR_IO, Vec::new()));
    }
    ds.counters.commits += 1;
    Some((NFS4_OK, ds.verf.to_vec()))
}

#[cfg(test)]
mod tests;
