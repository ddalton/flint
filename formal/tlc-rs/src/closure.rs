//! The closure compiler: the resolved IR, turned once into a tree of Rust
//! closures. Evaluation then runs no `match` on node kinds; each closure
//! already knows what it is. Three specializations do most of the work:
//!  - boolean formulas compile to `bool` closures (no `Value::Bool` box);
//!  - paths into the state or a constant (`sess[h].tok`) compile to
//!    closures returning a reference, so reading one clones only the leaf;
//!  - a guard inside a conjunction list is tested inline instead of
//!    through another continuation.
//! Locals still live in the frame stack, exactly as in the interpreter.

use crate::eval::*;
use crate::value::{Lazy, Value, R};
use std::sync::{Arc, OnceLock};

type VF = Box<dyn Fn(&mut Cx) -> R<Value> + Send + Sync>;
type BF = Box<dyn Fn(&mut Cx) -> R<bool> + Send + Sync>;
/// A reference into the state (lifetime 'a) or into a leaked constant.
type PF = Box<dyn for<'a> Fn(&'a [Value], &mut Cx) -> R<&'a Value> + Send + Sync>;
type AF = Box<dyn for<'a> Fn(&mut Cx<'a>, K<'_, 'a>) -> R<()> + Send + Sync>;

struct CBound {
    slots: Box<[u32]>,
    tuple: bool,
    set: VF,
}

struct OpC {
    body: VF,
    frame: usize,
}

#[derive(Default)]
struct Table {
    ops: OnceLock<Vec<OpC>>,
    lets: OnceLock<Vec<VF>>,
}

pub struct Closures {
    init: AF,
    next: AF,
    invariants: Vec<BF>,
    constraints: Vec<BF>,
    view: Option<VF>,
}

impl Engine for Closures {
    fn init<'a>(&self, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
        (self.init)(cx, k)
    }
    fn next<'a>(&self, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
        (self.next)(cx, k)
    }
    fn invariant(&self, i: usize, cx: &mut Cx) -> R<bool> {
        (self.invariants[i])(cx)
    }
    fn constraint(&self, i: usize, cx: &mut Cx) -> R<bool> {
        (self.constraints[i])(cx)
    }
    fn view(&self, cx: &mut Cx) -> R<Value> {
        (self.view.as_ref().ok_or("no VIEW")?)(cx)
    }
}

pub fn compile(p: &Program) -> R<Closures> {
    let c = C { p, t: Arc::new(Table::default()) };
    let ops = p
        .ops
        .iter()
        .map(|o| OpC { body: c.val(&o.body), frame: o.frame as usize })
        .collect();
    let _ = c.t.ops.set(ops);
    let _ = c.t.lets.set(p.lets.iter().map(|e| c.val(e)).collect());
    Ok(Closures {
        init: c.act(&p.init.body).into_af(),
        next: c.act(&p.next.body).into_af(),
        invariants: p.invariants.iter().map(|r| c.boolean(&r.body)).collect(),
        constraints: p.constraints.iter().map(|r| c.boolean(&r.body)).collect(),
        view: p.view.as_ref().map(|r| c.val(&r.body)),
    })
}

struct C<'p> {
    p: &'p Program,
    t: Arc<Table>,
}

enum A {
    Guard(BF),
    Act(AF),
}

impl A {
    fn into_af(self) -> AF {
        match self {
            A::Act(f) => f,
            A::Guard(g) => Box::new(move |cx, k| if g(cx)? { k(cx) } else { Ok(()) }),
        }
    }
}

#[inline]
fn bind(b: &CBound, x: &Value, cx: &mut Cx) -> R<()> {
    if b.tuple {
        match x {
            Value::Seq(s) if s.len() == b.slots.len() => {
                for (slot, v) in b.slots.iter().zip(s.iter()) {
                    cx.set(*slot, v.clone());
                }
                Ok(())
            }
            _ => Err(format!("cannot destructure {x} as a {}-tuple", b.slots.len())),
        }
    } else {
        cx.set(b.slots[0], x.clone());
        Ok(())
    }
}

