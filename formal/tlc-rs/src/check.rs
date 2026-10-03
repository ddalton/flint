//! Breadth-first search over the reachable states, the way TLC does it:
//! only a 64-bit fingerprint of each seen state is kept, parents go to
//! trace logs so a counterexample can be replayed, a level that outgrows
//! its memory budget spills to disk, and each BFS level is expanded by all
//! workers in parallel. See `store`.

use crate::eval::{Bufs, Engine, Program};
use crate::liveness::{self, FairInst, Graph, GraphLogs, Inst, Layout};
use crate::store::{self, FpSet, Level, LevelWriter, Meta, Trace, NO_PARENT};
use crate::symkey::{Scratch, SymKey};
use crate::value::{combine, fingerprint, same, var_hash, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub use crate::store::State;

/// States a worker hands to the next level at once: one block, the unit
/// of work (4096 left a spilled level's last blocks to a single worker).
const BATCH: usize = 256;

/// Where a run keeps what does not fit in memory, and when it checkpoints.
pub struct Disk {
    /// created only if needed; the files the run made are removed when it
    /// finishes
    pub metadir: PathBuf,
    /// 0: never
    pub checkpoint_secs: u64,
    /// estimated bytes of one BFS level kept in memory before spilling
    pub queue_mem: u64,
    /// bytes the in-memory fingerprint tables may use before shards
    /// spill to sorted files in the metadir
    pub fp_mem: u64,
    /// resume from the checkpoint in `metadir`
    pub recover: bool,
    /// written into a checkpoint; recovery refuses a mismatch
    pub source_hash: u64,
}

/// The u64s are trace indexes (see `store::Trace`).
pub enum Failure {
    Invariant(String, u64),
    /// a `[][A]_v` property, broken by the step from the state with this
    /// trace index to the given state
    Step(String, u64, State),
    /// a liveness property, with its counterexample already printed
    Liveness(String),
    Deadlock(u64),
    Eval(String, Option<u64>),
    /// a property's initial-state predicate, false in this initial state
    InitProperty(String, u64),
}

pub struct Outcome {
    pub generated: u64,
    pub distinct: usize,
    pub depth: usize,
    pub failure: Option<Failure>,
}

pub struct Checker<'p> {
    /// Diagnostic: stop at the first admitted state whose key is not in
    /// this set (keys of a reference run's states).
    pub reference: Option<std::collections::HashSet<u64>>,
    pub p: &'p Program,
    pub e: &'p dyn Engine,
    /// the key under SYMMETRY/VIEW; None for the plain fingerprint
    pub sk: Option<SymKey<'p>>,
    /// scratch buffers for evaluating VIEW beside an action in progress
    pub vbufs: Mutex<Vec<Scratch>>,
    pub workers: usize,
    pub progress: bool,
    /// property leaves, instantiated; fairness instances
    pub props: Vec<Inst<'p>>,
    pub fair: Vec<FairInst<'p>>,
    /// where each liveness instance's predicate bits sit in a graph node
    pub lay: Layout,
    pub disk: Disk,
}

