//! Chunk handling against a mock client: "remote memory" is one buffer
//! per rkey, addressed by the segment offset. Each DDP test has a
//! control that sends the SAME payload inline, so a pass cannot come
//! from the bytes having travelled some other way.

use super::*;

#[derive(Default)]
struct Mock {
    data: Vec<u8>,
    remote: HashMap<u32, Vec<u8>>,
    writes: usize,
    reads: usize,
}

impl Mock {
    fn new() -> Self {
        Mock { data: vec![0; 4 << 20], ..Default::default() }
    }
}

impl Rdma for Mock {
    fn data(&mut self) -> &mut [u8] {
        &mut self.data
    }
    fn write(&mut self, local: usize, len: u32, s: &Seg) -> Result<(), String> {
        assert!(len <= s.length, "an RDMA Write must not overrun its segment");
        let r = self.remote.entry(s.handle).or_default();
        let end = s.offset as usize + len as usize;
        if r.len() < end {
            r.resize(end, 0);
        }
        r[s.offset as usize..end].copy_from_slice(&self.data[local..local + len as usize]);
        self.writes += 1;
        Ok(())
    }
    fn read(&mut self, local: usize, s: &Seg) -> Result<(), String> {
        let r = self.remote.get(&s.handle).ok_or("read of an unregistered rkey")?;
        let src = &r[s.offset as usize..s.offset as usize + s.length as usize];
        self.data[local..local + src.len()].copy_from_slice(src);
        self.reads += 1;
        Ok(())
    }
    fn drain(&mut self) -> Result<(), String> {
        Ok(())
    }
}

const FH: &[u8] = b"fh-0001";

fn pattern(n: usize, salt: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt)).collect()
}

/// RPC call header + COMPOUND(SEQUENCE, PUTFH, <op>) — `op` is the
/// already-encoded last op. Returns the call and the offset `op` starts at.
fn call(xid: u32, op: &[u8]) -> (Vec<u8>, usize) {
    let mut e = Enc::default();
    e.u32(xid);
    e.u32(0); // CALL
    e.u32(2);
    e.u32(100003);
    e.u32(4);
    e.u32(1); // COMPOUND
    e.u32(0);
    e.u32(0); // cred AUTH_NONE
    e.u32(0);
    e.u32(0); // verf
    e.opaque(b"");
    e.u32(1); // minorversion
    e.u32(3);
    e.u32(OP_SEQUENCE);
    e.raw(&SESSION_ID);
    e.u32(1);
    e.u32(0);
    e.u32(0);
    e.u32(0);
    e.u32(OP_PUTFH);
    e.opaque(FH);
    let at = e.b.len();
    e.raw(op);
    (e.b, at)
}

fn transport(xid: u32, read_list: &[(u32, Seg)], write_list: &[Vec<Seg>], body: &[u8]) -> Vec<u8> {
    let mut e = Enc::default();
    e.u32(xid);
    e.u32(1);
    e.u32(CREDITS);
    e.u32(RDMA_MSG);
    for (pos, s) in read_list {
        e.u32(1);
        e.u32(*pos);
        e.u32(s.handle);
        e.u32(s.length);
        e.u64(s.offset);
    }
    e.u32(0);
    for chunk in write_list {
        e.u32(1);
        e.u32(chunk.len() as u32);
        for s in chunk {
            e.u32(s.handle);
            e.u32(s.length);
            e.u64(s.offset);
        }
    }
    e.u32(0);
    e.u32(0); // no reply chunk
    e.raw(body);
    e.b
}

/// Split a reply into its transport header and the results of the
/// LAST op (status, body bytes to the end of the message).
fn last_op(reply: &[u8]) -> (Hdr, u32, u32, Vec<u8>) {
    let (h, at) = parse_hdr(reply).expect("reply header");
    assert_eq!(h.proc_, RDMA_MSG);
    let mut d = Dec::new(&reply[at..]);
    d.skip(6 * 4).unwrap(); // xid REPLY ACCEPTED verf(2) SUCCESS
    let overall = d.u32().unwrap();
    d.opaque().unwrap();
    assert_eq!(d.u32().unwrap(), 3, "SEQUENCE, PUTFH, and the op");
    d.u32().unwrap();
    assert_eq!(d.u32().unwrap(), NFS4_OK);
    d.skip(16 + 5 * 4).unwrap(); // SEQUENCE body
    d.u32().unwrap();
    assert_eq!(d.u32().unwrap(), NFS4_OK); // PUTFH
    let op = d.u32().unwrap();
    let st = d.u32().unwrap();
    assert_eq!(overall, st);
    (h, op, st, d.b[d.p..].to_vec())
}

