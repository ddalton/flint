//! The seen-set key under SYMMETRY and/or VIEW, computed incrementally.
//!
//! Semantics are TLC's (TLCStateMut.fingerPrint): take the least permuted
//! image of the state, comparing variables in TLC's order, and key it — or,
//! with a VIEW, key the view of that image.
//!
//! What makes it cheap is that an action changes few variables:
//!  - a permutation is applied to a variable only on demand, and a value
//!    containing no permuted model value comes back "unchanged" without
//!    being copied;
//!  - per parent state, the permuted image of each variable, how it
//!    compares with the original, and its hash are memoized; a successor
//!    recomputes them only for the variables its action changed;
//!  - a VIEW that is a tuple is keyed part by part: a part that is a plain
//!    variable reuses that variable's hash, and an expression part is
//!    re-evaluated only when a variable it reads changed.
//! The key is `combine` of the parts' hashes: a different function from
//! the plain fingerprint, but the same partition, and one fixed function
//! for the whole run.

use crate::compile::visit;
use crate::eval::{Bin, Bufs, Engine, Expr, Program};
use crate::value::{combine, var_hash, Value, R};
use std::cmp::Ordering;

enum Comp<'p> {
    Var(usize),
    /// `equivariant`: view(perm(s)) = perm(view(s)), established
    /// syntactically; then the part is evaluated on the state itself and
    /// its value's permuted hash taken, instead of evaluating it on a
    /// permuted copy of the state.
    Expr { e: &'p Expr, frame: u32, deps: Vec<usize>, equivariant: bool },
}

pub struct SymKey<'p> {
    p: &'p Program,
    /// asked first for a VIEW part (`Engine::view_part`): a generated
    /// checker answers with compiled code, the interpreter with None
    e: &'p dyn Engine,
    /// 1 + the number of non-identity permutations
    np: usize,
    /// inverse of each non-identity permutation
    inv: Vec<Vec<u32>>,
    comps: Vec<Comp<'p>>,
}

/// Memoized per-(permutation, variable) and per-(permutation, part) facts.
#[derive(Default)]
pub struct Memo {
    img: Vec<Option<Option<Value>>>,
    cmp: Vec<Option<Ordering>>,
    vh: Vec<Option<u64>>,
    ch: Vec<Option<u64>>,
    /// per part: its value on the (unpermuted) state
    cv: Vec<Option<Value>>,
}

/// A worker's scratch space: the parent's memo, the current successor's,
/// the changed-variable mask, and evaluation buffers.
#[derive(Default)]
pub struct Scratch {
    pub parent: Memo,
    pub succ: Memo,
    pub changed: Vec<bool>,
    pub bufs: Bufs,
}

/// Is `e` equivariant under every permutation: built only from structure
/// (records, functions, tuples, set algebra, EXCEPT, quantifiers,
/// comparisons of equality, arithmetic) and constants no permutation
/// moves? CHOOSE, and anything else that could depend on which model
/// value is which, makes it not.
fn equivariant(p: &Program, e: &Expr, seen_ops: &mut [u8]) -> bool {
    let ok = match e {
        Expr::Choose(..) | Expr::Primed(_) | Expr::Enabled(_) => false,
        Expr::Bin(Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge, ..) => true, // integers only
        Expr::Const(v) => p.symmetry.iter().all(|q| !v.moves(q)),
        Expr::Call(op, _) => {
            let op = *op as usize;
            match seen_ops[op] {
                1 => true, // in progress: assume, as for recursion
                2 => true,
                3 => false,
                _ => {
                    seen_ops[op] = 1;
                    let r = p.ops[op].cached.as_ref().map_or_else(
                        || equivariant(p, &p.ops[op].body, seen_ops),
                        |v| p.symmetry.iter().all(|q| !v.moves(q)),
                    );
                    seen_ops[op] = if r { 2 } else { 3 };
                    r
                }
            }
        }
        Expr::LetRef(_, id) => equivariant(p, &p.lets[*id as usize], seen_ops),
        _ => true,
    };
    if !ok {
        return false;
    }
    let mut all = true;
    visit(e, &mut |x| all &= equivariant(p, x, seen_ops));
    all
}

