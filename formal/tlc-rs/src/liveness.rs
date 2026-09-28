//! Temporal properties over the reachable state graph.
//!
//! Step properties (`[][A]_v`) are checked on every transition during the
//! search. Liveness (`P ~> Q`, `[](P => <>Q)`, `<>[]P`, `[]<>P`) is checked
//! afterwards on the graph, under the spec's fairness, by finding fair
//! strongly connected components:
//!  - every state has a stuttering self-loop, as in TLA+;
//!  - a component satisfies WF_v(A) if A is disabled at some node of it or
//!    some edge inside it is an A-step (a path can cover every node and
//!    edge of a component infinitely often, so these are exact);
//!  - SF_v(A): if some node enables A and no edge is an A-step, drop the
//!    enabling nodes and recompute components (Emerson-Lei);
//!  - `P ~> Q` fails iff a reachable P /\ ~Q node reaches, through ~Q
//!    nodes, a fair component of ~Q nodes;
//!  - `<>[]P` fails iff some fair component contains a ~P node;
//!  - `[]<>P` fails iff some fair component lies within ~P.

use crate::eval::{Act, Bufs, Expr, FairTree, Program, TProp};
use crate::store::{io, put_var, Meta};
use crate::value::{Value, R};
use std::collections::HashMap;
use std::io::Write;

pub type State = Box<[Value]>;

/// One instance of a property leaf: the `\A` binders and operator
/// parameters above it, fixed to values.
pub struct Inst<'p> {
    pub name: String,
    pub leaf: &'p TProp,
    pub env: Vec<(u32, Value)>,
    pub frame: u32,
}

pub struct FairInst<'p> {
    pub strong: bool,
    pub sub: &'p Expr,
    pub act: &'p Act,
    pub env: Vec<(u32, Value)>,
    pub frame: u32,
    pub label: String,
}

/// Evaluates `e` in `state` (and, for a step, `next`) with `env` bound.
pub fn eval_env(p: &Program, e: &Expr, frame: u32, env: &[(u32, Value)], state: &[Value], next: Option<&[Value]>, bufs: &mut Bufs) -> R<Value> {
    let mut cx = bufs.cx(state, p.vars.len(), frame);
    for (s, v) in env {
        cx.stack[*s as usize] = v.clone();
    }
    if let Some(n) = next {
        for (i, v) in n.iter().enumerate() {
            cx.next[i] = Some(v.clone());
        }
        cx.next_fixed = true;
    }
    p.eval(e, &mut cx)
}

fn bind_all(p: &Program, b: &crate::eval::Bound, frame: u32, env: &[(u32, Value)], bufs: &mut Bufs) -> R<Vec<Vec<(u32, Value)>>> {
    let set = eval_env(p, &b.set, frame, env, &[], None, bufs)?.elems()?;
    let mut out = Vec::new();
    for x in set.iter() {
        if b.tuple {
            let Value::Seq(t) = x else { return Err(format!("cannot destructure {x}")) };
            out.push(b.slots.iter().zip(t.iter()).map(|(s, v)| (*s, v.clone())).collect());
        } else {
            out.push(vec![(b.slots[0], x.clone())]);
        }
    }
    Ok(out)
}