fn write_op(offset: u64, len: u32, inline: Option<&[u8]>) -> Vec<u8> {
    let mut e = Enc::default();
    e.u32(OP_WRITE);
    e.raw(&[0; 16]);
    e.u64(offset);
    e.u32(2); // FILE_SYNC4
    e.u32(len);
    if let Some(b) = inline {
        e.raw(b);
        e.pad(b.len());
    }
    e.b
}

fn read_op(offset: u64, count: u32) -> Vec<u8> {
    let mut e = Enc::default();
    e.u32(OP_READ);
    e.raw(&[0; 16]);
    e.u64(offset);
    e.u32(count);
    e.b
}

/// WRITE whose payload is in a Read chunk: the DS pulls it by RDMA
/// Read from the position just past the inline length word. The inline
/// stream carries neither the bytes nor the pad (10001 is odd on
/// purpose). Control: the same payload inline lands identically and
/// counts as inline.
#[test]
fn write_payload_is_pulled_from_the_read_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let mut ds = Ds::new(dir.path().into());
    let mut m = Mock::new();
    let payload = pattern(10001, 7);

    let (body, op_at) = call(9, &write_op(4096, payload.len() as u32, None));
    let position = (op_at + 4 + 16 + 8 + 4 + 4) as u32; // past op, stateid, offset, stable, len
    m.remote.insert(0x77, payload.clone());
    let seg = Seg { handle: 0x77, length: payload.len() as u32, offset: 0 };
    let msg = transport(9, &[(position, seg)], &[], &body);

    let reply = handle(&msg, &mut m, &mut ds).unwrap().unwrap();
    let (_, op, st, res) = last_op(&reply);
    assert_eq!((op, st), (OP_WRITE, NFS4_OK));
    let mut d = Dec::new(&res);
    assert_eq!(d.u32().unwrap(), payload.len() as u32, "count");
    assert_eq!(
        d.u32().unwrap(),
        1,
        "a FILE_SYNC request is answered DATA_SYNC: the size is the MDS's, \
         and FILE_SYNC would make the client skip LAYOUTCOMMIT"
    );
    assert_eq!(m.reads, 1, "the payload came by RDMA Read");
    let on_disk = std::fs::read(ds.path_of(FH)).unwrap();
    assert_eq!(&on_disk[4096..], &payload[..]);
    assert_eq!(ds.counters.rdma_read, payload.len() as u64);
    assert_eq!(ds.counters.inline_written, 0);

    // Control: the same bytes inline, to a fresh offset.
    let (body, _) = call(10, &write_op(0, payload.len() as u32, Some(&payload)));
    let msg = transport(10, &[], &[], &body);
    let reply = handle(&msg, &mut m, &mut ds).unwrap().unwrap();
    assert_eq!(last_op(&reply).2, NFS4_OK);
    assert_eq!(m.reads, 1, "control: no RDMA Read for an inline WRITE");
    assert_eq!(&std::fs::read(ds.path_of(FH)).unwrap()[..payload.len()], &payload[..]);
    assert_eq!(ds.counters.inline_written, payload.len() as u64);
}

/// A Read chunk at the WRONG position is not this WRITE's data: the
/// DS must then expect the bytes inline and, finding none, fail the
/// op — never write whatever the chunk held.
#[test]
fn a_read_chunk_at_another_position_is_not_the_write_payload() {
    let dir = tempfile::tempdir().unwrap();
    let mut ds = Ds::new(dir.path().into());
    let mut m = Mock::new();
    let payload = pattern(10001, 3);
    let (body, op_at) = call(11, &write_op(0, payload.len() as u32, None));
    let wrong = (op_at + 4) as u32;
    m.remote.insert(0x77, payload.clone());
    let seg = Seg { handle: 0x77, length: payload.len() as u32, offset: 0 };
    let msg = transport(11, &[(wrong, seg)], &[], &body);
    let reply = handle(&msg, &mut m, &mut ds).unwrap().unwrap();
    assert_ne!(last_op(&reply).2, NFS4_OK);
    assert_eq!(m.reads, 0);
    assert_eq!(std::fs::metadata(ds.path_of(FH)).map(|m| m.len()).unwrap_or(0), 0);
}