/// Every state variable an expression can read, through operator calls
/// and LET bodies.
pub fn deps(p: &Program, e: &Expr, acc: &mut [bool], seen_ops: &mut [bool]) {
    match e {
        Expr::Var(i) | Expr::Primed(i) => acc[*i as usize] = true,
        Expr::Call(op, _) => {
            let op = *op as usize;
            if !seen_ops[op] {
                seen_ops[op] = true;
                deps(p, &p.ops[op].body, acc, seen_ops);
            }
        }
        Expr::LetRef(_, id) => deps(p, &p.lets[*id as usize], acc, seen_ops),
        // an action can read any variable: be conservative
        Expr::Enabled(_) => acc.iter_mut().for_each(|x| *x = true),
        _ => {}
    }
    visit(e, &mut |x| deps(p, x, acc, seen_ops));
}

/// The VIEW split into the parts the key hashes one by one, each with the
/// frame it evaluates in: a VIEW naming a parameterless operator whose
/// body is a tuple is keyed by that tuple's items, a tuple VIEW by its
/// items, anything else as one part. None without a VIEW. The generator
/// compiles exactly these parts, so the indices agree (`Engine::view_part`).
pub fn view_parts(p: &Program) -> Option<Vec<(&Expr, u32)>> {
    let v = p.view.as_ref()?;
    Some(match &v.body {
        Expr::Call(op, args) if args.is_empty() => match &p.ops[*op as usize].body {
            Expr::Tuple(items) => items.iter().map(|e| (e, p.ops[*op as usize].frame)).collect(),
            body => vec![(body, p.ops[*op as usize].frame)],
        },
        Expr::Tuple(items) => items.iter().map(|e| (e, v.frame)).collect(),
        body => vec![(body, v.frame)],
    })
}

