//! Breadth-first search over the reachable states, the way TLC does it:
//! only a 64-bit fingerprint of each seen state is kept, parents go to
//! trace logs so a counterexample can be replayed, a level that outgrows
//! its memory budget spills to disk, and each BFS level is expanded by all
//! workers in parallel. See `store`.

use crate::eval::{Bufs, Engine, Program};
use crate::liveness::{self, FairInst, Graph, Inst};
use crate::store::{self, FpSet, Level, LevelWriter, Meta, Trace, NO_PARENT};
use crate::symkey::{Scratch, SymKey};
use crate::value::{combine, fingerprint, same, var_hash, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub use crate::store::State;

/// States a worker hands to the next level at once.
const BATCH: usize = 4096;

/// Where a run keeps what does not fit in memory, and when it checkpoints.
pub struct Disk {
    /// created only if needed; the files the run made are removed when it
    /// finishes
    pub metadir: PathBuf,
    /// 0: never
    pub checkpoint_secs: u64,
    /// estimated bytes of one BFS level kept in memory before spilling
    pub queue_mem: u64,
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
    pub disk: Disk,
}

impl<'p> Checker<'p> {
    pub fn new(p: &'p Program, e: &'p dyn Engine, workers: usize) -> Result<Checker<'p>, String> {
        let (props, fair) = liveness::instances(p)?;
        let disk = Disk {
            metadir: std::env::temp_dir().join(format!("tlc-rs-{}", std::process::id())),
            checkpoint_secs: 0,
            queue_mem: 1 << 30,
            recover: false,
            source_hash: 0,
        };
        Ok(Checker { reference: None, p, e, sk: SymKey::new(p), vbufs: Default::default(), workers, progress: true, props, fair, disk })
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
                    if keep_all || !seen.contains(fp) {
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
            if keep_all || !seen.contains(fp) {
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
        let seen = FpSet::default();
        let mut trace = Trace::new(self.workers);
        let (mut out, failure) = self.search(&meta, &seen, &mut trace, t0);
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
        if !meta.exists() {
            return;
        }
        if let Ok(rd) = std::fs::read_dir(&meta.dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("ckpt") {
                    let _ = std::fs::remove_dir_all(e.path());
                } else if (n.starts_with("trace-") || n.starts_with("queue-")) && n.ends_with(".bin") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::remove_dir(&meta.dir);
    }

    fn search(&self, meta: &Meta, seen: &FpSet, trace: &mut Trace, t0: Instant) -> (Outcome, Option<Failure>) {
        let mut bufs = Bufs::default();
        let graph = self.needs_graph();
        let keep_all = self.keep_all();
        let nvars = self.p.vars.len();
        let nodes: Mutex<Vec<(u64, State)>> = Mutex::new(Vec::new());
        let edges: Mutex<Vec<(u64, u64, u128)>> = Mutex::new(Vec::new());
        let enabled: Mutex<Vec<(u64, u128)>> = Mutex::new(Vec::new());
        let sv = liveness::sub_vars(self.p, &self.fair);
        let mut init_keys: Vec<u64> = Vec::new();
        let done = |generated, depth, failure| (Outcome { generated, distinct: seen.len(), depth, failure: None }, failure);

        let mut frontier = Level::default();
        let mut depth = 1;
        let mut generated = 0u64;
        if self.disk.recover {
            if graph {
                return done(0, 0, Some(Failure::Eval("-recover: liveness properties keep their graph in memory; a checkpoint cannot hold it".into(), None)));
            }
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
                if seen.insert(fp) {
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
                                nodes.lock().unwrap().push((fp, s.clone()));
                                init_keys.push(fp);
                            }
                            frontier.mem.push((idx, s))
                        }
                        Ok(Some(inv)) => return done(generated, 1, Some(Failure::Invariant(inv, idx))),
                        Err(e) => return done(generated, 1, Some(Failure::Eval(e, Some(idx)))),
                    }
                }
            }
        }
        let trace = &*trace;

        let failure: Mutex<Option<Failure>> = Mutex::new(None);
        let stop = AtomicBool::new(false);
        let gen_total = AtomicU64::new(generated);
        let mut last_report = Instant::now();
        let ckpt_every = Duration::from_secs(self.disk.checkpoint_secs);
        let mut last_ckpt = Instant::now();
        let mut next_live_check = 1000usize;
        if graph && self.disk.checkpoint_secs > 0 && self.progress {
            eprintln!("checkpoints: off (liveness properties keep their graph in memory)");
        }
        while !frontier.is_empty() {
            if !graph && self.disk.checkpoint_secs > 0 && last_ckpt.elapsed() >= ckpt_every {
                let t = Instant::now();
                let g = gen_total.load(Ordering::Relaxed);
                if let Err(e) = store::checkpoint(meta, self.disk.source_hash, seen, trace, depth, g, &frontier) {
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
            // nodes admitted before this level's expansion are fully
            // expanded once it ends (their successors all recorded)
            let expanded_before = if graph { nodes.lock().unwrap().len() } else { 0 };
            let writer = LevelWriter::new(meta, depth + 1, self.disk.queue_mem);
            let mem_cursor = AtomicUsize::new(0);
            let blk_cursor = AtomicUsize::new(0);
            let chunk = (frontier.mem.len() / (self.workers * 64)).clamp(1, 1024);
            let frontier_ref = &frontier;
            std::thread::scope(|s| {
                for w in 0..self.workers {
                    let (writer, failure, stop, gen_total, nodes, edges, enabled, sv) = (&writer, &failure, &stop, &gen_total, &nodes, &edges, &enabled, &sv);
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
                        let (mut local_nodes, mut local_edges): (Vec<(u64, State)>, Vec<(u64, u64, u128)>) = (Vec::new(), Vec::new());
                        let mut local_enabled: Vec<(u64, u128)> = Vec::new();
                        let mut local_gen = 0u64;
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
                            if en != 0 {
                                local_enabled.push((pfp, en));
                            }
                            for (fp, s) in succ.drain(..) {
                                if keep_all {
                                    for inst in self.props.iter().filter(|i| matches!(i.leaf, crate::eval::TProp::ActionBox(..))) {
                                        match liveness::step_ok(self.p, inst, st, &s, &mut bufs) {
                                            Ok(true) => {}
                                            Ok(false) => return Some(Failure::Step(inst.name.clone(), pidx, s.clone())),
                                            Err(e) => return Some(Failure::Eval(format!("evaluating {}: {e}", inst.name), Some(pidx))),
                                        }
                                    }
                                    if graph {
                                        let mask = if en == 0 { Ok(0) } else { liveness::edge_label(self.p, &self.fair, en, &subs, st, &s, &mut bufs) };
                                        match mask {
                                            Ok(m) => local_edges.push((pfp, fp, m)),
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
                                if !seen.insert(fp) {
                                    continue;
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
                                        if graph {
                                            local_nodes.push((fp, s.clone()));
                                        }
                                        local_next.push((idx, s));
                                        if local_next.len() >= BATCH {
                                            if let Err(e) = writer.push(&mut local_next, &mut enc) {
                                                return Some(Failure::Eval(e, None));
                                            }
                                        }
                                    }
                                    Ok(Some(inv)) => return Some(Failure::Invariant(inv, idx)),
                                    Err(e) => return Some(Failure::Eval(e, Some(idx))),
                                }
                            }
                            None
                        };
                        'outer: loop {
                            if stop.load(Ordering::Relaxed) {
                                break;
                            }
                            let i = mem_cursor.fetch_add(chunk, Ordering::Relaxed);
                            if i < frontier.mem.len() {
                                for (pidx, st) in &frontier.mem[i..(i + chunk).min(frontier.mem.len())] {
                                    if let Some(f) = handle(*pidx, st) {
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
                        gen_total.fetch_add(local_gen, Ordering::Relaxed);
                        if let Err(e) = writer.push(&mut local_next, &mut enc) {
                            fail(Failure::Eval(e, None));
                        }
                        if graph {
                            nodes.lock().unwrap().append(&mut local_nodes);
                            edges.lock().unwrap().append(&mut local_edges);
                            enabled.lock().unwrap().append(&mut local_enabled);
                        }
                    });
                }
            });
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
            if graph && !frontier.is_empty() && expanded_before >= next_live_check {
                next_live_check = expanded_before * 2;
                let partial = {
                    let n = nodes.lock().unwrap();
                    let (e, en) = (edges.lock().unwrap(), enabled.lock().unwrap());
                    (n[..expanded_before].to_vec(), e.clone(), en.clone())
                };
                if let Some(f) = self.check_liveness(partial.0, partial.1, partial.2, &init_keys, true) {
                    *failure.lock().unwrap() = Some(f);
                    break;
                }
            }
            if self.progress && last_report.elapsed().as_secs() >= 10 {
                last_report = Instant::now();
                let on_disk: u64 = frontier.blocks.iter().map(|b| b.count).sum();
                eprintln!(
                    "progress: depth {depth}, {} generated, {} distinct, {} on queue ({on_disk} on disk), {:.1}s",
                    gen_total.load(Ordering::Relaxed),
                    seen.len(),
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
            failure = self.check_liveness(nodes.into_inner().unwrap(), edges.into_inner().unwrap(), enabled.into_inner().unwrap(), &init_keys, false);
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

    /// `partial`: the graph holds only the states expanded so far.
    fn check_liveness(&self, nodes: Vec<(u64, State)>, edges: Vec<(u64, u64, u128)>, enabled: Vec<(u64, u128)>, init_keys: &[u64], partial: bool) -> Option<Failure> {
        let t0 = Instant::now();
        let mut bufs = Bufs::default();
        let g = Graph::build(nodes, edges, enabled);
        if self.progress && !partial {
            eprintln!("liveness: graph of {} nodes built in {:.1}s", g.keys.len(), t0.elapsed().as_secs_f64());
        }
        let inits: Vec<u32> = init_keys.iter().filter_map(|k| g.index.get(k).copied()).collect();
        for inst in self.props.iter().filter(|i| liveness::is_graph_leaf(i.leaf)) {
            match liveness::check_leaf(&g, self.p, inst, &self.fair, &inits, &mut bufs) {
                Ok(None) => {}
                Ok(Some(l)) => {
                    let env: Vec<String> = inst.env.iter().map(|(_, v)| v.to_string()).collect();
                    println!(
                        "Error: Temporal property {}{} is violated.",
                        inst.name,
                        if env.is_empty() { String::new() } else { format!(" (for {})", env.join(", ")) }
                    );
                    for (i, &u) in l.prefix.iter().enumerate() {
                        self.show(i, &g.states[u as usize]);
                    }
                    let base = l.prefix.len();
                    for (i, &u) in l.cycle.iter().enumerate().skip(1) {
                        self.show(base + i - 1, &g.states[u as usize]);
                    }
                    if l.cycle.len() == 1 {
                        println!("State {}: Stuttering", base + 1);
                    } else {
                        let back = base - 1 + 1;
                        println!("Back to state {back}.");
                    }
                    return Some(Failure::Liveness(inst.name.clone()));
                }
                Err(e) => return Some(Failure::Eval(format!("evaluating {}: {e}", inst.name), None)),
            }
        }
        if self.progress {
            if partial {
                eprintln!("liveness: {} expanded states checked, no violation yet ({:.1}s)", g.keys.len(), t0.elapsed().as_secs_f64());
            } else {
                eprintln!("liveness: all properties checked in {:.1}s", t0.elapsed().as_secs_f64());
            }
        }
        None
    }

    fn print_trace(&self, trace: &Trace, idx: u64) -> usize {
        let chain = match trace.chain(idx) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("(could not read the trace: {e})");
                return 0;
            }
        };
        let mut bufs = Bufs::default();
        let Ok(inits) = self.init_states(&mut bufs) else { return 0 };
        let Some(mut st) = inits.into_iter().find(|s| self.fp(s) == chain[0]) else {
            eprintln!("(could not replay the trace)");
            return 0;
        };
        let show = |i: usize, st: &[Value]| {
            println!("State {}:", i + 1);
            for (n, v) in self.p.vars.iter().zip(st.iter()) {
                println!("/\\ {n} = {v}");
            }
            println!();
        };
        show(0, &st);
        for (i, want) in chain.iter().enumerate().skip(1) {
            let mut succ = Vec::new();
            if self.successors(&st, &mut bufs, &mut succ).is_err() {
                return 0;
            }
            match succ.into_iter().find(|s| self.fp(s) == *want) {
                Some(s) => st = s,
                None => {
                    eprintln!("(could not replay the trace)");
                    return 0;
                }
            }
            show(i, &st);
        }
        chain.len()
    }

}
