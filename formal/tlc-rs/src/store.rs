//! What a search keeps, sized for runs that outgrow memory (TLC's layout):
//!  - the seen set holds 8-byte fingerprints only;
//!  - each worker appends (fingerprint, parent) records to its own trace
//!    log, buffered in memory and spilled to a file, so a counterexample is
//!    rebuilt by walking parents from disk;
//!  - a BFS level's states stay in memory up to a budget; beyond it they
//!    are serialized, in blocks, to a level file;
//!  - a checkpoint (fingerprints, the level about to be expanded, the
//!    trace logs' lengths) lets a killed run resume.

use crate::value::{Func, Value, R};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use std::os::unix::fs::FileExt;

pub type State = Box<[Value]>;

fn io<T>(what: &str, r: std::io::Result<T>) -> R<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

// ---- the fingerprint set ------------------------------------------------

const SHARDS: usize = 1024;

/// Open addressing over u64; 0 marks an empty slot (a fingerprint of 0 is
/// stored as 1, as TLC folds its own reserved value).
struct Table {
    t: Vec<u64>,
    n: usize,
}

impl Table {
    fn insert(&mut self, fp: u64) -> bool {
        if (self.n + 1) * 4 > self.t.len() * 3 {
            self.grow();
        }
        let mask = self.t.len() - 1;
        let mut i = fp as usize & mask;
        loop {
            let x = self.t[i];
            if x == fp {
                return false;
            }
            if x == 0 {
                self.t[i] = fp;
                self.n += 1;
                return true;
            }
            i = (i + 1) & mask;
        }
    }
    fn contains(&self, fp: u64) -> bool {
        let mask = self.t.len() - 1;
        let mut i = fp as usize & mask;
        loop {
            let x = self.t[i];
            if x == fp {
                return true;
            }
            if x == 0 {
                return false;
            }
            i = (i + 1) & mask;
        }
    }
    fn grow(&mut self) {
        let cap = (self.t.len() * 2).max(1024);
        let old = std::mem::replace(&mut self.t, vec![0; cap]);
        self.n = 0;
        for fp in old {
            if fp != 0 {
                self.insert(fp);
            }
        }
    }
}

pub struct FpSet {
    shards: Box<[Mutex<Table>]>,
}

impl Default for FpSet {
    fn default() -> FpSet {
        FpSet { shards: (0..SHARDS).map(|_| Mutex::new(Table { t: vec![0; 1024], n: 0 })).collect() }
    }
}

impl FpSet {
    #[inline]
    fn shard(&self, fp: u64) -> &Mutex<Table> {
        // high bits pick the shard; the table itself uses the low bits
        &self.shards[(fp >> 54) as usize % SHARDS]
    }
    /// true if fp was new
    pub fn insert(&self, fp: u64) -> bool {
        let fp = fp.max(1);
        self.shard(fp).lock().unwrap().insert(fp)
    }
    pub fn contains(&self, fp: u64) -> bool {
        let fp = fp.max(1);
        self.shard(fp).lock().unwrap().contains(fp)
    }
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().unwrap().n).sum()
    }
    fn save(&self, path: &Path) -> R<()> {
        let mut w = BufWriter::with_capacity(1 << 20, io("create", File::create(path))?);
        for s in self.shards.iter() {
            for &fp in s.lock().unwrap().t.iter().filter(|&&x| x != 0) {
                io("write", w.write_all(&fp.to_le_bytes()))?;
            }
        }
        io("write", w.flush())?;
        io("sync", w.get_ref().sync_all())
    }
    fn load(&self, path: &Path) -> R<()> {
        let mut r = BufReader::with_capacity(1 << 20, io("open", File::open(path))?);
        let mut b = [0u8; 8];
        loop {
            match r.read_exact(&mut b) {
                Ok(()) => {
                    self.insert(u64::from_le_bytes(b));
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(format!("reading fingerprints: {e}")),
            }
        }
    }
}

// ---- the metadir --------------------------------------------------------

/// The run's directory, created only when something first needs disk.
pub struct Meta {
    pub dir: PathBuf,
    made: Mutex<bool>,
}