/// READ with a Write chunk: the bytes go by RDMA Write across the
/// chunk's segments, the reply echoes the chunk with the lengths
/// actually written, and the inline READ result ends at the count —
/// no bytes, no pad. Control: no Write chunk → bytes inline + pad.
#[test]
fn read_payload_is_pushed_into_the_write_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let mut ds = Ds::new(dir.path().into());
    let mut m = Mock::new();
    let payload = pattern(10001, 9);
    std::fs::write(ds.path_of(FH), &payload).unwrap();

    let chunk = vec![
        Seg { handle: 0xA, length: 8192, offset: 0 },
        Seg { handle: 0xB, length: 8192, offset: 0 },
    ];
    let (body, _) = call(12, &read_op(0, 16384));
    let msg = transport(12, &[], &[chunk], &body);
    let reply = handle(&msg, &mut m, &mut ds).unwrap().unwrap();
    let (h, op, st, res) = last_op(&reply);
    assert_eq!((op, st), (OP_READ, NFS4_OK));

    assert_eq!(h.write_list.len(), 1, "the Write chunk is echoed");
    let lens: Vec<u32> = h.write_list[0].iter().map(|s| s.length).collect();
    assert_eq!(lens, vec![8192, 10001 - 8192], "echo carries the lengths written");
    assert_eq!(h.write_list[0][0].handle, 0xA);

    let mut d = Dec::new(&res);
    assert_eq!(d.u32().unwrap(), 1, "eof");
    assert_eq!(d.u32().unwrap(), 10001, "count stays inline");
    assert_eq!(d.p, res.len(), "no data bytes or pad inline");

    let mut got = m.remote[&0xA].clone();
    got.extend_from_slice(&m.remote[&0xB]);
    assert_eq!(got, payload);
    assert_eq!(ds.counters.rdma_written, 10001);

    // Control: no Write chunk — the same READ answers inline.
    let (body, _) = call(13, &read_op(0, 16384));
    let msg = transport(13, &[], &[], &body);
    let writes_before = m.writes;
    let reply = handle(&msg, &mut m, &mut ds);
    // 10001 bytes inline exceed the client's 4096-byte inline receive
    // and no Reply chunk was offered: that is an error, not a reply the
    // client cannot receive.
    assert!(reply.is_err(), "oversized inline reply without a reply chunk must fail");
    assert_eq!(m.writes, writes_before);

    let (body, _) = call(14, &read_op(0, 1001));
    let msg = transport(14, &[], &[], &body);
    let reply = handle(&msg, &mut m, &mut ds).unwrap().unwrap();
    let (_, _, st, res) = last_op(&reply);
    assert_eq!(st, NFS4_OK);
    let mut d = Dec::new(&res);
    assert_eq!(d.u32().unwrap(), 0, "not eof");
    assert_eq!(d.opaque().unwrap(), &payload[..1001]);
    assert_eq!(d.p, res.len(), "inline data is padded to a word");
    assert_eq!(ds.counters.inline_read, 1001);
}

/// A READ that comes back shorter than the chunk leaves trailing
/// segments at length 0 — and the RDMA Writes never overrun.
#[test]
fn short_read_echoes_zero_length_for_unused_segments() {
    let dir = tempfile::tempdir().unwrap();
    let mut ds = Ds::new(dir.path().into());
    let mut m = Mock::new();
    std::fs::write(ds.path_of(FH), pattern(100, 1)).unwrap();
    let chunk = vec![
        Seg { handle: 0xA, length: 4096, offset: 0 },
        Seg { handle: 0xB, length: 4096, offset: 0 },
    ];
    let (body, _) = call(15, &read_op(0, 8192));
    let reply = handle(&transport(15, &[], &[chunk], &body), &mut m, &mut ds).unwrap().unwrap();
    let (h, _, st, _) = last_op(&reply);
    assert_eq!(st, NFS4_OK);
    let lens: Vec<u32> = h.write_list[0].iter().map(|s| s.length).collect();
    assert_eq!(lens, vec![100, 0]);
    assert_eq!(m.writes, 1);
}