fn each(bs: &[CBound], i: usize, cx: &mut Cx, f: &mut dyn FnMut(&mut Cx) -> R<bool>) -> R<bool> {
    if i == bs.len() {
        return f(cx);
    }
    let set = (bs[i].set)(cx)?.elems()?;
    for x in set.iter() {
        bind(&bs[i], x, cx)?;
        if !each(bs, i + 1, cx, f)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn leak(v: &Value) -> &'static Value {
    Box::leak(Box::new(v.clone()))
}

impl C<'_> {
    fn bounds(&self, bs: &[Bound]) -> Box<[CBound]> {
        bs.iter().map(|b| CBound { slots: b.slots.clone(), tuple: b.tuple, set: self.val(&b.set) }).collect()
    }

    /// A closure returning a reference, when `e` is a path rooted in the
    /// state or in a constant.
    fn path(&self, e: &Expr) -> Option<PF> {
        Some(match e {
            Expr::Var(i) => {
                let i = *i as usize;
                Box::new(move |st, _| Ok(&st[i]))
            }
            Expr::Const(v) => {
                let v = leak(v);
                Box::new(move |_, _| Ok(v))
            }
            Expr::Call(op, args) if args.is_empty() && self.p.ops[*op as usize].cached.is_some() => {
                let v = leak(self.p.ops[*op as usize].cached.as_ref().unwrap());
                Box::new(move |_, _| Ok(v))
            }
            Expr::App(f, x) => {
                let f = self.path(f)?;
                if let Some(xp) = self.path(x) {
                    Box::new(move |st, cx| {
                        let k = xp(st, cx)?;
                        f(st, cx)?.apply_ref(k)
                    })
                } else {
                    let x = self.val(x);
                    Box::new(move |st, cx| {
                        let k = x(cx)?;
                        f(st, cx)?.apply_ref(&k)
                    })
                }
            }
            Expr::Field(r, id) => {
                let r = self.path(r)?;
                let id = *id;
                Box::new(move |st, cx| r(st, cx)?.field_ref(id))
            }
            _ => return None,
        })
    }

    /// Applies `f` to the two operands, borrowing where a path allows.
    fn pair(&self, a: &Expr, b: &Expr, f: fn(&Value, &Value) -> R<bool>) -> BF {
        match (self.path(a), self.path(b)) {
            (Some(pa), Some(pb)) => Box::new(move |cx| {
                let st = cx.state;
                let x = pa(st, cx)?;
                f(x, pb(st, cx)?)
            }),
            (Some(pa), None) => {
                let vb = self.val(b);
                Box::new(move |cx| {
                    let st = cx.state;
                    let x = pa(st, cx)?;
                    f(x, &vb(cx)?)
                })
            }
            (None, Some(pb)) => {
                let va = self.val(a);
                Box::new(move |cx| {
                    let x = va(cx)?;
                    let st = cx.state;
                    f(&x, pb(st, cx)?)
                })
            }
            (None, None) => {
                let (va, vb) = (self.val(a), self.val(b));
                Box::new(move |cx| {
                    let x = va(cx)?;
                    f(&x, &vb(cx)?)
                })
            }
        }
    }

    fn int_pair(&self, a: &Expr, b: &Expr) -> (VF, VF) {
        (self.val(a), self.val(b))
    }

    fn boolean(&self, e: &Expr) -> BF {
        match e {
            Expr::Const(Value::Bool(b)) => {
                let b = *b;
                Box::new(move |_| Ok(b))
            }
            Expr::Not(a) => {
                let a = self.boolean(a);
                Box::new(move |cx| Ok(!a(cx)?))
            }
            Expr::And(v) => {
                let v: Vec<BF> = v.iter().map(|x| self.boolean(x)).collect();
                Box::new(move |cx| {
                    for a in &v {
                        if !a(cx)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })
            }
            Expr::Or(v) => {
                let v: Vec<BF> = v.iter().map(|x| self.boolean(x)).collect();
                Box::new(move |cx| {
                    for a in &v {
                        if a(cx)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                })
            }
            Expr::Implies(a, b) => {
                let (a, b) = (self.boolean(a), self.boolean(b));
                Box::new(move |cx| Ok(!a(cx)? || b(cx)?))
            }
            Expr::Quant(forall, bs, body) => {
                let forall = *forall;
                let bs = self.bounds(bs);
                let body = self.boolean(body);
                Box::new(move |cx| {
                    // `each` stops early when the callback returns false.
                    let mut hit = false;
                    each(&bs, 0, cx, &mut |cx| {
                        let r = body(cx)?;
                        if r != forall {
                            hit = true;
                            return Ok(false);
                        }
                        Ok(true)
                    })?;
                    Ok(if hit { !forall } else { forall })
                })
            }
            Expr::Bin(Bin::Eq, a, b) => self.pair(a, b, |x, y| Ok(x == y)),
            Expr::Bin(Bin::Neq, a, b) => self.pair(a, b, |x, y| Ok(x != y)),
            Expr::Bin(Bin::In, a, b) => self.pair(a, b, |x, s| s.contains(x)),
            Expr::Bin(Bin::NotIn, a, b) => self.pair(a, b, |x, s| Ok(!s.contains(x)?)),
            Expr::Bin(op @ (Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge), a, b) => {
                let (a, b) = self.int_pair(a, b);
                match op {
                    Bin::Lt => Box::new(move |cx| Ok(a(cx)?.as_int()? < b(cx)?.as_int()?)),
                    Bin::Le => Box::new(move |cx| Ok(a(cx)?.as_int()? <= b(cx)?.as_int()?)),
                    Bin::Gt => Box::new(move |cx| Ok(a(cx)?.as_int()? > b(cx)?.as_int()?)),
                    _ => Box::new(move |cx| Ok(a(cx)?.as_int()? >= b(cx)?.as_int()?)),
                }
            }
            Expr::Bin(Bin::Equiv, a, b) => {
                let (a, b) = (self.boolean(a), self.boolean(b));
                Box::new(move |cx| Ok(a(cx)? == b(cx)?))
            }
            _ => {
                if let Some(p) = self.path(e) {
                    return Box::new(move |cx| {
                        let st = cx.state;
                        p(st, cx)?.as_bool()
                    });
                }
                let v = self.val(e);
                Box::new(move |cx| v(cx)?.as_bool())
            }
        }
    }

    fn val(&self, e: &Expr) -> VF {
        match e {
            // not memoized here: the closure engine never evaluates a
            // step property, and a state-level memo only saves time
            Expr::Memo(_, _, e) => self.val(e),
            Expr::Const(v) => {
                let v = v.clone();
                Box::new(move |_| Ok(v.clone()))
            }
            Expr::Local(i) => {
                let i = *i as usize;
                Box::new(move |cx| Ok(cx.stack[cx.base + i].clone()))
            }
            Expr::Var(i) => {
                let i = *i as usize;
                Box::new(move |cx| Ok(cx.state[i].clone()))
            }
            Expr::Primed(i) => {
                let i = *i as usize;
                let name = self.p.vars[i].clone();
                Box::new(move |cx| match &cx.next[i] {
                    Some(v) => Ok(v.clone()),
                    None => Err(format!("{name}' is read before the action assigns it")),
                })
            }
            Expr::Call(op, args) => {
                let op = *op as usize;
                if let Some(v) = &self.p.ops[op].cached {
                    let v = v.clone();
                    return Box::new(move |_| Ok(v.clone()));
                }
                let args: Vec<VF> = args.iter().map(|a| self.val(a)).collect();
                let t = self.t.clone();
                Box::new(move |cx| {
                    let o = &t.ops.get().unwrap()[op];
                    let (base, frame) = (cx.base, cx.frame);
                    let nb = base + frame;
                    let need = nb + o.frame.max(args.len());
                    if cx.stack.len() < need {
                        cx.stack.resize(need, Value::Bool(false));
                        cx.ok.resize(need, false);
                    }
                    for (i, a) in args.iter().enumerate() {
                        cx.frame = frame + i;
                        let v = a(cx)?;
                        cx.stack[nb + i] = v;
                    }
                    cx.base = nb;
                    cx.frame = o.frame;
                    let r = (o.body)(cx);
                    cx.base = base;
                    cx.frame = frame;
                    r
                })
            }
            Expr::Not(_) | Expr::And(_) | Expr::Or(_) | Expr::Implies(..) | Expr::Quant(..) => {
                let b = self.boolean(e);
                Box::new(move |cx| Ok(Value::Bool(b(cx)?)))
            }
            Expr::Bin(Bin::Eq | Bin::Neq | Bin::In | Bin::NotIn | Bin::Equiv | Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge, ..) => {
                let b = self.boolean(e);
                Box::new(move |cx| Ok(Value::Bool(b(cx)?)))
            }
            Expr::If(c, a, b) => {
                let (c, a, b) = (self.boolean(c), self.val(a), self.val(b));
                Box::new(move |cx| if c(cx)? { a(cx) } else { b(cx) })
            }
            Expr::Case(arms, other) => {
                let arms: Vec<(BF, VF)> = arms.iter().map(|(p, v)| (self.boolean(p), self.val(v))).collect();
                let other = other.as_ref().map(|o| self.val(o));
                Box::new(move |cx| {
                    for (p, v) in &arms {
                        if p(cx)? {
                            return v(cx);
                        }
                    }
                    match &other {
                        Some(o) => o(cx),
                        None => Err("no CASE arm matched".into()),
                    }
                })
            }
            Expr::Let(defs, body) => {
                let defs: Vec<(u32, VF)> = defs.iter().map(|(s, d)| (*s, self.val(d))).collect();
                let body = self.val(body);
                Box::new(move |cx| {
                    for (s, d) in &defs {
                        let v = d(cx)?;
                        cx.set(*s, v);
                    }
                    body(cx)
                })
            }
            Expr::Lazy(slots, body) => {
                let slots = slots.clone();
                let body = self.val(body);
                Box::new(move |cx| {
                    for (s, _) in slots.iter() {
                        cx.ok[cx.base + *s as usize] = false;
                    }
                    body(cx)
                })
            }
            Expr::LetRef(slot, id) => {
                let (slot, id) = (*slot as usize, *id as usize);
                let t = self.t.clone();
                Box::new(move |cx| {
                    let i = cx.base + slot;
                    if !cx.ok[i] {
                        let v = (t.lets.get().unwrap()[id])(cx)?;
                        cx.stack[i] = v;
                        cx.ok[i] = true;
                    }
                    Ok(cx.stack[i].clone())
                })
            }
            Expr::Enabled(a) => {
                let a = self.act(a).into_af();
                Box::new(move |cx| {
                    let n = cx.next.len();
                    let saved = std::mem::replace(cx.next, vec![None; n]);
                    let mut found = false;
                    let r = a(cx, &mut |_| {
                        found = true;
                        Err(ENABLED_STOP.into())
                    });
                    *cx.next = saved;
                    match r {
                        Err(e) if e != ENABLED_STOP => Err(e),
                        _ => Ok(Value::Bool(found)),
                    }
                })
            }
            Expr::SelectSeq(seq, slot, pred) => {
                let (seq, slot, pred) = (self.val(seq), *slot, self.boolean(pred));
                Box::new(move |cx| {
                    let Value::Seq(xs) = seq(cx)? else { return Err("SelectSeq of a non-sequence".into()) };
                    let mut out = Vec::with_capacity(xs.len());
                    for x in xs.iter() {
                        cx.set(slot, x.clone());
                        if pred(cx)? {
                            out.push(x.clone());
                        }
                    }
                    Ok(Value::Seq(out.into()))
                })
            }
            Expr::Choose(b, p) => {
                let b = CBound { slots: b.slots.clone(), tuple: b.tuple, set: self.val(&b.set) };
                let p = self.boolean(p);
                Box::new(move |cx| {
                    let set = (b.set)(cx)?.elems()?;
                    for x in set.iter() {
                        bind(&b, x, cx)?;
                        if p(cx)? {
                            return Ok(x.clone());
                        }
                    }
                    Err("CHOOSE found no element satisfying its predicate".into())
                })
            }
            Expr::SetEnum(v) => {
                let v: Vec<VF> = v.iter().map(|x| self.val(x)).collect();
                Box::new(move |cx| {
                    let mut out = Vec::with_capacity(v.len());
                    for a in &v {
                        out.push(a(cx)?);
                    }
                    Value::set(out)
                })
            }
            Expr::SetFilter(b, p) => {
                let b = CBound { slots: b.slots.clone(), tuple: b.tuple, set: self.val(&b.set) };
                let p = self.boolean(p);
                Box::new(move |cx| {
                    let set = (b.set)(cx)?.elems()?;
                    let mut out = Vec::new();
                    for x in set.iter() {
                        bind(&b, x, cx)?;
                        if p(cx)? {
                            out.push(x.clone());
                        }
                    }
                    Ok(if out.len() == set.len() { Value::Set(set) } else { Value::set_sorted(out) })
                })
            }
            Expr::SetMap(bs, body) => {
                let bs = self.bounds(bs);
                let body = self.val(body);
                Box::new(move |cx| {
                    let mut out = Vec::new();
                    each(&bs, 0, cx, &mut |cx| {
                        out.push(body(cx)?);
                        Ok(true)
                    })?;
                    Value::set(out)
                })
            }
            Expr::FuncCons(bs, body) => {
                let single = bs.len() == 1 && !bs[0].tuple;
                let bs = self.bounds(bs);
                let body = self.val(body);
                if single {
                    return Box::new(move |cx| {
                        let set = (bs[0].set)(cx)?.elems()?;
                        let mut pairs = Vec::with_capacity(set.len());
                        let slot = bs[0].slots[0];
                        for x in set.iter() {
                            cx.set(slot, x.clone());
                            pairs.push((x.clone(), body(cx)?.normalized()?));
                        }
                        Ok(Value::func_sorted(pairs))
                    });
                }
                Box::new(move |cx| {
                    let mut pairs = Vec::new();
                    each(&bs, 0, cx, &mut |cx| {
                        let key: Vec<Value> = bs
                            .iter()
                            .flat_map(|b| b.slots.iter())
                            .map(|s| cx.stack[cx.base + *s as usize].clone())
                            .collect();
                        let key = if bs.len() == 1 && bs[0].tuple || key.len() > 1 {
                            Value::Seq(key.into())
                        } else {
                            key.into_iter().next().unwrap()
                        };
                        pairs.push((key, body(cx)?));
                        Ok(true)
                    })?;
                    Value::func(pairs)
                })
            }
            Expr::Record(fields) => {
                let fields: Vec<(u32, VF)> = fields.iter().map(|(f, a)| (*f, self.val(a))).collect();
                Box::new(move |cx| {
                    let mut pairs = Vec::with_capacity(fields.len());
                    for (f, a) in &fields {
                        pairs.push((Value::Str(*f), a(cx)?.normalized()?));
                    }
                    Ok(Value::func_sorted(pairs))
                })
            }
            Expr::RecSet(fields) => {
                let fields: Vec<(u32, VF)> = fields.iter().map(|(f, a)| (*f, self.val(a))).collect();
                Box::new(move |cx| {
                    let mut fs = Vec::with_capacity(fields.len());
                    for (f, a) in &fields {
                        fs.push((*f, a(cx)?));
                    }
                    Ok(Value::Lazy(Arc::new(Lazy::RecSet(fs.into()))))
                })
            }
            Expr::FuncSet(d, r) => {
                let (d, r) = (self.val(d), self.val(r));
                Box::new(move |cx| Ok(Value::Lazy(Arc::new(Lazy::FuncSet(d(cx)?, r(cx)?)))))
            }
            Expr::Product(v) => {
                let v: Vec<VF> = v.iter().map(|x| self.val(x)).collect();
                Box::new(move |cx| {
                    let mut sets = Vec::with_capacity(v.len());
                    for a in &v {
                        sets.push(a(cx)?);
                    }
                    Ok(Value::Lazy(Arc::new(Lazy::Product(sets.into()))))
                })
            }
            Expr::Tuple(v) => {
                let v: Vec<VF> = v.iter().map(|x| self.val(x)).collect();
                Box::new(move |cx| {
                    let mut out = Vec::with_capacity(v.len());
                    for a in &v {
                        out.push(a(cx)?.normalized()?);
                    }
                    Ok(Value::Seq(out.into()))
                })
            }
            Expr::App(..) | Expr::Field(..) if self.path(e).is_some() => {
                let p = self.path(e).unwrap();
                Box::new(move |cx| {
                    let st = cx.state;
                    Ok(p(st, cx)?.clone())
                })
            }
            Expr::App(f, x) => {
                let (f, x) = (self.val(f), self.val(x));
                Box::new(move |cx| {
                    let fv = f(cx)?;
                    let xv = x(cx)?;
                    Ok(fv.apply_ref(&xv)?.clone())
                })
            }
            Expr::Field(r, id) => {
                let (r, id) = (self.val(r), *id);
                Box::new(move |cx| Ok(r(cx)?.field_ref(id)?.clone()))
            }
            Expr::Except(f, ups) => {
                let f = self.val(f);
                let ups: Vec<(Vec<PathC>, u32, VF)> = ups
                    .iter()
                    .map(|u| {
                        let path = u
                            .path
                            .iter()
                            .map(|p| match p {
                                PathE::Idx(e) => PathC::Idx(self.val(e)),
                                PathE::Field(id) => PathC::Field(*id),
                            })
                            .collect();
                        (path, u.at, self.val(&u.val))
                    })
                    .collect();
                Box::new(move |cx| {
                    let mut v = f(cx)?;
                    for (path, at, val) in &ups {
                        v = except_path(v, path, *at, val, cx)?;
                    }
                    Ok(v)
                })
            }
            Expr::Bin(op, a, b) => {
                let (a, b, op) = (self.val(a), self.val(b), *op);
                match op {
                    Bin::Plus => Box::new(move |cx| Ok(Value::Int(a(cx)?.as_int()? + b(cx)?.as_int()?))),
                    Bin::Minus => Box::new(move |cx| Ok(Value::Int(a(cx)?.as_int()? - b(cx)?.as_int()?))),
                    _ => Box::new(move |cx| {
                        let x = a(cx)?;
                        binop(op, x, b(cx)?)
                    }),
                }
            }
            Expr::Un(op, a) => {
                let (a, op) = (self.val(a), *op);
                Box::new(move |cx| {
                    let x = a(cx)?;
                    Ok(match op {
                        Un::Neg => Value::Int(-x.as_int()?),
                        Un::Subset => Value::Lazy(Arc::new(Lazy::Subset(x))),
                        Un::Domain => return x.domain(),
                        Un::Union => {
                            let mut out = Vec::new();
                            for s in x.elems()?.iter() {
                                out.extend(s.elems()?.iter().cloned());
                            }
                            return Value::set(out);
                        }
                    })
                })
            }
            Expr::Builtin(bi, args) => {
                let (bi, args) = (*bi, args.iter().map(|a| self.val(a)).collect::<Vec<VF>>());
                Box::new(move |cx| {
                    let mut v = Vec::with_capacity(args.len());
                    for a in &args {
                        v.push(a(cx)?);
                    }
                    builtin_apply(bi, v)
                })
            }
        }
    }

    // ---- actions --------------------------------------------------------

    fn act(&self, a: &Act) -> A {
        match a {
            Act::Guard(e) => A::Guard(self.boolean(e)),
            Act::And(v) => {
                let parts: Vec<A> = v.iter().map(|x| self.act(x)).collect();
                and(parts)
            }
            Act::Or(v) => {
                let v: Vec<AF> = v.iter().map(|x| self.act(x).into_af()).collect();
                A::Act(Box::new(move |cx, k| {
                    for b in &v {
                        b(cx, k)?;
                    }
                    Ok(())
                }))
            }
            Act::Exists(bs, body) => {
                let bs = self.bounds(bs);
                let body = self.act(body).into_af();
                A::Act(Box::new(move |cx, k| run_exists(&bs, 0, &body, cx, k)))
            }
            Act::If(c, t, e) => {
                let (c, t, e) = (self.boolean(c), self.act(t).into_af(), self.act(e).into_af());
                A::Act(Box::new(move |cx, k| if c(cx)? { t(cx, k) } else { e(cx, k) }))
            }
            Act::Case(arms, other) => {
                let arms: Vec<(BF, AF)> = arms.iter().map(|(p, b)| (self.boolean(p), self.act(b).into_af())).collect();
                let other = other.as_ref().map(|o| self.act(o).into_af());
                A::Act(Box::new(move |cx, k| {
                    for (p, b) in &arms {
                        if p(cx)? {
                            return b(cx, k);
                        }
                    }
                    match &other {
                        Some(o) => o(cx, k),
                        None => Err("no CASE arm matched".into()),
                    }
                }))
            }
            Act::Let(defs, body) => {
                let defs: Vec<(u32, VF)> = defs.iter().map(|(s, d)| (*s, self.val(d))).collect();
                let body = self.act(body).into_af();
                A::Act(Box::new(move |cx, k| {
                    for (s, d) in &defs {
                        let v = d(cx)?;
                        cx.set(*s, v);
                    }
                    body(cx, k)
                }))
            }
            Act::Lazy(slots, body) => {
                let slots = slots.clone();
                let body = self.act(body).into_af();
                A::Act(Box::new(move |cx, k| {
                    for (s, _) in slots.iter() {
                        cx.ok[cx.base + *s as usize] = false;
                    }
                    body(cx, k)
                }))
            }
            Act::Assign(var, e) => {
                let (var, e) = (*var as usize, self.val(e));
                A::Act(Box::new(move |cx, k| {
                    let v = e(cx)?.normalized()?;
                    assign(var, v, cx, k)
                }))
            }
            Act::AssignIn(var, s) => {
                let (var, s) = (*var as usize, self.val(s));
                A::Act(Box::new(move |cx, k| {
                    let set = s(cx)?.elems()?;
                    for x in set.iter() {
                        assign(var, x.clone(), cx, k)?;
                    }
                    Ok(())
                }))
            }
            Act::Unchanged(vars) => {
                let vars = vars.clone();
                A::Act(Box::new(move |cx, k| {
                    let mut set_here: u128 = 0;
                    let mut ok = true;
                    for (i, &var) in vars.iter().enumerate() {
                        let var = var as usize;
                        match &cx.next[var] {
                            None => {
                                cx.next[var] = Some(cx.state[var].clone());
                                set_here |= 1 << i;
                            }
                            Some(v) => {
                                if *v != cx.state[var] {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                    }
                    let r = if ok { k(cx) } else { Ok(()) };
                    for (i, &var) in vars.iter().enumerate() {
                        if set_here & (1 << i) != 0 {
                            cx.next[var as usize] = None;
                        }
                    }
                    r
                }))
            }
        }
    }
}

/// A conjunction list: guards are tested inline, and only real actions
/// open a continuation.
fn and(mut parts: Vec<A>) -> A {
    if parts.len() == 1 {
        return parts.pop().unwrap();
    }
    let first = parts.remove(0);
    let rest = and(parts);
    match (first, rest) {
        (A::Guard(g), A::Guard(h)) => A::Guard(Box::new(move |cx| Ok(g(cx)? && h(cx)?))),
        (A::Guard(g), A::Act(r)) => A::Act(Box::new(move |cx, k| if g(cx)? { r(cx, k) } else { Ok(()) })),
        (A::Act(f), A::Guard(h)) => A::Act(Box::new(move |cx, k| f(cx, &mut |cx| if h(cx)? { k(cx) } else { Ok(()) }))),
        (A::Act(f), A::Act(r)) => A::Act(Box::new(move |cx, k| f(cx, &mut |cx| r(cx, k)))),
    }
}

#[inline]
fn assign<'a>(var: usize, v: Value, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
    match &cx.next[var] {
        None => {
            cx.next[var] = Some(v);
            let r = k(cx);
            cx.next[var] = None;
            r
        }
        Some(old) => {
            if *old == v {
                k(cx)
            } else {
                Ok(())
            }
        }
    }
}

fn run_exists<'a>(bs: &[CBound], i: usize, body: &AF, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
    if i == bs.len() {
        return body(cx, k);
    }
    let set = (bs[i].set)(cx)?.elems()?;
    for x in set.iter() {
        bind(&bs[i], x, cx)?;
        run_exists(bs, i + 1, body, cx, k)?;
    }
    Ok(())
}

enum PathC {
    Idx(VF),
    Field(u32),
}

fn except_path(f: Value, path: &[PathC], at: u32, val: &VF, cx: &mut Cx) -> R<Value> {
    let key = match &path[0] {
        PathC::Idx(e) => e(cx)?,
        PathC::Field(id) => Value::Str(*id),
    };
    let Ok(old) = f.apply(&key) else { return Ok(f) };
    let new = if path.len() == 1 {
        cx.set(at, old);
        val(cx)?.normalized()?
    } else {
        except_path(old, &path[1..], at, val, cx)?
    };
    f.except(&key, new)
}