impl Meta {
    pub fn new(dir: PathBuf) -> Meta {
        Meta { dir, made: Mutex::new(false) }
    }
    fn path(&self, name: &str) -> R<PathBuf> {
        let mut made = self.made.lock().unwrap();
        if !*made {
            io(&format!("creating {}", self.dir.display()), fs::create_dir_all(&self.dir))?;
            *made = true;
        }
        Ok(self.dir.join(name))
    }
    pub fn exists(&self) -> bool {
        *self.made.lock().unwrap()
    }
}

// ---- trace logs ---------------------------------------------------------

/// A state's trace index: the log (worker slot) in the top 8 bits, the
/// record's position in that log below.
pub const SLOT_SHIFT: u32 = 56;
pub const NO_PARENT: u64 = u64::MAX;

pub struct Log {
    slot: usize,
    file: Option<File>,
    /// records [0, flushed) are in the file, the rest in `buf`
    flushed: u64,
    buf: Vec<[u64; 2]>,
    cap: usize,
}

impl Log {
    /// Append (fp, parent); returns the new record's trace index.
    pub fn append(&mut self, meta: &Meta, fp: u64, parent: u64) -> R<u64> {
        if self.buf.len() >= self.cap {
            self.flush(meta)?;
        }
        let idx = ((self.slot as u64) << SLOT_SHIFT) | (self.flushed + self.buf.len() as u64);
        self.buf.push([fp, parent]);
        Ok(idx)
    }
    fn flush(&mut self, meta: &Meta) -> R<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            let p = meta.path(&format!("trace-{}.bin", self.slot))?;
            self.file = Some(io("trace log", OpenOptions::new().create(true).read(true).append(true).open(p))?);
        }
        let mut bytes = Vec::with_capacity(self.buf.len() * 16);
        for [a, b] in &self.buf {
            bytes.extend_from_slice(&a.to_le_bytes());
            bytes.extend_from_slice(&b.to_le_bytes());
        }
        io("trace log", self.file.as_mut().unwrap().write_all(&bytes))?;
        self.flushed += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }
    fn get(&self, seq: u64) -> R<[u64; 2]> {
        if seq >= self.flushed {
            return self.buf.get((seq - self.flushed) as usize).copied().ok_or_else(|| "trace index out of range".into());
        }
        let mut b = [0u8; 16];
        io("trace log", self.file.as_ref().unwrap().read_exact_at(&mut b, seq * 16))?;
        Ok([u64::from_le_bytes(b[..8].try_into().unwrap()), u64::from_le_bytes(b[8..].try_into().unwrap())])
    }
}

pub struct Trace {
    pub logs: Vec<Mutex<Log>>,
}

impl Trace {
    pub fn new(workers: usize) -> Trace {
        // at most ~32M records (512 MB) buffered in all; TLCRS_TRACE_BUF
        // (records per log) forces spilling, for testing
        let cap = std::env::var("TLCRS_TRACE_BUF").ok().and_then(|v| v.parse().ok()).unwrap_or((32 << 20) / workers.max(1)).max(1);
        Trace { logs: (0..workers.max(1)).map(|slot| Mutex::new(Log { slot, file: None, flushed: 0, buf: Vec::new(), cap })).collect() }
    }
    /// The fingerprints from an initial state to the state at `idx`.
    pub fn chain(&self, idx: u64) -> R<Vec<u64>> {
        let mut out = Vec::new();
        let mut cur = idx;
        while cur != NO_PARENT {
            let slot = (cur >> SLOT_SHIFT) as usize;
            let [fp, parent] = self.logs.get(slot).ok_or("bad trace index")?.lock().unwrap().get(cur & ((1 << SLOT_SHIFT) - 1))?;
            out.push(fp);
            cur = parent;
        }
        out.reverse();
        Ok(out)
    }
}

// ---- serialized states --------------------------------------------------

