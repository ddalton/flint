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

pub(crate) fn io<T>(what: &str, r: std::io::Result<T>) -> R<T> {
    r.map_err(|e| format!("{what}: {e}"))
}

// ---- open files -----------------------------------------------------------

/// A spilled seen set holds one open file per shard (1024), beside the
/// trace logs and queue files: more than Linux's default soft limit of
/// 1024 descriptors. Raise the soft limit toward the hard one, as the JVM
/// does for TLC (HotSpot's MaxFDLimit).
#[cfg(unix)]
pub fn raise_fd_limit() {
    #[repr(C)]
    struct Rlimit {
        cur: u64,
        max: u64,
    }
    unsafe extern "C" {
        fn getrlimit(resource: i32, rlim: *mut Rlimit) -> i32;
        fn setrlimit(resource: i32, rlim: *const Rlimit) -> i32;
    }
    #[cfg(target_os = "linux")]
    const RLIMIT_NOFILE: i32 = 7;
    #[cfg(not(target_os = "linux"))]
    const RLIMIT_NOFILE: i32 = 8;
    const WANT: u64 = 65536;
    let mut r = Rlimit { cur: 0, max: 0 };
    // SAFETY: plain libc calls on a properly laid out struct
    unsafe {
        if getrlimit(RLIMIT_NOFILE, &mut r) == 0 && r.cur < WANT {
            let want = Rlimit { cur: WANT.min(r.max), max: r.max };
            setrlimit(RLIMIT_NOFILE, &want);
        }
    }
}

#[cfg(not(unix))]
pub fn raise_fd_limit() {}

// ---- the fingerprint set ------------------------------------------------

const SHARDS: usize = 1024;
/// fingerprints per block of a shard's disk run (one read per lookup)
const BLOCK: usize = 128;

/// Open addressing over u64; 0 marks an empty slot (a fingerprint of 0 is
/// stored as 1, as TLC folds its own reserved value).
struct Table {
    t: Vec<u64>,
    n: usize,
}

impl Table {
    fn insert_new(&mut self, fp: u64) {
        if (self.n + 1) * 4 > self.t.len() * 3 {
            self.grow();
        }
        let mask = self.t.len() - 1;
        let mut i = fp as usize & mask;
        while self.t[i] != 0 {
            i = (i + 1) & mask;
        }
        self.t[i] = fp;
        self.n += 1;
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
                self.insert_new(fp);
            }
        }
    }
    /// Would the next insert grow the table past `max_len` slots?
    fn full(&self, max_len: usize) -> bool {
        (self.n + 1) * 4 > self.t.len() * 3 && self.t.len() * 2 > max_len
    }
}

/// A shard's fingerprints on disk: one sorted file, and the first
/// fingerprint of each BLOCK of it.
#[derive(Default)]
struct Run {
    file: Option<(File, PathBuf)>,
    len: u64,
    index: Vec<u64>,
    /// a Bloom filter over the run (~10 bits a fingerprint, 4 probes), so
    /// a new fingerprint rarely costs a read
    bloom: Vec<u64>,
}

/// The filter's bit positions for fp: slices of a remix of it (the shard
/// and the table already use fp's own high and low bits).
#[inline]
fn probes(fp: u64, nbits: u64) -> [u64; 4] {
    let h = fp.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let g = (fp ^ (fp >> 29)).wrapping_mul(0xBF58_476D_1CE4_E5B9) | 1;
    let m = nbits - 1;
    [h & m, h.wrapping_add(g) & m, h.wrapping_add(g.wrapping_mul(2)) & m, h.wrapping_add(g.wrapping_mul(3)) & m]
}

fn bloom_for(len: u64) -> Vec<u64> {
    vec![0; ((len * 10).max(64).next_power_of_two() / 64) as usize]
}

fn bloom_add(b: &mut [u64], fp: u64) {
    for p in probes(fp, b.len() as u64 * 64) {
        b[(p / 64) as usize] |= 1 << (p % 64);
    }
}

