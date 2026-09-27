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
use crate::value::{Value, R};
use std::collections::HashMap;

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

// ---- the graph ----------------------------------------------------------------

pub struct Graph {
    pub keys: Vec<u64>,
    pub states: Vec<State>,
    pub index: HashMap<u64, u32>,
    /// successors, each with the fairness actions the edge is a step of
    pub adj: Vec<Vec<(u32, u128)>>,
    /// per node: the fairness actions enabled there
    pub enabled: Vec<u128>,
}

impl Graph {
    pub fn build(nodes: Vec<(u64, State)>, edges: Vec<(u64, u64, u128)>, enabled: Vec<(u64, u128)>) -> Graph {
        let mut index = HashMap::with_capacity(nodes.len());
        let (mut keys, mut states) = (Vec::with_capacity(nodes.len()), Vec::with_capacity(nodes.len()));
        for (k, s) in nodes {
            if let std::collections::hash_map::Entry::Vacant(e) = index.entry(k) {
                e.insert(keys.len() as u32);
                keys.push(k);
                states.push(s);
            }
        }
        let mut adj: Vec<Vec<(u32, u128)>> = vec![Vec::new(); keys.len()];
        for (a, b, m) in edges {
            if let (Some(&x), Some(&y)) = (index.get(&a), index.get(&b)) {
                adj[x as usize].push((y, m));
            }
        }
        for v in adj.iter_mut() {
            v.sort_unstable_by_key(|e| e.0);
            // one edge per target, carrying the union of its steps' labels
            v.dedup_by(|b, a| {
                if a.0 == b.0 {
                    a.1 |= b.1;
                    true
                } else {
                    false
                }
            });
        }
        let mut en = vec![0u128; keys.len()];
        for (k, m) in enabled {
            if let Some(&x) = index.get(&k) {
                en[x as usize] |= m;
            }
        }
        Graph { keys, states, index, adj, enabled: en }
    }

    /// Strongly connected components of the subgraph induced by `nodes`
    /// (iterative Tarjan).
    fn sccs(&self, nodes: &[u32], member: &mut Vec<u32>, stamp: u32) -> Vec<Vec<u32>> {
        for &u in nodes {
            member[u as usize] = stamp;
        }
        let n = self.keys.len();
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
                let edges = &self.adj[u as usize];
                if *ei < edges.len() {
                    let v = edges[*ei].0;
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
        let nodes: Vec<u32> = (0..self.keys.len() as u32).filter(|&u| allowed[u as usize]).collect();
        let mut member = vec![0u32; self.keys.len()];
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
                for &(v, mask) in &self.adj[u as usize] {
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
                let any = c.iter().any(|&u| self.enabled[u as usize] & bit != 0);
                let all = c.iter().all(|&u| self.enabled[u as usize] & bit != 0);
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
                let rest: Vec<u32> = c.iter().copied().filter(|&u| self.enabled[u as usize] & drop == 0).collect();
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
        let mut prev = vec![u32::MAX; self.keys.len()];
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
            for &(v, _) in &self.adj[u as usize] {
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

pub fn check_leaf(g: &Graph, p: &Program, i: &Inst, fair: &[FairInst], inits: &[u32], bufs: &mut Bufs) -> R<Option<Lasso>> {
    let n = g.keys.len();
    let holds = |e: &Expr, bufs: &mut Bufs| -> R<Vec<bool>> {
        (0..n).map(|u| eval_env(p, e, i.frame, &i.env, &g.states[u], None, bufs)?.as_bool()).collect()
    };
    let all = vec![true; n];
    let (allowed, accept, starts): (Vec<bool>, Option<Vec<bool>>, Option<Vec<bool>>) = match i.leaf {
        TProp::LeadsTo(pe, qe) => {
            let q = holds(qe, bufs)?;
            let pv = holds(pe, bufs)?;
            let not_q: Vec<bool> = q.iter().map(|x| !x).collect();
            let starts: Vec<bool> = (0..n).map(|u| pv[u] && !q[u]).collect();
            if !starts.iter().any(|&b| b) {
                return Ok(None);
            }
            (not_q, None, Some(starts))
        }
        TProp::AlwaysEventually(pe) => (holds(pe, bufs)?.iter().map(|x| !x).collect(), None, None),
        TProp::EventuallyAlways(pe) => (all.clone(), Some(holds(pe, bufs)?.iter().map(|x| !x).collect()), None),
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
        let succs: Vec<u32> = g.adj[entry as usize].iter().map(|e| e.0).filter(|&v| in_comp[v as usize]).collect();
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