pub fn instances(p: &Program) -> R<(Vec<Inst<'_>>, Vec<FairInst<'_>>)> {
    let mut bufs = Bufs::default();
    let mut props = Vec::new();
    fn walk<'p>(p: &'p Program, t: &'p TProp, name: &str, frame: u32, env: &mut Vec<(u32, Value)>, out: &mut Vec<Inst<'p>>, bufs: &mut Bufs) -> R<()> {
        match t {
            TProp::ForAll(b, body) => {
                for binding in bind_all(p, b, frame, env, bufs)? {
                    let n = env.len();
                    env.extend(binding);
                    walk(p, body, name, frame, env, out, bufs)?;
                    env.truncate(n);
                }
            }
            TProp::Let(binds, body) => {
                let n = env.len();
                for (s, e) in binds.iter() {
                    let v = eval_env(p, e, frame, env, &[], None, bufs)?;
                    env.push((*s, v));
                }
                walk(p, body, name, frame, env, out, bufs)?;
                env.truncate(n);
            }
            TProp::And(v) => {
                for x in v {
                    walk(p, x, name, frame, env, out, bufs)?;
                }
            }
            leaf => out.push(Inst { name: name.to_string(), leaf, env: env.clone(), frame }),
        }
        Ok(())
    }
    for prop in &p.properties {
        walk(p, &prop.body, &prop.name, prop.frame, &mut Vec::new(), &mut props, &mut bufs)?;
    }
    let mut fair = Vec::new();
    fn fwalk<'p>(p: &'p Program, t: &'p FairTree, frame: u32, env: &mut Vec<(u32, Value)>, out: &mut Vec<FairInst<'p>>, bufs: &mut Bufs) -> R<()> {
        match t {
            FairTree::ForAll(b, body) => {
                for binding in bind_all(p, b, frame, env, bufs)? {
                    let n = env.len();
                    env.extend(binding);
                    fwalk(p, body, frame, env, out, bufs)?;
                    env.truncate(n);
                }
            }
            FairTree::Let(binds, body) => {
                let n = env.len();
                for (s, e) in binds.iter() {
                    let v = eval_env(p, e, frame, env, &[], None, bufs)?;
                    env.push((*s, v));
                }
                fwalk(p, body, frame, env, out, bufs)?;
                env.truncate(n);
            }
            FairTree::And(v) => {
                for x in v {
                    fwalk(p, x, frame, env, out, bufs)?;
                }
            }
            FairTree::Fair { strong, sub, act } => {
                let label = format!(
                    "{}F({})",
                    if *strong { "S" } else { "W" },
                    env.iter().map(|(_, v)| v.to_string()).collect::<Vec<_>>().join(", ")
                );
                out.push(FairInst { strong: *strong, sub, act, env: env.clone(), frame, label });
            }
        }
        Ok(())
    }
    if let Some(f) = &p.fairness {
        fwalk(p, &f.body, f.frame, &mut Vec::new(), &mut fair, &mut bufs)?;
    }
    if fair.len() > 128 {
        return Err(format!("{} fairness instances; at most 128 are supported", fair.len()));
    }
    Ok((props, fair))
}

/// Checks a step against a `[][A]_v` instance: true if A holds or v is
/// unchanged.
pub fn step_ok(p: &Program, i: &Inst, s: &[Value], t: &[Value], bufs: &mut Bufs) -> R<bool> {
    let TProp::ActionBox(a, sub) = i.leaf else { return Ok(true) };
    let v0 = eval_env(p, sub, i.frame, &i.env, s, None, bufs)?;
    let v1 = eval_env(p, sub, i.frame, &i.env, t, None, bufs)?;
    if v0 == v1 {
        return Ok(true);
    }
    eval_env(p, a, i.frame, &i.env, s, Some(t), bufs)?.as_bool()
}

pub fn is_graph_leaf(t: &TProp) -> bool {
    matches!(t, TProp::LeadsTo(..) | TProp::EventuallyAlways(_) | TProp::AlwaysEventually(_))
}

// ---- which state predicates the graph carries ------------------------------

/// Where each graph-leaf instance's predicates sit in a node's bit words:
/// `P ~> Q` has two bits (P, Q), `[]<>P` and `<>[]P` one.
pub struct Layout {
    /// per instance (indexes into the checker's `props`): its first bit
    pub base: Vec<Option<usize>>,
    pub words: usize,
}

pub fn layout(props: &[Inst]) -> Layout {
    let mut next = 0;
    let base = props
        .iter()
        .map(|i| match i.leaf {
            TProp::LeadsTo(..) => {
                next += 2;
                Some(next - 2)
            }
            TProp::AlwaysEventually(_) | TProp::EventuallyAlways(_) => {
                next += 1;
                Some(next - 1)
            }
            _ => None,
        })
        .collect();
    Layout { base, words: next.div_ceil(64) }
}