impl Run {
    fn contains(&self, fp: u64) -> R<bool> {
        let Some((f, _)) = &self.file else { return Ok(false) };
        if probes(fp, self.bloom.len() as u64 * 64).iter().any(|p| self.bloom[(p / 64) as usize] & (1 << (p % 64)) == 0) {
            return Ok(false);
        }
        let b = self.index.partition_point(|&x| x <= fp);
        if b == 0 {
            return Ok(false);
        }
        let start = (b - 1) * BLOCK;
        let n = BLOCK.min(self.len as usize - start);
        let mut buf = vec![0u8; n * 8];
        io("fingerprint file", f.read_exact_at(&mut buf, start as u64 * 8))?;
        let block: Vec<u64> = buf.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
        Ok(block.binary_search(&fp).is_ok())
    }

    /// Adopt a sorted file (a checkpoint's), building its index.
    fn open(path: PathBuf) -> R<Run> {
        let f = io("fingerprint file", File::open(&path))?;
        let len = io("fingerprint file", f.metadata())?.len() / 8;
        let mut r = BufReader::with_capacity(1 << 20, io("fingerprint file", File::open(&path))?);
        let mut index = Vec::with_capacity(len as usize / BLOCK + 1);
        let mut bloom = bloom_for(len);
        let mut b = [0u8; 8];
        for i in 0..len {
            io("fingerprint file", r.read_exact(&mut b))?;
            let fp = u64::from_le_bytes(b);
            if i as usize % BLOCK == 0 {
                index.push(fp);
            }
            bloom_add(&mut bloom, fp);
        }
        Ok(Run { file: Some((f, path)), len, index, bloom })
    }
}

struct Shard {
    mem: Table,
    run: Run,
}

pub struct FpSet {
    shards: Box<[Mutex<Shard>]>,
    /// the most slots one shard's table may hold before it spills
    max_len: usize,
    dir: PathBuf,
    /// spills so far (for progress lines)
    pub spills: AtomicU64,
}

impl Default for FpSet {
    fn default() -> FpSet {
        FpSet::new(u64::MAX, PathBuf::new())
    }
}