impl<'p> SymKey<'p> {
    /// None when the plain per-variable fingerprint applies.
    pub fn new(p: &'p Program, e: &'p dyn Engine) -> Option<SymKey<'p>> {
        if p.view.is_none() && p.symmetry.is_empty() {
            return None;
        }
        let nv = p.vars.len();
        let mk = |e: &'p Expr, frame: u32| {
            if let Expr::Var(i) = e {
                return Comp::Var(*i as usize);
            }
            let mut acc = vec![false; nv];
            deps(p, e, &mut acc, &mut vec![false; p.ops.len()]);
            let equivariant = equivariant(p, e, &mut vec![0; p.ops.len()]);
            Comp::Expr { e, frame, deps: (0..nv).filter(|&i| acc[i]).collect(), equivariant }
        };
        let comps = match view_parts(p) {
            None => (0..nv).map(Comp::Var).collect(),
            Some(parts) => parts.into_iter().map(|(e, frame)| mk(e, frame)).collect(),
        };
        let inv = p
            .symmetry
            .iter()
            .map(|q| {
                let mut v = vec![0u32; q.len()];
                for (a, &b) in q.iter().enumerate() {
                    v[b as usize] = a as u32;
                }
                v
            })
            .collect();
        Some(SymKey { p, e, np: 1 + p.symmetry.len(), inv, comps })
    }

    pub fn reset(&self, m: &mut Memo) {
        let (np, nv, nc) = (self.np, self.p.vars.len(), self.comps.len());
        m.img.clear();
        m.img.resize(np * nv, None);
        m.cmp.clear();
        m.cmp.resize(np * nv, None);
        m.vh.clear();
        m.vh.resize(np * nv, None);
        m.ch.clear();
        m.ch.resize(np * nc, None);
        m.cv.clear();
        m.cv.resize(nc, None);
    }

    /// Variable i of the image of `st` under permutation k.
    fn img(&self, k: usize, i: usize, st: &[Value], changed: &[bool], pm: &mut Memo, sm: &mut Memo) -> Value {
        if k == 0 {
            return st[i].clone();
        }
        let m = if changed[i] { sm } else { pm };
        let slot = &mut m.img[k * st.len() + i];
        if slot.is_none() {
            *slot = Some(st[i].permute_opt(&self.p.symmetry[k - 1]));
        }
        match slot.as_ref().unwrap() {
            Some(v) => v.clone(),
            None => st[i].clone(),
        }
    }

    /// How variable i's image under k compares with variable i itself.
    fn cmp_id(&self, k: usize, i: usize, st: &[Value], changed: &[bool], pm: &mut Memo, sm: &mut Memo) -> Ordering {
        let at = k * st.len() + i;
        let cached = if changed[i] { sm.cmp[at] } else { pm.cmp[at] };
        if let Some(c) = cached {
            return c;
        }
        let c = st[i].cmp_perm(&self.p.symmetry[k - 1], &self.inv[k - 1]);
        if changed[i] { sm.cmp[at] = Some(c) } else { pm.cmp[at] = Some(c) }
        c
    }

    fn vh(&self, k: usize, i: usize, st: &[Value], changed: &[bool], pm: &mut Memo, sm: &mut Memo) -> u64 {
        let at = k * st.len() + i;
        let cached = if changed[i] { sm.vh[at] } else { pm.vh[at] };
        if let Some(h) = cached {
            return h;
        }
        let h = if k == 0 { var_hash(&st[i]) } else { st[i].hash_perm(&self.p.symmetry[k - 1], &self.inv[k - 1]) };
        if changed[i] { sm.vh[at] = Some(h) } else { pm.vh[at] = Some(h) }
        h
    }

    fn ch(&self, k: usize, c: usize, st: &[Value], changed: &[bool], pm: &mut Memo, sm: &mut Memo, bufs: &mut Bufs) -> R<u64> {
        let Comp::Expr { e, frame, deps, equivariant } = &self.comps[c] else { unreachable!() };
        let stale = deps.iter().any(|&i| changed[i]);
        let at = k * self.comps.len() + c;
        let cached = if stale { sm.ch[at] } else { pm.ch[at] };
        if let Some(h) = cached {
            return Ok(h);
        }
        let h = if *equivariant {
            // evaluate on the state itself (memoized as the value's slot),
            // then hash its image
            let v = {
                let m = if stale { &mut *sm } else { &mut *pm };
                match &m.cv[c] {
                    Some(v) => v.clone(),
                    None => {
                        let v = self.part(c, e, *frame, st, bufs)?;
                        m.cv[c] = Some(v.clone());
                        v
                    }
                }
            };
            if k == 0 { var_hash(&v) } else { v.hash_perm(&self.p.symmetry[k - 1], &self.inv[k - 1]) }
        } else {
            let image: Vec<Value> = (0..st.len()).map(|i| self.img(k, i, st, changed, pm, sm)).collect();
            var_hash(&self.part(c, e, *frame, &image, bufs)?)
        };
        if stale { sm.ch[at] = Some(h) } else { pm.ch[at] = Some(h) }
        Ok(h)
    }

    /// VIEW part `c` on `st`: the engine's compiled code when it has it,
    /// else the interpreter.
    fn part(&self, c: usize, e: &Expr, frame: u32, st: &[Value], bufs: &mut Bufs) -> R<Value> {
        let v = match self.e.view_part(c, st) {
            Some(r) => r,
            None => {
                let mut cx = bufs.cx(st, st.len(), frame);
                self.p.eval(e, &mut cx)
            }
        };
        v.map_err(|e| format!("evaluating VIEW: {e}"))?.normalized()
    }

    /// The key of `st`. `changed[i]` says variable i differs from the
    /// state `pm` was filled for; with all false, `pm` is `st`'s own memo.
    pub fn key(&self, st: &[Value], changed: &[bool], pm: &mut Memo, sm: &mut Memo, bufs: &mut Bufs) -> R<u64> {
        let mut least = 0;
        for k in 1..self.np {
            let mut c = Ordering::Equal;
            for &i in &self.p.cmp_order {
                c = if least == 0 {
                    self.cmp_id(k, i, st, changed, pm, sm)
                } else {
                    let a = self.img(k, i, st, changed, pm, sm);
                    a.cmp(&self.img(least, i, st, changed, pm, sm))
                };
                if c != Ordering::Equal {
                    break;
                }
            }
            if c == Ordering::Less {
                least = k;
            }
        }
        let mut hs = Vec::with_capacity(self.comps.len());
        for (ci, comp) in self.comps.iter().enumerate() {
            hs.push(match comp {
                Comp::Var(i) => self.vh(least, *i, st, changed, pm, sm),
                Comp::Expr { .. } => self.ch(least, ci, st, changed, pm, sm, bufs)?,
            });
        }
        Ok(combine(&hs))
    }

    /// The key of a state with no parent to reuse.
    pub fn key_alone(&self, st: &[Value], s: &mut Scratch) -> R<u64> {
        self.reset(&mut s.parent);
        s.changed.clear();
        s.changed.resize(st.len(), false);
        self.key(st, &s.changed, &mut s.parent, &mut s.succ, &mut s.bufs)
    }
}