/// A node's predicate bits, evaluated once, when the state is expanded:
/// the graph never needs the state again.
pub fn node_bits(p: &Program, props: &[Inst], lay: &Layout, st: &[Value], bufs: &mut Bufs, out: &mut Vec<u64>) -> R<()> {
    out.clear();
    out.resize(lay.words, 0);
    for (i, inst) in props.iter().enumerate() {
        let Some(b) = lay.base[i] else { continue };
        let mut set = |k: usize, e: &Expr, bufs: &mut Bufs| -> R<()> {
            if eval_env(p, e, inst.frame, &inst.env, st, None, bufs)?.as_bool()? {
                out[k / 64] |= 1 << (k % 64);
            }
            Ok(())
        };
        match inst.leaf {
            TProp::LeadsTo(pe, qe) => {
                set(b, pe, bufs)?;
                set(b + 1, qe, bufs)?;
            }
            TProp::AlwaysEventually(pe) | TProp::EventuallyAlways(pe) => set(b, pe, bufs)?,
            _ => {}
        }
    }
    Ok(())
}

// ---- the graph on disk --------------------------------------------------------

/// Per worker, an append-only log of expanded nodes, one record each:
/// trace index, key, enabled fairness actions, predicate bits, and the
/// edges out (target key, fairness actions the step is a step of).
/// Nothing of the graph stays in memory during the search.
pub struct GraphLogs {
    pub logs: Vec<std::sync::Mutex<GLog>>,
}

pub struct GLog {
    slot: usize,
    file: Option<std::fs::File>,
    buf: Vec<u8>,
    /// bytes in the file
    pub flushed: u64,
    /// records written (flushed or not)
    pub records: u64,
}

impl GraphLogs {
    pub fn new(workers: usize) -> GraphLogs {
        GraphLogs {
            logs: (0..workers.max(1))
                .map(|slot| std::sync::Mutex::new(GLog { slot, file: None, buf: Vec::new(), flushed: 0, records: 0 }))
                .collect(),
        }
    }
    pub fn records(&self) -> u64 {
        self.logs.iter().map(|l| l.lock().unwrap().records).sum()
    }
    pub fn flush_all(&self, meta: &Meta) -> R<()> {
        for l in &self.logs {
            l.lock().unwrap().flush(meta)?;
        }
        Ok(())
    }
    pub fn lengths(&self) -> Vec<(u64, u64)> {
        self.logs.iter().map(|l| { let l = l.lock().unwrap(); (l.flushed, l.records) }).collect()
    }
    /// After a checkpoint's recovery: each log cut back to the checkpoint's
    /// length. Records written after it (by expansions in progress when
    /// the run stopped) are not part of it; TLC's own disk graph is not
    /// cut, and its recovery then fails reading it (seen by flint-27).
    pub fn truncate(&mut self, meta: &Meta, lens: &[(u64, u64)]) -> R<()> {
        while self.logs.len() < lens.len() {
            let slot = self.logs.len();
            self.logs.push(std::sync::Mutex::new(GLog { slot, file: None, buf: Vec::new(), flushed: 0, records: 0 }));
        }
        for (slot, &(len, records)) in lens.iter().enumerate() {
            let p = meta.dir.join(format!("graph-{slot}.bin"));
            let mut l = self.logs[slot].lock().unwrap();
            if len == 0 {
                let _ = std::fs::remove_file(&p);
                continue;
            }
            let f = io("graph log", std::fs::OpenOptions::new().read(true).append(true).open(&p))?;
            io("graph log", f.set_len(len))?;
            l.file = Some(f);
            l.flushed = len;
            l.records = records;
        }
        Ok(())
    }
    fn paths(&self, meta: &Meta) -> Vec<std::path::PathBuf> {
        self.logs.iter().filter(|l| l.lock().unwrap().flushed > 0).map(|l| meta.dir.join(format!("graph-{}.bin", l.lock().unwrap().slot))).collect()
    }
}