fn put_var(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn get_var(b: &[u8], pos: &mut usize) -> R<u64> {
    let mut n = 0u64;
    let mut shift = 0;
    loop {
        let x = *b.get(*pos).ok_or("truncated queue block")?;
        *pos += 1;
        n |= ((x & 0x7f) as u64) << shift;
        if x < 0x80 {
            return Ok(n);
        }
        shift += 7;
    }
}

pub fn encode(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Bool(b) => out.push(*b as u8),
        Value::Int(n) => {
            out.push(2);
            put_var(out, ((n << 1) ^ (n >> 63)) as u64);
        }
        Value::Str(s) => {
            out.push(3);
            put_var(out, *s as u64);
        }
        Value::Model(s) => {
            out.push(4);
            put_var(out, *s as u64);
        }
        Value::Set(xs) | Value::Seq(xs) => {
            out.push(if matches!(v, Value::Set(_)) { 5 } else { 6 });
            put_var(out, xs.len() as u64);
            xs.iter().for_each(|x| encode(x, out));
        }
        Value::Func(f) => {
            out.push(7);
            put_var(out, f.keys.len() as u64);
            f.keys.iter().for_each(|x| encode(x, out));
            f.vals.iter().for_each(|x| encode(x, out));
        }
        Value::Lazy(_) => unreachable!("a state variable is never lazy"),
    }
}

pub fn decode(b: &[u8], pos: &mut usize) -> R<Value> {
    let tag = *b.get(*pos).ok_or("truncated queue block")?;
    *pos += 1;
    let many = |pos: &mut usize, n: u64| (0..n).map(|_| decode(b, pos)).collect::<R<Vec<Value>>>();
    Ok(match tag {
        0 | 1 => Value::Bool(tag == 1),
        2 => {
            let z = get_var(b, pos)?;
            Value::Int(((z >> 1) as i64) ^ -((z & 1) as i64))
        }
        3 => Value::Str(get_var(b, pos)? as u32),
        4 => Value::Model(get_var(b, pos)? as u32),
        5 | 6 => {
            let n = get_var(b, pos)?;
            let xs: Arc<[Value]> = many(pos, n)?.into();
            if tag == 5 { Value::Set(xs) } else { Value::Seq(xs) }
        }
        7 => {
            let n = get_var(b, pos)?;
            let keys = many(pos, n)?;
            let vals = many(pos, n)?;
            Value::Func(Arc::new(Func { keys: keys.into(), vals: vals.into() }))
        }
        t => return Err(format!("bad value tag {t} in a queue block")),
    })
}

fn encode_states(states: &[(u64, State)], out: &mut Vec<u8>) {
    for (idx, st) in states {
        put_var(out, *idx);
        st.iter().for_each(|v| encode(v, out));
    }
}

pub fn decode_states(b: &[u8], count: usize, nvars: usize, out: &mut Vec<(u64, State)>) -> R<()> {
    let mut pos = 0;
    for _ in 0..count {
        let idx = get_var(b, &mut pos)?;
        let st: State = (0..nvars).map(|_| decode(b, &mut pos)).collect::<R<_>>()?;
        out.push((idx, st));
    }
    Ok(())
}

// ---- BFS levels ---------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Block {
    pub off: u64,
    pub len: u64,
    pub count: u64,
}

/// One BFS level: the states held in memory, then blocks in a file.
#[derive(Default)]
pub struct Level {
    pub mem: Vec<(u64, State)>,
    pub blocks: Vec<Block>,
    pub file: Option<(File, PathBuf)>,
}

impl Level {
    pub fn len(&self) -> u64 {
        self.mem.len() as u64 + self.blocks.iter().map(|b| b.count).sum::<u64>()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn read(&self, b: &Block, nvars: usize, out: &mut Vec<(u64, State)>) -> R<()> {
        let mut buf = vec![0u8; b.len as usize];
        io("queue file", self.file.as_ref().unwrap().0.read_exact_at(&mut buf, b.off))?;
        decode_states(&buf, b.count as usize, nvars, out)
    }
    /// The level's file is no longer needed (a checkpoint keeps its own
    /// link to it).
    pub fn discard(self) {
        if let Some((_, p)) = self.file {
            if !p.as_os_str().is_empty() {
                let _ = fs::remove_file(p);
            }
        }
    }
}

/// The next level, as the workers produce it.
pub struct LevelWriter<'a> {
    meta: &'a Meta,
    name: String,
    budget: u64,
    mem_bytes: AtomicU64,
    mem: Mutex<Vec<(u64, State)>>,
    disk: Mutex<(Option<(File, PathBuf)>, u64, Vec<Block>)>,
}