impl<'p> Checker<'p> {
    pub fn new(p: &'p Program, e: &'p dyn Engine, workers: usize) -> Result<Checker<'p>, String> {
        let (props, fair) = liveness::instances(p)?;
        let disk = Disk {
            metadir: std::env::temp_dir().join(format!("tlc-rs-{}", std::process::id())),
            checkpoint_secs: 0,
            queue_mem: 1 << 30,
            fp_mem: u64::MAX,
            recover: false,
            source_hash: 0,
        };
        let lay = liveness::layout(&props);
        Ok(Checker { reference: None, p, e, sk: SymKey::new(p), vbufs: Default::default(), workers, progress: true, props, fair, lay, disk })
    }

    fn needs_graph(&self) -> bool {
        self.props.iter().any(|i| liveness::is_graph_leaf(i.leaf))
    }

    /// Every transition must be seen, not only those into new states.
    fn keep_all(&self) -> bool {
        self.needs_graph() || self.props.iter().any(|i| matches!(i.leaf, crate::eval::TProp::ActionBox(..)))
    }

    /// A property's state-predicate leaves (`Init` of `Init /\ [][Next]_v`)
    /// hold in each initial state.
    fn init_ok(&self, st: &[Value], bufs: &mut Bufs) -> Result<Option<String>, String> {
        for i in &self.props {
            if let crate::eval::TProp::Init(e) = i.leaf {
                if !liveness::eval_env(self.p, e, i.frame, &i.env, st, None, bufs)?.as_bool()? {
                    return Ok(Some(i.name.clone()));
                }
            }
        }
        Ok(None)
    }

    /// `[]P` properties are invariants.
    fn always_ok(&self, st: &[Value], bufs: &mut Bufs) -> Result<Option<String>, String> {
        for i in &self.props {
            if let crate::eval::TProp::Always(e) = i.leaf {
                if !liveness::eval_env(self.p, e, i.frame, &i.env, st, None, bufs)?.as_bool()? {
                    return Ok(Some(i.name.clone()));
                }
            }
        }
        Ok(None)
    }

    fn successors(&self, st: &[Value], bufs: &mut Bufs, out: &mut Vec<State>) -> Result<(), String> {
        let p = self.p;
        let nvars = p.vars.len();
        let mut cx = bufs.cx(st, nvars, p.next.frame);
        self.e.next(&mut cx, &mut |cx| {
            let mut s = Vec::with_capacity(nvars);
            for (i, v) in cx.next.iter().enumerate() {
                match v {
                    Some(v) => s.push(v.clone()),
                    None => return Err(format!("a step of Next leaves {}' unassigned", p.vars[i])),
                }
            }
            out.push(s.into_boxed_slice());
            Ok(())
        })
    }

    /// Expand one state for the BFS. Successors already seen are dropped
    /// before they are ever allocated; the rest come back with their
    /// fingerprints. Returns (parent fingerprint, successors generated).
    fn expand(
        &self,
        st: &[Value],
        seen: &FpSet,
        bufs: &mut Bufs,
        hv: &mut Vec<u64>,
        hs: &mut Vec<u64>,
        sc: &mut Scratch,
        out: &mut Vec<(u64, State)>,
    ) -> Result<(u64, u64), String> {
        let p = self.p;
        let nvars = p.vars.len();
        let keep_all = self.keep_all();
        if let Some(sk) = &self.sk {
            let Scratch { parent, succ, changed, bufs: vbufs } = sc;
            sk.reset(parent);
            changed.clear();
            changed.resize(nvars, false);
            let r = (|| {
                let pfp = sk.key(st, changed, parent, succ, vbufs)?;
                let mut n = 0u64;
                let mut cx = bufs.cx(st, nvars, p.next.frame);
                self.e.next(&mut cx, &mut |cx| {
                    n += 1;
                    let mut s = Vec::with_capacity(nvars);
                    for (i, v) in cx.next.iter().enumerate() {
                        match v {
                            Some(v) => {
                                changed[i] = !same(v, &st[i]);
                                s.push(v.clone());
                            }
                            None => return Err(format!("a step of Next leaves {}' unassigned", p.vars[i])),
                        }
                    }
                    sk.reset(succ);
                    let fp = sk.key(&s, changed, parent, succ, vbufs)?;
                    if keep_all || !seen.contains(fp)? {
                        out.push((fp, s.into_boxed_slice()));
                    }
                    Ok(())
                })?;
                Ok((pfp, n))
            })();
            return r;
        }
        // hv[k * nvars + i]: the hash of variable i under permutation k
        // (k = 0 is the identity). A successor's untouched variables reuse
        // the parent's hash under every permutation, never re-permuted.
        let np = 1;
        hv.clear();
        hv.extend(st.iter().map(var_hash));
        let pfp = combine(hv);
        let mut n = 0u64;
        let mut cx = bufs.cx(st, nvars, p.next.frame);
        self.e.next(&mut cx, &mut |cx| {
            n += 1;
            hs.clear();
            hs.resize(np * nvars, 0);
            for (i, v) in cx.next.iter().enumerate() {
                let Some(v) = v else {
                    return Err(format!("a step of Next leaves {}' unassigned", p.vars[i]));
                };
                if same(v, &st[i]) {
                    for k in 0..np {
                        hs[k * nvars + i] = hv[k * nvars + i];
                    }
                } else {
                    hs[i] = var_hash(v);
                    for (k, q) in p.symmetry.iter().enumerate() {
                        hs[(k + 1) * nvars + i] = var_hash(&v.permute(q));
                    }
                }
            }
            let fp = (0..np).map(|k| combine(&hs[k * nvars..(k + 1) * nvars])).min().unwrap();
            if keep_all || !seen.contains(fp)? {
                out.push((fp, cx.next.iter().map(|v| v.clone().unwrap()).collect()));
            }
            Ok(())
        })?;
        Ok((pfp, n))
    }

    /// A state's fingerprint: under SYMMETRY, the least fingerprint of its
    /// images under the permutations, so every state in an orbit gets one
    /// (TLC's rule). The queue keeps the actual state.
    pub fn fp(&self, st: &[Value]) -> u64 {
        self.key_of(st).unwrap_or(0)
    }

    /// A state's seen-set key, computed without a parent to reuse.
    pub fn key_of(&self, st: &[Value]) -> Result<u64, String> {
        match &self.sk {
            None => Ok(fingerprint(st)),
            Some(sk) => sk.key_alone(st, &mut Scratch::default()),
        }
    }

    /// The first of the invariants (or, with `constraints`, the state
    /// constraints) that `st` violates.
    fn holds(&self, constraints: bool, st: &[Value], bufs: &mut Bufs) -> Result<Option<String>, String> {
        let list = if constraints { &self.p.constraints } else { &self.p.invariants };
        for (i, r) in list.iter().enumerate() {
            let mut cx = bufs.cx(st, self.p.vars.len(), r.frame);
            let ok = if constraints { self.e.constraint(i, &mut cx) } else { self.e.invariant(i, &mut cx) };
            if !ok.map_err(|e| format!("evaluating {}: {e}", r.name))? {
                return Ok(Some(r.name.clone()));
            }
        }
        Ok(None)
    }

    pub fn init_states(&self, bufs: &mut Bufs) -> Result<Vec<State>, String> {
        let p = self.p;
        let nvars = p.vars.len();
        let empty: Vec<Value> = vec![];
        let mut out = Vec::new();
        let mut cx = bufs.cx(&empty, nvars, p.init.frame);
        self.e.init(&mut cx, &mut |cx| {
            let mut s = Vec::with_capacity(nvars);
            for (i, v) in cx.next.iter().enumerate() {
                match v {
                    Some(v) => s.push(v.clone()),
                    None => return Err(format!("Init leaves {} unassigned", p.vars[i])),
                }
            }
            out.push(s.into_boxed_slice());
            Ok(())
        })?;
        Ok(out)
    }

    pub fn run(&self) -> Outcome {
        let t0 = Instant::now();
        let meta = Meta::new(self.disk.metadir.clone());
        store::raise_fd_limit();
        let seen = FpSet::new(self.disk.fp_mem, meta.dir.clone());
        let mut trace = Trace::new(self.workers);
        let mut glogs = GraphLogs::new(self.workers);
        let (mut out, failure) = self.search(&meta, &seen, &mut trace, &mut glogs, t0);
        if let Some(f) = &failure {
            let idx = match f {
                Failure::Invariant(_, i) | Failure::Deadlock(i) | Failure::Step(_, i, _) | Failure::InitProperty(_, i) => Some(*i),
                Failure::Eval(_, i) => *i,
                Failure::Liveness(_) => None,
            };
            if let Some(idx) = idx {
                let n = self.print_trace(&trace, idx);
                if let Failure::Step(_, _, s) = f {
                    self.show(n, s);
                }
            }
        }
        drop(trace);
        self.clean(&meta);
        out.failure = failure;
        out
    }

    /// Remove what the run wrote (never anything else in the directory).
    fn clean(&self, meta: &Meta) {
        if !meta.dir.is_dir() {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(&meta.dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("ckpt") {
                    let _ = std::fs::remove_dir_all(e.path());
                } else if (n.starts_with("trace-") || n.starts_with("queue-") || n.starts_with("fpset-") || n.starts_with("graph-"))
                    && (n.ends_with(".bin") || n.ends_with(".tmp"))
                {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::remove_dir(&meta.dir);
    }

    fn search(&self, meta: &Meta, seen: &FpSet, trace: &mut Trace, glogs: &mut GraphLogs, t0: Instant) -> (Outcome, Option<Failure>) {
        let mut bufs = Bufs::default();
        let graph = self.needs_graph();
        let keep_all = self.keep_all();
        let nvars = self.p.vars.len();
        let sv = liveness::sub_vars(self.p, &self.fair);
        let mut init_keys: Vec<u64> = Vec::new();
        let done = |generated, depth, failure| (Outcome { generated, distinct: seen.len(), depth, failure: None }, failure);

        let mut frontier = Level::default();
        let mut depth = 1;
        let mut generated = 0u64;
        if self.disk.recover {
            match store::recover(meta, self.disk.source_hash, nvars, seen, trace) {
                Ok(s) => {
                    eprintln!(
                        "recovered from {}: depth {}, {} generated, {} distinct, {} on queue",
                        meta.dir.join("ckpt").display(),
                        s.depth,
                        s.generated,
                        seen.len(),
                        s.level.len()
                    );
                    if graph {
                        if let Err(e) = glogs.truncate(meta, &s.graph_logs) {
                            return done(0, 0, Some(Failure::Eval(format!("-recover: {e}"), None)));
                        }
                        init_keys = s.graph_inits.clone();
                    }
                    (depth, generated, frontier) = (s.depth, s.generated, s.level);
                }
                Err(e) => return done(0, 0, Some(Failure::Eval(format!("-recover: {e}"), None))),
            }
        } else {
            let inits = match self.init_states(&mut bufs) {
                Ok(v) => v,
                Err(e) => return done(generated, 0, Some(Failure::Eval(e, None))),
            };
            let mut log = trace.logs[0].lock().unwrap();
            let mut init_level: Vec<(u64, State)> = Vec::new();
            for s in inits {
                generated += 1;
                let fp = match self.key_of(&s) {
                    Ok(fp) => fp,
                    Err(e) => return done(generated, 0, Some(Failure::Eval(e, None))),
                };
                match self.holds(true, &s, &mut bufs) {
                    Ok(None) => {}
                    Ok(Some(_)) => continue,
                    Err(e) => return done(generated, 0, Some(Failure::Eval(e, None))),
                }
                let new = match seen.insert(fp) {
                    Ok(b) => b,
                    Err(e) => return done(generated, 0, Some(Failure::Eval(e, None))),
                };
                if new {
                    let idx = match log.append(meta, fp, NO_PARENT) {
                        Ok(i) => i,
                        Err(e) => return done(generated, 1, Some(Failure::Eval(e, None))),
                    };
                    match self.init_ok(&s, &mut bufs) {
                        Ok(None) => {}
                        Ok(Some(name)) => return done(generated, 1, Some(Failure::InitProperty(name, idx))),
                        Err(e) => return done(generated, 1, Some(Failure::Eval(e, Some(idx)))),
                    }
                    let held = self.holds(false, &s, &mut bufs).and_then(|r| match r {
                        None => self.always_ok(&s, &mut bufs),
                        some => Ok(some),
                    });
                    match held {
                        Ok(None) => {
                            if graph {
                                init_keys.push(fp);
                            }
                            init_level.push((idx, s))
                        }
                        Ok(Some(inv)) => return done(generated, 1, Some(Failure::Invariant(inv, idx))),
                        Err(e) => return done(generated, 1, Some(Failure::Eval(e, Some(idx)))),
                    }
                }
            }
            for part in init_level.chunks(BATCH) {
                frontier.push_mem(part);
            }
        }
        let trace = &*trace;
        let glogs = &*glogs;

        let failure: Mutex<Option<Failure>> = Mutex::new(None);
        let stop = AtomicBool::new(false);
        let gen_total = AtomicU64::new(generated);
        let mut last_report = Instant::now();
        let ckpt_every = Duration::from_secs(self.disk.checkpoint_secs);
        let mut last_ckpt = Instant::now();
        let mut next_live_check = 1000usize;
        while !frontier.is_empty() {
            if self.disk.checkpoint_secs > 0 && last_ckpt.elapsed() >= ckpt_every {
                let t = Instant::now();
                let g = gen_total.load(Ordering::Relaxed);
                let gl = if graph {
                    if let Err(e) = glogs.flush_all(meta) {
                        return done(g, depth, Some(Failure::Eval(format!("checkpoint: {e}"), None)));
                    }
                    Some((glogs.lengths(), init_keys.as_slice()))
                } else {
                    None
                };
                if let Err(e) = store::checkpoint(meta, self.disk.source_hash, seen, trace, depth, g, &frontier, gl) {
                    return done(g, depth, Some(Failure::Eval(format!("checkpoint: {e}"), None)));
                }
                if self.progress {
                    eprintln!(
                        "checkpoint: {} (depth {depth}, {} distinct, {} on queue) in {:.1}s",
                        meta.dir.join("ckpt").display(),
                        seen.len(),
                        frontier.len(),
                        t.elapsed().as_secs_f64()
                    );
                }
                last_ckpt = Instant::now();
            }
            let writer = LevelWriter::new(meta, depth + 1, self.disk.queue_mem);
            let mem_cursor = AtomicUsize::new(0);
            // blocks sized so the next level (about this one's size) has
            // ~16 per worker
            let batch = (frontier.len() as usize / (self.workers * 16)).clamp(1, BATCH);
            let blk_cursor = AtomicUsize::new(0);
            let frontier_ref = &frontier;
            let lvl_t0 = Instant::now();
            let busy_ns = AtomicU64::new(0);
            let busy = &busy_ns;
            std::thread::scope(|s| {
                for w in 0..self.workers {
                    let (writer, failure, stop, gen_total, sv) = (&writer, &failure, &stop, &gen_total, &sv);
                    let (mem_cursor, blk_cursor) = (&mem_cursor, &blk_cursor);
                    s.spawn(move || {
                        let frontier = frontier_ref;
                        let mut log = trace.logs[w].lock().unwrap();
                        let mut bufs = Bufs::default();
                        let mut succ: Vec<(u64, State)> = Vec::new();
                        let (mut hv, mut hs) = (Vec::new(), Vec::new());
                        let mut sc = Scratch::default();
                        let mut enc: Vec<u8> = Vec::new();
                        let mut decoded: Vec<(u64, State)> = Vec::new();
                        let mut local_next: Vec<(u64, State)> = Vec::new();
                        // this worker's graph log, and the node being expanded
                        let mut glog = glogs.logs[w].lock().unwrap();
                        let (mut node_edges, mut node_bits): (Vec<(u64, u128)>, Vec<u64>) = (Vec::new(), Vec::new());
                        let mut local_gen = 0u64;
                        let wt0 = Instant::now();
                        let fail = |f: Failure| {
                            let mut g = failure.lock().unwrap();
                            if g.is_none() {
                                *g = Some(f);
                            }
                            stop.store(true, Ordering::Relaxed);
                        };
                        // expand one state; Some(failure) stops the search
                        let mut handle = |pidx: u64, st: &[Value]| -> Option<Failure> {
                            succ.clear();
                            let (pfp, n) = match self.expand(st, seen, &mut bufs, &mut hv, &mut hs, &mut sc, &mut succ) {
                                Ok(r) => r,
                                Err(e) => return Some(Failure::Eval(e, Some(pidx))),
                            };
                            if n == 0 && self.p.check_deadlock {
                                return Some(Failure::Deadlock(pidx));
                            }
                            local_gen += n;
                            let (en, subs) = if graph && !self.fair.is_empty() {
                                match liveness::node_label(self.p, &self.fair, sv, st, &mut bufs) {
                                    Ok(l) => l,
                                    Err(e) => return Some(Failure::Eval(format!("evaluating fairness: {e}"), Some(pidx))),
                                }
                            } else {
                                (0, Vec::new())
                            };
                            node_edges.clear();
                            for (fp, s) in succ.drain(..) {
                                if keep_all {
                                    for (pi, inst) in self.props.iter().enumerate().filter(|(_, i)| matches!(i.leaf, crate::eval::TProp::ActionBox(..))) {
                                        let ok = match self.e.step_prop(pi, st, &s) {
                                            Some(r) => r,
                                            None => liveness::step_ok(self.p, inst, st, &s, &mut bufs),
                                        };
                                        match ok {
                                            Ok(true) => {}
                                            Ok(false) => return Some(Failure::Step(inst.name.clone(), pidx, s.clone())),
                                            Err(e) => return Some(Failure::Eval(format!("evaluating {}: {e}", inst.name), Some(pidx))),
                                        }
                                    }
                                    if graph {
                                        let mask = if en == 0 { Ok(0) } else { liveness::edge_label(self.p, &self.fair, en, &subs, st, &s, &mut bufs) };
                                        match mask {
                                            Ok(m) => node_edges.push((fp, m)),
                                            Err(e) => return Some(Failure::Eval(format!("evaluating fairness: {e}"), Some(pidx))),
                                        }
                                    }
                                }
                                if !self.p.constraints.is_empty() {
                                    match self.holds(true, &s, &mut bufs) {
                                        Ok(None) => {}
                                        Ok(Some(_)) => continue,
                                        Err(e) => return Some(Failure::Eval(e, Some(pidx))),
                                    }
                                }
                                match seen.insert(fp) {
                                    Ok(true) => {}
                                    Ok(false) => continue,
                                    Err(e) => return Some(Failure::Eval(e, None)),
                                }
                                let idx = match log.append(meta, fp, pidx) {
                                    Ok(i) => i,
                                    Err(e) => return Some(Failure::Eval(e, None)),
                                };
                                if let Some(r) = &self.reference {
                                    if !r.contains(&fp) {
                                        return Some(Failure::Eval("state not in the reference run".into(), Some(idx)));
                                    }
                                }
                                let held = self.holds(false, &s, &mut bufs).and_then(|r| match r {
                                    None => self.always_ok(&s, &mut bufs),
                                    some => Ok(some),
                                });
                                match held {
                                    Ok(None) => {
                                        local_next.push((idx, s));
                                        if local_next.len() >= batch {
                                            if let Err(e) = writer.push(&mut local_next, &mut enc) {
                                                return Some(Failure::Eval(e, None));
                                            }
                                        }
                                    }
                                    Ok(Some(inv)) => return Some(Failure::Invariant(inv, idx)),
                                    Err(e) => return Some(Failure::Eval(e, Some(idx))),
                                }
                            }
                            if graph {
                                // the node, expanded: its record on disk
                                if let Err(e) = liveness::node_bits(self.p, &self.props, &self.lay, st, &mut bufs, &mut node_bits) {
                                    return Some(Failure::Eval(format!("evaluating a liveness property: {e}"), Some(pidx)));
                                }
                                if let Err(e) = glog.append(meta, pidx, pfp, en, &node_bits, &node_edges) {
                                    return Some(Failure::Eval(e, None));
                                }
                            }
                            None
                        };
                        'outer: loop {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            // a block (up to BATCH states) at a time, from
                            // memory first, then from the file
                            let i = mem_cursor.fetch_add(1, Ordering::Relaxed);
                            if i < frontier.mem.len() {
                                decoded.clear();
                                if let Err(e) = frontier.read_mem(i, nvars, &mut decoded) {
                                    fail(Failure::Eval(e, None));
                                    break;
                                }
                                for (pidx, st) in decoded.drain(..) {
                                    if let Some(f) = handle(pidx, &st) {
                                        fail(f);
                                        break 'outer;
                                    }
                                }
                                continue;
                            }
                            let b = blk_cursor.fetch_add(1, Ordering::Relaxed);
                            if b >= frontier.blocks.len() {
                                break;
                            }
                            decoded.clear();
                            if let Err(e) = frontier.read(&frontier.blocks[b], nvars, &mut decoded) {
                                fail(Failure::Eval(e, None));
                                break;
                            }
                            for (pidx, st) in decoded.drain(..) {
                                if let Some(f) = handle(pidx, &st) {
                                    fail(f);
                                    break 'outer;
                                }
                            }
                        }
                        busy.fetch_add(wt0.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        gen_total.fetch_add(local_gen, Ordering::Relaxed);
                        if let Err(e) = writer.push(&mut local_next, &mut enc) {
                            fail(Failure::Eval(e, None));
                        }
                    });
                }
            });
            // TLCRS_LEVEL_PROFILE: per level, how busy the workers were
            if std::env::var_os("TLCRS_LEVEL_PROFILE").is_some() {
                let wall = lvl_t0.elapsed().as_secs_f64();
                eprintln!("level {depth}: {} states, wall {:.1} ms, workers busy {:.0}%", frontier.len(), wall * 1e3, busy_ns.load(Ordering::Relaxed) as f64 / 1e9 / (wall * self.workers as f64) * 100.0);
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::mem::replace(&mut frontier, writer.finish()).discard();
            if !frontier.is_empty() {
                depth += 1;
            }
            // Fail fast, as TLC does: check the properties on the graph of
            // the states expanded so far, each time it has doubled. Every
            // cycle there is a cycle of the final graph, and every node in
            // it has its final successors and enabledness, so a fair
            // counterexample found here is real; the final check still
            // runs for what this one cannot see yet.
            if graph && !frontier.is_empty() {
                if let Err(e) = glogs.flush_all(meta) {
                    *failure.lock().unwrap() = Some(Failure::Eval(e, None));
                    break;
                }
                let expanded = glogs.records() as usize;
                if expanded >= next_live_check {
                    next_live_check = expanded * 2;
                    if let Some(f) = self.check_liveness(glogs, meta, trace, &init_keys, true) {
                        *failure.lock().unwrap() = Some(f);
                        break;
                    }
                }
            }
            if self.progress && last_report.elapsed().as_secs() >= 10 {
                last_report = Instant::now();
                let on_disk: u64 = frontier.blocks.iter().map(|b| b.count).sum();
                eprintln!(
                    "progress: depth {depth}, {} generated, {} distinct ({} fingerprint spills), {} on queue ({on_disk} on disk), {:.1}s",
                    gen_total.load(Ordering::Relaxed),
                    seen.len(),
                    seen.spills.load(Ordering::Relaxed),
                    frontier.len(),
                    t0.elapsed().as_secs_f64()
                );
            }
        }
        let mut failure = failure.into_inner().unwrap();
        let generated = gen_total.load(Ordering::Relaxed);
        drop(frontier);
        if failure.is_none() && graph {
            if self.progress {
                eprintln!("liveness: checking {} property instances under {} fairness instances", self.props.iter().filter(|i| liveness::is_graph_leaf(i.leaf)).count(), self.fair.len());
            }
            failure = match glogs.flush_all(meta) {
                Ok(()) => self.check_liveness(glogs, meta, trace, &init_keys, false),
                Err(e) => Some(Failure::Eval(e, None)),
            };
        }
        done(generated, depth, failure)
    }

    /// Walk parent fingerprints back to an initial state, then replay
    /// forward, regenerating each state from its predecessor.
    fn show(&self, i: usize, st: &[Value]) {
        println!("State {}:", i + 1);
        for (n, v) in self.p.vars.iter().zip(st.iter()) {
            println!("/\\ {n} = {v}");
        }
        println!();
    }

    /// `partial`: the logs hold only the states expanded so far. The
    /// graph is loaded from them; a counterexample's states are rebuilt
    /// from the trace logs, since the graph keeps none.
    fn check_liveness(&self, glogs: &GraphLogs, meta: &Meta, trace: &Trace, init_keys: &[u64], partial: bool) -> Option<Failure> {
        let t0 = Instant::now();
        let g = match Graph::load(glogs, meta, self.lay.words) {
            Ok(g) => g,
            Err(e) => return Some(Failure::Eval(format!("loading the liveness graph: {e}"), None)),
        };
        if self.progress && !partial {
            eprintln!("liveness: graph of {} nodes loaded in {:.1}s", g.len(), t0.elapsed().as_secs_f64());
        }
        let inits: Vec<u32> = init_keys.iter().filter_map(|&k| g.index(k)).collect();
        for (i, inst) in self.props.iter().enumerate() {
            let Some(base) = self.lay.base[i] else { continue };
            match liveness::check_leaf(&g, inst, base, &self.fair, &inits) {
                Ok(None) => {}
                Ok(Some(l)) => {
                    let env: Vec<String> = inst.env.iter().map(|(_, v)| v.to_string()).collect();
                    // TLC's own line, which the gates match; then which one
                    println!("Error: Temporal properties were violated.");
                    println!(
                        "Error: Temporal property {}{} is violated.",
                        inst.name,
                        if env.is_empty() { String::new() } else { format!(" (for {})", env.join(", ")) }
                    );
                    let nodes: Vec<u32> = l.prefix.iter().chain(l.cycle.iter().skip(1)).copied().collect();
                    for (i, &u) in nodes.iter().enumerate() {
                        match self.state_at(trace, g.tidx[u as usize]) {
                            Some(st) => self.show(i, &st),
                            None => println!("State {}: (could not rebuild it from the trace)\n", i + 1),
                        }
                    }
                    let base = l.prefix.len();
                    if l.cycle.len() == 1 {
                        println!("State {}: Stuttering", base + 1);
                    } else {
                        println!("Back to state {base}.");
                    }
                    return Some(Failure::Liveness(inst.name.clone()));
                }
                Err(e) => return Some(Failure::Eval(format!("evaluating {}: {e}", inst.name), None)),
            }
        }
        if self.progress {
            if partial {
                eprintln!("liveness: {} expanded states checked, no violation yet ({:.1}s)", g.len(), t0.elapsed().as_secs_f64());
            } else {
                eprintln!("liveness: all properties checked in {:.1}s", t0.elapsed().as_secs_f64());
            }
        }
        None
    }

    /// The states from an initial state to the one at trace index `idx`,
    /// replayed from the fingerprints the trace logs keep.
    fn replay(&self, trace: &Trace, idx: u64) -> Result<Vec<State>, String> {
        let chain = trace.chain(idx)?;
        let mut bufs = Bufs::default();
        let inits = self.init_states(&mut bufs)?;
        let mut st = inits.into_iter().find(|s| self.fp(s) == chain[0]).ok_or("could not replay the trace")?;
        let mut out = vec![st.clone()];
        for want in chain.iter().skip(1) {
            let mut succ = Vec::new();
            self.successors(&st, &mut bufs, &mut succ)?;
            st = succ.into_iter().find(|s| self.fp(s) == *want).ok_or("could not replay the trace")?;
            out.push(st.clone());
        }
        Ok(out)
    }

    fn state_at(&self, trace: &Trace, idx: u64) -> Option<State> {
        self.replay(trace, idx).ok().and_then(|v| v.into_iter().last())
    }

    fn print_trace(&self, trace: &Trace, idx: u64) -> usize {
        match self.replay(trace, idx) {
            Ok(states) => {
                for (i, st) in states.iter().enumerate() {
                    self.show(i, st);
                }
                states.len()
            }
            Err(e) => {
                eprintln!("({e})");
                0
            }
        }
    }

}