impl GLog {
    /// One node's record.
    pub fn append(&mut self, meta: &Meta, tidx: u64, key: u64, en: u128, bits: &[u64], edges: &[(u64, u128)]) -> R<()> {
        let b = &mut self.buf;
        put_var(b, tidx);
        b.extend_from_slice(&key.to_le_bytes());
        put_var(b, en as u64);
        put_var(b, (en >> 64) as u64);
        for &w in bits {
            put_var(b, w);
        }
        put_var(b, edges.len() as u64);
        for &(t, m) in edges {
            b.extend_from_slice(&t.to_le_bytes());
            put_var(b, m as u64);
            put_var(b, (m >> 64) as u64);
        }
        self.records += 1;
        if self.buf.len() >= 4 << 20 {
            self.flush(meta)?;
        }
        Ok(())
    }
    fn flush(&mut self, meta: &Meta) -> R<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            let p = meta.path(&format!("graph-{}.bin", self.slot))?;
            self.file = Some(io("graph log", std::fs::OpenOptions::new().create(true).read(true).append(true).open(p))?);
        }
        io("graph log", self.file.as_mut().unwrap().write_all(&self.buf))?;
        self.flushed += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }
}

/// Reads the records of one graph log, in order, streaming.
fn each_record(path: &std::path::Path, words: usize, mut f: impl FnMut(u64, u64, u128, &[u64], &[(u64, u128)]) -> R<()>) -> R<()> {
    use std::io::Read;
    let mut r = std::io::BufReader::with_capacity(1 << 20, io("graph log", std::fs::File::open(path))?);
    let mut byte = [0u8; 1];
    // Ok(None) at a clean end of file
    let mut var = |r: &mut std::io::BufReader<std::fs::File>, first: bool| -> R<Option<u64>> {
        let (mut n, mut shift) = (0u64, 0);
        loop {
            match r.read_exact(&mut byte) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && first && shift == 0 => return Ok(None),
                Err(e) => return Err(format!("graph log: {e}")),
            }
            n |= ((byte[0] & 0x7f) as u64) << shift;
            if byte[0] < 0x80 {
                return Ok(Some(n));
            }
            shift += 7;
        }
    };
    let u64le = |r: &mut std::io::BufReader<std::fs::File>| -> R<u64> {
        let mut b = [0u8; 8];
        io("graph log", r.read_exact(&mut b))?;
        Ok(u64::from_le_bytes(b))
    };
    let (mut bits, mut edges) = (Vec::with_capacity(words), Vec::new());
    loop {
        let Some(tidx) = var(&mut r, true)? else { return Ok(()) };
        let key = u64le(&mut r)?;
        let en = var(&mut r, false)?.unwrap() as u128 | (var(&mut r, false)?.unwrap() as u128) << 64;
        bits.clear();
        for _ in 0..words {
            bits.push(var(&mut r, false)?.unwrap());
        }
        let ne = var(&mut r, false)?.unwrap();
        edges.clear();
        for _ in 0..ne {
            let t = u64le(&mut r)?;
            let m = var(&mut r, false)?.unwrap() as u128 | (var(&mut r, false)?.unwrap() as u128) << 64;
            edges.push((t, m));
        }
        f(tidx, key, en, &bits, &edges)?;
    }
}

/// Distinct fairness masks (there are few), so an edge carries 4 bytes.
struct Palette {
    v: Vec<u128>,
    map: HashMap<u128, u32>,
}

impl Palette {
    fn idx(&mut self, m: u128) -> u32 {
        if let Some(&i) = self.map.get(&m) {
            return i;
        }
        self.v.push(m);
        self.map.insert(m, self.v.len() as u32 - 1);
        self.v.len() as u32 - 1
    }
}

/// The graph, compact: nodes by sorted key (the node id is the position),
/// edges in one array (CSR), fairness masks through a small palette.
pub struct Graph {
    pub keys: Vec<u64>,
    /// per node: its trace index (to rebuild its state for a counterexample)
    pub tidx: Vec<u64>,
    /// per node: `words` words of predicate bits
    pub bits: Vec<u64>,
    pub words: usize,
    /// per node: palette index of the fairness actions enabled there
    en: Vec<u32>,
    off: Vec<u64>,
    tgt: Vec<u32>,
    /// per edge: palette index of the fairness actions it is a step of
    emask: Vec<u32>,
    palette: Vec<u128>,
}