impl<'a> LevelWriter<'a> {
    pub fn new(meta: &'a Meta, depth: usize, budget: u64) -> LevelWriter<'a> {
        LevelWriter {
            meta,
            name: format!("queue-{depth}.bin"),
            budget,
            mem_bytes: AtomicU64::new(0),
            mem: Mutex::new(Vec::new()),
            disk: Mutex::new((None, 0, Vec::new())),
        }
    }
    /// Hand over a worker's batch: kept in memory while the level's
    /// estimated size is under the budget, else written out as a block.
    pub fn push(&self, batch: &mut Vec<(u64, State)>, enc: &mut Vec<u8>) -> R<()> {
        if batch.is_empty() {
            return Ok(());
        }
        // estimate from the first state's encoding: a tree in memory costs
        // about 5x its serialized bytes (measured: FlintTierSession)
        enc.clear();
        encode_states(&batch[..1], enc);
        let est = enc.len() as u64 * 5 * batch.len() as u64;
        if self.mem_bytes.fetch_add(est, Ordering::Relaxed) + est <= self.budget {
            self.mem.lock().unwrap().append(batch);
            return Ok(());
        }
        self.mem_bytes.fetch_sub(est, Ordering::Relaxed);
        enc.clear();
        encode_states(batch, enc);
        let mut d = self.disk.lock().unwrap();
        if d.0.is_none() {
            let p = self.meta.path(&self.name)?;
            let f = io("queue file", OpenOptions::new().create(true).truncate(true).read(true).write(true).open(&p))?;
            d.0 = Some((f, p));
        }
        let off = d.1;
        io("queue file", d.0.as_ref().unwrap().0.write_all_at(enc, off))?;
        d.1 += enc.len() as u64;
        d.2.push(Block { off, len: enc.len() as u64, count: batch.len() as u64 });
        batch.clear();
        Ok(())
    }
    pub fn finish(self) -> Level {
        let (file, _, blocks) = self.disk.into_inner().unwrap();
        Level { mem: self.mem.into_inner().unwrap(), blocks, file }
    }
}

// ---- checkpoints --------------------------------------------------------

pub struct Snapshot {
    pub depth: usize,
    pub generated: u64,
    pub level: Level,
}

/// Write a checkpoint of the search as it stands before expanding
/// `level`: into `ckpt.tmp`, then swapped in for `ckpt`.
pub fn checkpoint(meta: &Meta, source_hash: u64, fps: &FpSet, trace: &Trace, depth: usize, generated: u64, level: &Level) -> R<()> {
    let tmp = meta.path("ckpt.tmp")?;
    let _ = fs::remove_dir_all(&tmp);
    io("checkpoint", fs::create_dir_all(&tmp))?;
    fps.save(&tmp.join("fps.bin"))?;
    let mut lens = Vec::new();
    for l in &trace.logs {
        let mut l = l.lock().unwrap();
        l.flush(meta)?;
        if let Some(f) = &l.file {
            io("sync", f.sync_all())?;
        }
        lens.push(l.flushed);
    }
    // the level: in-memory states serialized; blocks already on disk linked
    let mut enc = Vec::new();
    encode_states(&level.mem, &mut enc);
    let mut f = io("checkpoint", File::create(tmp.join("queue-mem.bin")))?;
    io("checkpoint", f.write_all(&enc))?;
    io("sync", f.sync_all())?;
    if let Some((qf, p)) = &level.file {
        io("sync", qf.sync_all())?;
        // an empty path: the level was recovered from the current checkpoint
        let src = if p.as_os_str().is_empty() { meta.dir.join("ckpt").join("queue-disk.bin") } else { p.clone() };
        io("checkpoint", fs::hard_link(src, tmp.join("queue-disk.bin")))?;
    }
    let mut m = String::new();
    m += &format!("source_hash {source_hash}\ndepth {depth}\ngenerated {generated}\nmem {}\n", level.mem.len());
    m += &format!("trace {}\n", lens.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(" "));
    for b in &level.blocks {
        m += &format!("block {} {} {}\n", b.off, b.len, b.count);
    }
    let mut f = io("checkpoint", File::create(tmp.join("meta.txt")))?;
    io("checkpoint", f.write_all(m.as_bytes()))?;
    io("sync", f.sync_all())?;
    let ck = meta.dir.join("ckpt");
    let old = meta.dir.join("ckpt.old");
    let _ = fs::remove_dir_all(&old);
    if ck.exists() {
        io("checkpoint", fs::rename(&ck, &old))?;
    }
    io("checkpoint", fs::rename(&tmp, &ck))?;
    let _ = fs::remove_dir_all(&old);
    if let Ok(d) = File::open(&meta.dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Is there a checkpoint in `dir`, written for these sources?
pub fn check_recoverable(dir: &Path, source_hash: u64) -> R<()> {
    let m = dir.join("ckpt").join("meta.txt");
    let text = io(&format!("no checkpoint to recover ({})", m.display()), fs::read_to_string(&m))?;
    match text.lines().find_map(|l| l.strip_prefix("source_hash ")) {
        Some(h) if h.trim() == source_hash.to_string() => Ok(()),
        _ => Err(format!("the checkpoint in {} was written for different sources (spec or cfg changed); refusing to recover", dir.display())),
    }
}

/// Load the checkpoint in `meta.dir`: fills `fps`, truncates and reopens
/// the trace logs, and returns the level to expand next.
pub fn recover(meta: &Meta, source_hash: u64, nvars: usize, fps: &FpSet, trace: &mut Trace) -> R<Snapshot> {
    let ck = meta.dir.join("ckpt");
    let text = io(&format!("reading {}", ck.join("meta.txt").display()), fs::read_to_string(ck.join("meta.txt")))?;
    let (mut depth, mut generated, mut nmem, mut lens, mut blocks) = (0, 0, 0, Vec::new(), Vec::new());
    for line in text.lines() {
        let mut w = line.split_whitespace();
        let key = w.next().unwrap_or("");
        let nums: Vec<u64> = w.map(|x| x.parse().map_err(|_| format!("bad checkpoint line: {line}"))).collect::<R<_>>()?;
        match key {
            "source_hash" if nums[0] != source_hash => {
                return Err("the checkpoint was written for different sources (spec or cfg changed); refusing to recover".into());
            }
            "source_hash" => {}
            "depth" => depth = nums[0] as usize,
            "generated" => generated = nums[0],
            "mem" => nmem = nums[0] as usize,
            "trace" => lens = nums,
            "block" => blocks.push(Block { off: nums[0], len: nums[1], count: nums[2] }),
            _ => return Err(format!("bad checkpoint line: {line}")),
        }
    }
    *meta.made.lock().unwrap() = true;
    fps.load(&ck.join("fps.bin"))?;
    if trace.logs.len() < lens.len() {
        let cap = trace.logs[0].lock().unwrap().cap;
        for slot in trace.logs.len()..lens.len() {
            trace.logs.push(Mutex::new(Log { slot, file: None, flushed: 0, buf: Vec::new(), cap }));
        }
    }
    for (slot, &len) in lens.iter().enumerate() {
        let p = meta.dir.join(format!("trace-{slot}.bin"));
        if len == 0 {
            // records written after the checkpoint are not part of it
            let _ = fs::remove_file(&p);
            continue;
        }
        let f = io("trace log", OpenOptions::new().read(true).append(true).open(&p))?;
        io("trace log", f.set_len(len * 16))?;
        let mut l = trace.logs[slot].lock().unwrap();
        l.file = Some(f);
        l.flushed = len;
    }
    let buf = io("checkpoint", fs::read(ck.join("queue-mem.bin")))?;
    let mut mem = Vec::with_capacity(nmem);
    decode_states(&buf, nmem, nvars, &mut mem)?;
    let file = if blocks.is_empty() {
        None
    } else {
        // read through the checkpoint's own link; never deleted by the run
        let p = ck.join("queue-disk.bin");
        Some((io("checkpoint", File::open(&p))?, PathBuf::new()))
    };
    Ok(Snapshot { depth, generated, level: Level { mem, blocks, file } })
}