impl FpSet {
    /// `mem_bytes`: what the in-memory tables may use in all; past it,
    /// shards spill to sorted files in `dir`.
    pub fn new(mem_bytes: u64, dir: PathBuf) -> FpSet {
        let per = (mem_bytes / SHARDS as u64 / 8).max(1024);
        FpSet {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard { mem: Table { t: vec![0; 1024], n: 0 }, run: Run::default() })).collect(),
            max_len: per.min(usize::MAX as u64 / 2) as usize,
            dir,
            spills: AtomicU64::new(0),
        }
    }

    #[inline]
    fn shard(&self, fp: u64) -> &Mutex<Shard> {
        // high bits pick the shard; the table itself uses the low bits
        &self.shards[(fp >> 54) as usize % SHARDS]
    }

    /// true if fp was new
    pub fn insert(&self, fp: u64) -> R<bool> {
        let fp = fp.max(1);
        let i = (fp >> 54) as usize % SHARDS;
        let mut s = self.shards[i].lock().unwrap();
        if s.mem.contains(fp) || s.run.contains(fp)? {
            return Ok(false);
        }
        if s.mem.full(self.max_len) {
            self.spill(i, &mut s)?;
        }
        s.mem.insert_new(fp);
        Ok(true)
    }

    pub fn contains(&self, fp: u64) -> R<bool> {
        let fp = fp.max(1);
        let s = self.shard(fp).lock().unwrap();
        Ok(s.mem.contains(fp) || s.run.contains(fp)?)
    }

    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| {
                let s = s.lock().unwrap();
                s.mem.n + s.run.len as usize
            })
            .sum()
    }

    /// Merge shard i's table into its sorted file: a new file, renamed over
    /// the old, so a checkpoint's link to the old one stays intact.
    fn spill(&self, i: usize, s: &mut Shard) -> R<()> {
        let mut mem: Vec<u64> = s.mem.t.iter().copied().filter(|&x| x != 0).collect();
        mem.sort_unstable();
        io("creating the metadir", fs::create_dir_all(&self.dir))?;
        let path = self.dir.join(format!("fpset-{i}.bin"));
        let tmp = self.dir.join(format!("fpset-{i}.tmp"));
        let mut w = BufWriter::with_capacity(1 << 20, io("fingerprint file", File::create(&tmp))?);
        let mut index = Vec::new();
        let mut bloom = bloom_for(s.run.len + mem.len() as u64);
        let mut n = 0u64;
        let mut put = |fp: u64, w: &mut BufWriter<File>| -> R<()> {
            if n as usize % BLOCK == 0 {
                index.push(fp);
            }
            bloom_add(&mut bloom, fp);
            n += 1;
            io("fingerprint file", w.write_all(&fp.to_le_bytes()))
        };
        let mut old = match &s.run.file {
            Some((_, p)) => Some(BufReader::with_capacity(1 << 20, io("fingerprint file", File::open(p))?)),
            None => None,
        };
        let next_old = |r: &mut Option<BufReader<File>>| -> R<Option<u64>> {
            let Some(r) = r else { return Ok(None) };
            let mut b = [0u8; 8];
            match r.read_exact(&mut b) {
                Ok(()) => Ok(Some(u64::from_le_bytes(b))),
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
                Err(e) => Err(format!("fingerprint file: {e}")),
            }
        };
        let mut a = next_old(&mut old)?;
        let mut mi = 0;
        loop {
            match (a, mem.get(mi)) {
                (Some(x), Some(&y)) if x < y => {
                    put(x, &mut w)?;
                    a = next_old(&mut old)?;
                }
                (Some(x), None) => {
                    put(x, &mut w)?;
                    a = next_old(&mut old)?;
                }
                (_, Some(&y)) => {
                    put(y, &mut w)?;
                    mi += 1;
                }
                (None, None) => break,
            }
        }
        io("fingerprint file", w.flush())?;
        drop(w);
        io("fingerprint file", fs::rename(&tmp, &path))?;
        let f = io("fingerprint file", File::open(&path))?;
        s.run = Run { file: Some((f, path)), len: n, index, bloom };
        s.mem.t.iter_mut().for_each(|x| *x = 0);
        s.mem.n = 0;
        self.spills.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Into a checkpoint directory: each shard's disk run hard-linked,
    /// the in-memory fingerprints written out.
    fn save(&self, dir: &Path) -> R<()> {
        let mut w = BufWriter::with_capacity(1 << 20, io("create", File::create(dir.join("fps.bin")))?);
        for (i, s) in self.shards.iter().enumerate() {
            let s = s.lock().unwrap();
            for &fp in s.mem.t.iter().filter(|&&x| x != 0) {
                io("write", w.write_all(&fp.to_le_bytes()))?;
            }
            if let Some((f, p)) = &s.run.file {
                io("sync", f.sync_all())?;
                io("checkpoint", fs::hard_link(p, dir.join(format!("fpset-{i}.bin"))))?;
            }
        }
        io("write", w.flush())?;
        io("sync", w.get_ref().sync_all())
    }

    fn load(&self, dir: &Path) -> R<()> {
        for (i, s) in self.shards.iter().enumerate() {
            let src = dir.join(format!("fpset-{i}.bin"));
            if src.exists() {
                // adopt a link of the checkpoint's run; a later spill
                // replaces the link, never the checkpoint's file
                io("creating the metadir", fs::create_dir_all(&self.dir))?;
                let own = self.dir.join(format!("fpset-{i}.bin"));
                let _ = fs::remove_file(&own);
                io("checkpoint", fs::hard_link(&src, &own))?;
                s.lock().unwrap().run = Run::open(own)?;
            }
        }
        let mut r = BufReader::with_capacity(1 << 20, io("open", File::open(dir.join("fps.bin")))?);
        let mut b = [0u8; 8];
        loop {
            match r.read_exact(&mut b) {
                Ok(()) => {
                    self.insert(u64::from_le_bytes(b))?;
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
    pub(crate) fn path(&self, name: &str) -> R<PathBuf> {
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

pub(crate) fn put_var(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

pub(crate) fn get_var(b: &[u8], pos: &mut usize) -> R<u64> {
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

/// Serialized states held in memory.
pub struct MemBlock {
    pub bytes: Vec<u8>,
    pub count: u64,
}

/// One BFS level: blocks of serialized states in memory, then blocks in a
/// file. States are kept serialized even in memory: a worker decodes its
/// own private copy, where a shared tree would have every worker bumping
/// the same reference counts across cores (LeanScopedSyncHolds at 4
/// workers: 6.15 s holding trees, 5.34 s decoding), and the budget counts
/// real bytes.
#[derive(Default)]
pub struct Level {
    pub mem: Vec<MemBlock>,
    pub blocks: Vec<Block>,
    pub file: Option<(File, PathBuf)>,
}

impl Level {
    pub fn len(&self) -> u64 {
        self.mem.iter().map(|b| b.count).sum::<u64>() + self.blocks.iter().map(|b| b.count).sum::<u64>()
    }
    /// Append states as one block in memory.
    pub fn push_mem(&mut self, states: &[(u64, State)]) {
        if states.is_empty() {
            return;
        }
        let mut bytes = Vec::new();
        encode_states(states, &mut bytes);
        self.mem.push(MemBlock { bytes, count: states.len() as u64 });
    }
    pub fn read_mem(&self, i: usize, nvars: usize, out: &mut Vec<(u64, State)>) -> R<()> {
        let b = &self.mem[i];
        decode_states(&b.bytes, b.count as usize, nvars, out)
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
    /// a batch has gone to disk: every later one follows it, so the level
    /// is read back (memory, then blocks) in the order it was written —
    /// under SYMMETRY which state stands for an orbit depends on that
    /// order, and a small last batch slipping back under the budget
    /// changed TLC-exact counts (found by flint-27 on ForgeSyncRewind)
    spilled: std::sync::atomic::AtomicBool,
    mem: Mutex<Vec<MemBlock>>,
    disk: Mutex<(Option<(File, PathBuf)>, u64, Vec<Block>)>,
}

impl<'a> LevelWriter<'a> {
    pub fn new(meta: &'a Meta, depth: usize, budget: u64) -> LevelWriter<'a> {
        LevelWriter {
            meta,
            name: format!("queue-{depth}.bin"),
            budget,
            mem_bytes: AtomicU64::new(0),
            spilled: std::sync::atomic::AtomicBool::new(false),
            mem: Mutex::new(Vec::new()),
            disk: Mutex::new((None, 0, Vec::new())),
        }
    }
    /// Hand over a worker's batch, serialized: kept in memory while the
    /// level's bytes are under the budget, else written out as a block.
    pub fn push(&self, batch: &mut Vec<(u64, State)>, enc: &mut Vec<u8>) -> R<()> {
        if batch.is_empty() {
            return Ok(());
        }
        enc.clear();
        encode_states(batch, enc);
        let n = enc.len() as u64;
        if !self.spilled.load(Ordering::Relaxed) {
            if self.mem_bytes.fetch_add(n, Ordering::Relaxed) + n <= self.budget {
                self.mem.lock().unwrap().push(MemBlock { bytes: enc.clone(), count: batch.len() as u64 });
                batch.clear();
                return Ok(());
            }
            self.mem_bytes.fetch_sub(n, Ordering::Relaxed);
            self.spilled.store(true, Ordering::Relaxed);
        }
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
    /// with liveness properties: each graph log's (bytes, records) at the
    /// checkpoint, and the initial states' keys
    pub graph_logs: Vec<(u64, u64)>,
    pub graph_inits: Vec<u64>,
}

/// Write a checkpoint of the search as it stands before expanding
/// `level`: into `ckpt.tmp`, then swapped in for `ckpt`.
pub fn checkpoint(
    meta: &Meta,
    source_hash: u64,
    fps: &FpSet,
    trace: &Trace,
    depth: usize,
    generated: u64,
    level: &Level,
    graph: Option<(Vec<(u64, u64)>, &[u64])>,
) -> R<()> {
    let tmp = meta.path("ckpt.tmp")?;
    let _ = fs::remove_dir_all(&tmp);
    io("checkpoint", fs::create_dir_all(&tmp))?;
    fps.save(&tmp)?;
    let mut lens = Vec::new();
    for l in &trace.logs {
        let mut l = l.lock().unwrap();
        l.flush(meta)?;
        if let Some(f) = &l.file {
            io("sync", f.sync_all())?;
        }
        lens.push(l.flushed);
    }
    // the level: its memory blocks written out; blocks already on disk linked
    let mut f = BufWriter::new(io("checkpoint", File::create(tmp.join("queue-mem.bin")))?);
    for b in &level.mem {
        io("checkpoint", f.write_all(&b.bytes))?;
    }
    io("checkpoint", f.flush())?;
    io("sync", f.get_ref().sync_all())?;
    if let Some((qf, p)) = &level.file {
        io("sync", qf.sync_all())?;
        // an empty path: the level was recovered from the current checkpoint
        let src = if p.as_os_str().is_empty() { meta.dir.join("ckpt").join("queue-disk.bin") } else { p.clone() };
        io("checkpoint", fs::hard_link(src, tmp.join("queue-disk.bin")))?;
    }
    let mut m = String::new();
    m += &format!("source_hash {source_hash}\ndepth {depth}\ngenerated {generated}\n");
    for b in &level.mem {
        m += &format!("memblock {} {}\n", b.bytes.len(), b.count);
    }
    if let Some((lens, inits)) = &graph {
        // the graph logs are cut here on recovery; they must be on disk
        for slot in 0..lens.len() {
            if let Ok(f) = File::open(meta.dir.join(format!("graph-{slot}.bin"))) {
                io("sync", f.sync_all())?;
            }
        }
        for (len, records) in lens {
            m += &format!("graphlog {len} {records}\n");
        }
        for k in inits.iter() {
            m += &format!("graphinit {k}\n");
        }
    }
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
    let mut memblocks: Vec<(u64, u64)> = Vec::new();
    let (mut graph_logs, mut graph_inits) = (Vec::new(), Vec::new());
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
            // an older checkpoint: the memory part as one run of states
            "mem" => nmem = nums[0] as usize,
            "memblock" => memblocks.push((nums[0], nums[1])),
            "graphlog" => graph_logs.push((nums[0], nums[1])),
            "graphinit" => graph_inits.push(nums[0]),
            "trace" => lens = nums,
            "block" => blocks.push(Block { off: nums[0], len: nums[1], count: nums[2] }),
            _ => return Err(format!("bad checkpoint line: {line}")),
        }
    }
    *meta.made.lock().unwrap() = true;
    fps.load(&ck)?;
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
    let mut mem = Vec::new();
    if nmem > 0 {
        // check it decodes, then keep it as one block
        decode_states(&buf, nmem, nvars, &mut Vec::new())?;
        mem.push(MemBlock { bytes: buf, count: nmem as u64 });
    } else {
        let mut at = 0usize;
        for (len, count) in memblocks {
            let end = at + len as usize;
            let bytes = buf.get(at..end).ok_or("checkpoint: queue-mem.bin is shorter than its blocks")?.to_vec();
            mem.push(MemBlock { bytes, count });
            at = end;
        }
    }
    let file = if blocks.is_empty() {
        None
    } else {
        // read through the checkpoint's own link; never deleted by the run
        let p = ck.join("queue-disk.bin");
        Some((io("checkpoint", File::open(&p))?, PathBuf::new()))
    };
    Ok(Snapshot { depth, generated, level: Level { mem, blocks, file }, graph_logs, graph_inits })
}