impl Graph {
    /// Two passes over the logs: the keys (sorted, they give the ids),
    /// then the edges, resolved to ids; an edge to a state that was never
    /// expanded (not admitted, or not reached yet) is dropped.
    pub fn load(logs: &GraphLogs, meta: &Meta, words: usize) -> R<Graph> {
        let paths = logs.paths(meta);
        let mut rows: Vec<(u64, u64, u128, u64)> = Vec::new(); // key, tidx, en, degree
        let mut bits_by_row: Vec<u64> = Vec::new();
        for p in &paths {
            each_record(p, words, |tidx, key, en, bits, edges| {
                rows.push((key, tidx, en, edges.len() as u64));
                bits_by_row.extend_from_slice(bits);
                Ok(())
            })?;
        }
        let mut order: Vec<u32> = (0..rows.len() as u32).collect();
        order.sort_unstable_by_key(|&r| rows[r as usize].0);
        // each state is expanded once; a key recorded twice means the logs
        // hold records a checkpoint's recovery should have cut, and the
        // graph would silently split that node's edges
        if let Some(w) = order.windows(2).find(|w| rows[w[0] as usize].0 == rows[w[1] as usize].0) {
            return Err(format!("the graph logs record the state with key {:016x} twice (logs not cut back to the checkpoint?)", rows[w[0] as usize].0));
        }
        let n = order.len();
        let mut pal = Palette { v: vec![0], map: HashMap::from([(0, 0)]) };
        let (mut keys, mut tidx, mut en, mut bits) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n * words));
        let mut off = Vec::with_capacity(n + 1);
        off.push(0u64);
        for &r in &order {
            let (k, t, e, d) = rows[r as usize];
            keys.push(k);
            tidx.push(t);
            en.push(pal.idx(e));
            bits.extend_from_slice(&bits_by_row[r as usize * words..(r as usize + 1) * words]);
            off.push(off.last().unwrap() + d);
        }
        drop(rows);
        drop(bits_by_row);
        let id = |k: u64| keys.binary_search(&k).ok().map(|i| i as u32);
        let total = *off.last().unwrap() as usize;
        let (mut tgt, mut emask) = (vec![u32::MAX; total], vec![0u32; total]);
        for p in &paths {
            each_record(p, words, |_, key, _, _, edges| {
                let u = id(key).ok_or("graph log: a node without its key")? as usize;
                let at = off[u] as usize;
                for (j, &(t, m)) in edges.iter().enumerate() {
                    if let Some(v) = id(t) {
                        tgt[at + j] = v;
                        emask[at + j] = pal.idx(m);
                    }
                }
                Ok(())
            })?;
        }
        // per node: edges to one target merged (their masks OR-ed), the
        // dropped ones (u32::MAX) removed
        let mut out_off = Vec::with_capacity(n + 1);
        out_off.push(0u64);
        let (mut t2, mut m2) = (Vec::with_capacity(total), Vec::with_capacity(total));
        let mut row: Vec<(u32, u128)> = Vec::new();
        for u in 0..n {
            row.clear();
            for e in off[u] as usize..off[u + 1] as usize {
                if tgt[e] != u32::MAX {
                    row.push((tgt[e], pal.v[emask[e] as usize]));
                }
            }
            row.sort_unstable_by_key(|e| e.0);
            let mut i = 0;
            while i < row.len() {
                let (v, mut m) = row[i];
                i += 1;
                while i < row.len() && row[i].0 == v {
                    m |= row[i].1;
                    i += 1;
                }
                t2.push(v);
                m2.push(pal.idx(m));
            }
            out_off.push(t2.len() as u64);
        }
        Ok(Graph { keys, tidx, bits, words, en, off: out_off, tgt: t2, emask: m2, palette: pal.v })
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }
    pub fn index(&self, key: u64) -> Option<u32> {
        self.keys.binary_search(&key).ok().map(|i| i as u32)
    }
    fn enabled(&self, u: u32) -> u128 {
        self.palette[self.en[u as usize] as usize]
    }
    fn succ(&self, u: u32) -> impl Iterator<Item = (u32, u128)> + '_ {
        let r = self.off[u as usize] as usize..self.off[u as usize + 1] as usize;
        self.tgt[r.clone()].iter().zip(self.emask[r].iter()).map(|(&v, &m)| (v, self.palette[m as usize]))
    }
    fn bit(&self, u: u32, k: usize) -> bool {
        self.bits[u as usize * self.words + k / 64] & (1 << (k % 64)) != 0
    }

    /// Strongly connected components of the subgraph induced by `nodes`
    /// (iterative Tarjan).
    fn sccs(&self, nodes: &[u32], member: &mut Vec<u32>, stamp: u32) -> Vec<Vec<u32>> {
        for &u in nodes {
            member[u as usize] = stamp;
        }
        let n = self.len();
        let mut idx = vec![u32::MAX; 0];
        idx.resize(n, u32::MAX);
        let mut low = vec![0u32; n];
        let mut on = vec![false; n];
        let mut stack: Vec<u32> = Vec::new();
        let mut out = Vec::new();
        let mut counter = 0u32;
        for &root in nodes {
            if idx[root as usize] != u32::MAX {
                continue;
            }
            let mut call: Vec<(u32, usize)> = vec![(root, 0)];
            idx[root as usize] = counter;
            low[root as usize] = counter;
            counter += 1;
            stack.push(root);
            on[root as usize] = true;
            while let Some(&mut (u, ref mut ei)) = call.last_mut() {
                let start = self.off[u as usize] as usize;
                let deg = self.off[u as usize + 1] as usize - start;
                if *ei < deg {
                    let v = self.tgt[start + *ei];
                    *ei += 1;
                    if member[v as usize] != stamp {
                        continue;
                    }
                    if idx[v as usize] == u32::MAX {
                        idx[v as usize] = counter;
                        low[v as usize] = counter;
                        counter += 1;
                        stack.push(v);
                        on[v as usize] = true;
                        call.push((v, 0));
                    } else if on[v as usize] {
                        low[u as usize] = low[u as usize].min(idx[v as usize]);
                    }
                } else {
                    call.pop();
                    if let Some(&(parent, _)) = call.last() {
                        low[parent as usize] = low[parent as usize].min(low[u as usize]);
                    }
                    if low[u as usize] == idx[u as usize] {
                        let mut comp = Vec::new();
                        loop {
                            let w = stack.pop().unwrap();
                            on[w as usize] = false;
                            comp.push(w);
                            if w == u {
                                break;
                            }
                        }
                        out.push(comp);
                    }
                }
            }
        }
        out
    }

    /// The fair components within `allowed`, each containing an
    /// `accept` node when that is given.
    pub fn fair_sccs(&self, allowed: &[bool], accept: Option<&[bool]>, fair: &[FairInst]) -> Vec<Vec<u32>> {
        let nodes: Vec<u32> = (0..self.len() as u32).filter(|&u| allowed[u as usize]).collect();
        let mut member = vec![0u32; self.len()];
        let mut stamp = 1;
        let mut work = self.sccs(&nodes, &mut member, stamp);
        let mut result = Vec::new();
        while let Some(c) = work.pop() {
            if let Some(acc) = accept {
                if !c.iter().any(|&u| acc[u as usize]) {
                    continue;
                }
            }
            stamp += 1;
            for &u in &c {
                member[u as usize] = stamp;
            }
            // which fairness actions have a step inside c
            let mut stepped: u128 = 0;
            for &u in &c {
                for (v, mask) in self.succ(u) {
                    if member[v as usize] == stamp {
                        stepped |= mask;
                    }
                }
            }
            let mut ok = true;
            let mut drop: u128 = 0;
            for (j, f) in fair.iter().enumerate() {
                let bit = 1u128 << j;
                if stepped & bit != 0 {
                    continue;
                }
                let any = c.iter().any(|&u| self.enabled(u) & bit != 0);
                let all = c.iter().all(|&u| self.enabled(u) & bit != 0);
                if !f.strong && all {
                    ok = false; // continuously enabled, never taken: no sub-cycle helps
                    break;
                }
                if f.strong && any {
                    drop |= bit;
                }
            }
            if !ok {
                continue;
            }
            if drop != 0 {
                let rest: Vec<u32> = c.iter().copied().filter(|&u| self.enabled(u) & drop == 0).collect();
                if !rest.is_empty() {
                    stamp += 1;
                    work.extend(self.sccs(&rest, &mut member, stamp));
                }
                continue;
            }
            result.push(c);
        }
        result
    }

    /// Shortest path from any `from` node to any `to` node, within `allowed`.
    pub fn path(&self, from: &[u32], to: &[bool], allowed: &[bool]) -> Option<Vec<u32>> {
        let mut prev = vec![u32::MAX; self.len()];
        let mut q = std::collections::VecDeque::new();
        for &f in from {
            if allowed[f as usize] {
                prev[f as usize] = f;
                q.push_back(f);
            }
        }
        while let Some(u) = q.pop_front() {
            if to[u as usize] {
                let mut p = vec![u];
                let mut x = u;
                while prev[x as usize] != x {
                    x = prev[x as usize];
                    p.push(x);
                }
                p.reverse();
                return Some(p);
            }
            for (v, _) in self.succ(u) {
                if allowed[v as usize] && prev[v as usize] == u32::MAX {
                    prev[v as usize] = u;
                    q.push_back(v);
                }
            }
        }
        None
    }
}

/// A violated liveness instance: the prefix to the loop, then the loop
/// (a single node means stuttering forever there).
pub struct Lasso {
    pub prefix: Vec<u32>,
    pub cycle: Vec<u32>,
}

/// `base`: the instance's first predicate bit (see `layout`).
pub fn check_leaf(g: &Graph, i: &Inst, base: usize, fair: &[FairInst], inits: &[u32]) -> R<Option<Lasso>> {
    let n = g.len();
    let holds = |k: usize| -> Vec<bool> { (0..n as u32).map(|u| g.bit(u, k)).collect() };
    let all = vec![true; n];
    let (allowed, accept, starts): (Vec<bool>, Option<Vec<bool>>, Option<Vec<bool>>) = match i.leaf {
        TProp::LeadsTo(..) => {
            let pv = holds(base);
            let q = holds(base + 1);
            let not_q: Vec<bool> = q.iter().map(|x| !x).collect();
            let starts: Vec<bool> = (0..n).map(|u| pv[u] && !q[u]).collect();
            if !starts.iter().any(|&b| b) {
                return Ok(None);
            }
            (not_q, None, Some(starts))
        }
        TProp::AlwaysEventually(_) => (holds(base).iter().map(|x| !x).collect(), None, None),
        TProp::EventuallyAlways(_) => (all.clone(), Some(holds(base).iter().map(|x| !x).collect()), None),
        _ => return Ok(None),
    };
    let comps = g.fair_sccs(&allowed, accept.as_deref(), fair);
    if comps.is_empty() {
        return Ok(None);
    }
    let mut in_fair = vec![false; n];
    for c in &comps {
        for &u in c {
            in_fair[u as usize] = true;
        }
    }
    // the stem: from an initial state to a start node (LeadsTo), then,
    // within `allowed`, to a fair component
    let (prefix, entry) = match &starts {
        Some(st) => {
            // a start node that can reach a fair component within ~Q
            let starts_list: Vec<u32> = (0..n as u32).filter(|&u| st[u as usize]).collect();
            let Some(stem) = g.path(&starts_list, &in_fair, &allowed) else { return Ok(None) };
            let first = stem[0];
            let mut to_first = vec![false; n];
            to_first[first as usize] = true;
            let mut pre = g.path(inits, &to_first, &all).unwrap_or_else(|| vec![first]);
            pre.pop();
            pre.extend(stem);
            let last = *pre.last().unwrap();
            (pre, last)
        }
        None => {
            let pre = g.path(inits, &in_fair, &all).ok_or("unreachable fair component")?;
            let last = *pre.last().unwrap();
            (pre, last)
        }
    };
    // the loop: the component containing the entry, walked back to the entry
    let comp = comps.iter().find(|c| c.contains(&entry)).cloned().unwrap_or_else(|| vec![entry]);
    let mut in_comp = vec![false; n];
    for &u in &comp {
        in_comp[u as usize] = true;
    }
    let cycle = if comp.len() == 1 {
        vec![entry]
    } else {
        let succs: Vec<u32> = g.succ(entry).map(|e| e.0).filter(|&v| in_comp[v as usize]).collect();
        let mut target = vec![false; n];
        target[entry as usize] = true;
        let mut c = g.path(&succs, &target, &in_comp).unwrap_or_default();
        c.insert(0, entry);
        c.pop();
        c
    };
    Ok(Some(Lasso { prefix, cycle }))
}

const STOP: &str = "\u{0}stop";

/// ENABLED <<A>>_sub at `st`: some A-step changes sub. A variable A leaves
/// unassigned is free (TLA+), so it can always be changed.
pub fn enabled(p: &Program, f: &FairInst, st: &[Value], sub_vars: &[bool], bufs: &mut Bufs) -> R<bool> {
    let nv = p.vars.len();
    let sub0 = eval_env(p, f.sub, f.frame, &f.env, st, None, bufs)?;
    let mut found = false;
    let mut tb = Bufs::default();
    let r = {
        let mut cx = bufs.cx(st, nv, f.frame);
        for (s, v) in &f.env {
            cx.stack[*s as usize] = v.clone();
        }
        p.run(f.act, &mut cx, &mut |cx| {
            if (0..nv).any(|i| sub_vars[i] && cx.next[i].is_none()) {
                found = true;
                return Err(STOP.into());
            }
            let t: Vec<Value> = (0..nv).map(|i| cx.next[i].clone().unwrap_or_else(|| st[i].clone())).collect();
            if eval_env(p, f.sub, f.frame, &f.env, &t, None, &mut tb)? != sub0 {
                found = true;
                return Err(STOP.into());
            }
            Ok(())
        })
    };
    match r {
        Err(e) if e != STOP => Err(e),
        _ => Ok(found),
    }
}

/// Is st -> t an A-step: A holds with every primed variable preset to t.
pub fn a_step(p: &Program, f: &FairInst, st: &[Value], t: &[Value], bufs: &mut Bufs) -> R<bool> {
    let mut cx = bufs.cx(st, p.vars.len(), f.frame);
    for (s, v) in &f.env {
        cx.stack[*s as usize] = v.clone();
    }
    for (i, v) in t.iter().enumerate() {
        cx.next[i] = Some(v.clone());
    }
    let mut found = false;
    match p.run(f.act, &mut cx, &mut |_| {
        found = true;
        Err(STOP.into())
    }) {
        Err(e) if e != STOP => Err(e),
        _ => Ok(found),
    }
}

/// Per fairness instance: the variables its subscript reads.
pub fn sub_vars(p: &Program, fair: &[FairInst]) -> Vec<Vec<bool>> {
    fair.iter()
        .map(|f| {
            let mut acc = vec![false; p.vars.len()];
            crate::symkey::deps(p, f.sub, &mut acc, &mut vec![false; p.ops.len()]);
            acc
        })
        .collect()
}

/// The fairness actions enabled at `st`, and their subscripts' values
/// there (for judging the steps out of it).
pub fn node_label(p: &Program, fair: &[FairInst], sv: &[Vec<bool>], st: &[Value], bufs: &mut Bufs) -> R<(u128, Vec<Value>)> {
    let mut en = 0u128;
    let mut subs = Vec::with_capacity(fair.len());
    for (j, f) in fair.iter().enumerate() {
        subs.push(eval_env(p, f.sub, f.frame, &f.env, st, None, bufs)?);
        if enabled(p, f, st, &sv[j], bufs)? {
            en |= 1 << j;
        }
    }
    Ok((en, subs))
}

/// The fairness actions the step st -> t is a step of.
pub fn edge_label(p: &Program, fair: &[FairInst], en: u128, subs: &[Value], st: &[Value], t: &[Value], bufs: &mut Bufs) -> R<u128> {
    let mut mask = 0u128;
    for (j, f) in fair.iter().enumerate() {
        if en & (1 << j) == 0 {
            continue;
        }
        if eval_env(p, f.sub, f.frame, &f.env, t, None, bufs)? != subs[j] && a_step(p, f, st, t, bufs)? {
            mask |= 1 << j;
        }
    }
    Ok(mask)
}
