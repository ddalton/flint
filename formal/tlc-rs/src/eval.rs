//! The compiled IR and its interpreter.
//!
//! Names are resolved at compile time. A local is a slot in the current
//! frame (`stack[base + slot]`); an operator call opens a new frame above
//! the caller's. Inside one action tree every binder gets its own slot, so
//! a continuation never clobbers a binding an enclosing `\E` still needs.

use crate::value::{Lazy, Value, R};
use std::sync::Arc;

pub enum Expr {
    /// a zero-argument operator evaluated at most once per evaluation
    /// context (an instance's substituted state function): the memo slot,
    /// whether it reads the next state, and the call
    Memo(u32, bool, Box<Expr>),
    Const(Value),
    Local(u32),
    Var(u32),
    Primed(u32),
    Call(u32, Box<[Expr]>),
    Not(Box<Expr>),
    And(Box<[Expr]>),
    Or(Box<[Expr]>),
    Implies(Box<Expr>, Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    Case(Box<[(Expr, Expr)]>, Option<Box<Expr>>),
    Let(Box<[(u32, Expr)]>, Box<Expr>),
    /// LET definitions: marks the slots unevaluated; `LetRef` fills one
    /// on first use. TLA+ LET is lazy, and specs rely on it (a definition
    /// that is only meaningful behind a later guard).
    Lazy(Box<[(u32, u32)]>, Box<Expr>),
    LetRef(u32, u32),
    SelectSeq(Box<Expr>, u32, Box<Expr>),
    /// `ENABLED A`: A has a successor from the current state
    Enabled(Box<Act>),
    Quant(bool, Box<[Bound]>, Box<Expr>),
    Choose(Box<Bound>, Box<Expr>),
    SetEnum(Box<[Expr]>),
    SetFilter(Box<Bound>, Box<Expr>),
    SetMap(Box<[Bound]>, Box<Expr>),
    FuncCons(Box<[Bound]>, Box<Expr>),
    /// fields sorted by interned id
    Record(Box<[(u32, Expr)]>),
    RecSet(Box<[(u32, Expr)]>),
    FuncSet(Box<Expr>, Box<Expr>),
    Product(Box<[Expr]>),
    Tuple(Box<[Expr]>),
    App(Box<Expr>, Box<Expr>),
    Field(Box<Expr>, u32),
    Except(Box<Expr>, Box<[Update]>),
    Bin(Bin, Box<Expr>, Box<Expr>),
    Un(Un, Box<Expr>),
    Builtin(Bi, Box<[Expr]>),
}

pub struct Update {
    pub path: Box<[PathE]>,
    /// the slot `@` reads
    pub at: u32,
    pub val: Expr,
}

pub enum PathE {
    Idx(Expr),
    Field(u32),
}

pub struct Bound {
    pub slots: Box<[u32]>,
    pub tuple: bool,
    pub set: Expr,
}

#[derive(Clone, Copy, Debug)]
pub enum Bin {
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    NotIn,
    Subseteq,
    Subset,
    Supseteq,
    Cup,
    Cap,
    SetMinus,
    Range,
    Plus,
    Minus,
    Mul,
    Div,
    Mod,
    Pow,
    Concat,
    ColonGt,
    AtAt,
    Equiv,
}

#[derive(Clone, Copy, Debug)]
pub enum Un {
    Neg,
    Subset,
    Union,
    Domain,
    /// is a function (a record, a tuple): what a function or record set's
    /// membership asks first, as TLC answers FALSE, not an error
    IsFcn,
}

#[derive(Clone, Copy, Debug)]
pub enum Bi {
    Cardinality,
    Len,
    Append,
    Head,
    Tail,
    SubSeq,
    SeqSet,
    IsFiniteSet,
    Print,
    PrintT,
    Assert,
    Permutations,
}

/// An action: evaluated for its successors, not its value.
pub enum Act {
    Guard(Expr),
    And(Box<[Act]>),
    Or(Box<[Act]>),
    Exists(Box<[Bound]>, Box<Act>),
    If(Expr, Box<Act>, Box<Act>),
    Case(Box<[(Expr, Act)]>, Option<Box<Act>>),
    Let(Box<[(u32, Expr)]>, Box<Act>),
    Lazy(Box<[(u32, u32)]>, Box<Act>),
    Assign(u32, Expr),
    AssignIn(u32, Expr),
    Unchanged(Box<[u32]>),
}

pub struct Op {
    pub name: String,
    /// leading slots that are parameters (for a lifted LET RECURSIVE:
    /// its captured locals, then its own parameters)
    pub nparams: usize,
    pub frame: u32,
    pub body: Expr,
    /// Zero-arity and constant-level: evaluated once, before checking.
    pub cached: Option<Value>,
}

/// A compiled expression with the frame size its top level needs.
pub struct Rooted<T> {
    pub name: String,
    pub frame: u32,
    pub body: T,
}

/// A temporal property, in the shapes the gates use. Leaves are state
/// predicates (or, for ActionBox, a step predicate reading primes).
pub enum TProp {
    ForAll(Box<Bound>, Box<TProp>),
    /// operator parameters bound to (constant) arguments
    Let(Box<[(u32, Expr)]>, Box<TProp>),
    And(Vec<TProp>),
    /// `P ~> Q`, `[](P => <>Q)`
    LeadsTo(Expr, Expr),
    /// `<>[]P`
    EventuallyAlways(Expr),
    /// `[]<>P`
    AlwaysEventually(Expr),
    /// `<>P`, P a state predicate: every behavior reaches P
    Eventually(Expr),
    /// `[]P`, P a state predicate
    Always(Expr),
    /// P, a state predicate: holds in every initial state
    Init(Expr),
    /// `[][A]_v`: every step satisfies A or leaves v unchanged
    ActionBox(Expr, Expr),
}

pub struct Property {
    pub name: String,
    pub frame: u32,
    pub body: TProp,
}

/// The fairness conjuncts of the spec.
pub enum FairTree {
    ForAll(Box<Bound>, Box<FairTree>),
    Let(Box<[(u32, Expr)]>, Box<FairTree>),
    And(Vec<FairTree>),
    /// WF_sub(act) or, with `strong`, SF_sub(act)
    Fair { strong: bool, sub: Expr, act: Act },
}

pub struct Fairness {
    pub frame: u32,
    pub body: FairTree,
}

pub struct Program {
    pub vars: Vec<String>,
    pub ops: Vec<Op>,
    /// bodies of LET definitions, by id
    pub lets: Vec<Expr>,
    pub init: Rooted<Act>,
    pub next: Rooted<Act>,
    pub invariants: Vec<Rooted<Expr>>,
    pub constraints: Vec<Rooted<Expr>>,
    pub check_deadlock: bool,
    /// SYMMETRY: each non-identity permutation as a map over model-value ids
    pub symmetry: Vec<Box<[u32]>>,
    /// VIEW: states are identified by this expression's value
    pub view: Option<Rooted<Expr>>,
    /// The order SYMMETRY compares variables in when picking the least
    /// image. TLC's is a Java Hashtable order over SANY's symbol table,
    /// not declaration order; `-var-order` supplies it.
    pub cmp_order: Vec<usize>,
    pub properties: Vec<Property>,
    pub fairness: Option<Fairness>,
}

pub struct Cx<'a> {
    pub state: &'a [Value],
    pub next: &'a mut Vec<Option<Value>>,
    pub memo: &'a mut Vec<Option<Value>>,
    /// the next state is complete and will not change under this context
    /// (a step property), so primed memo slots may be used
    pub next_fixed: bool,
    pub stack: &'a mut Vec<Value>,
    /// per stack slot: has a lazy LET slot been evaluated
    pub ok: &'a mut Vec<bool>,
    pub base: usize,
    pub frame: usize,
}

/// Per-worker buffers, reused across every state the worker expands.
#[derive(Default)]
pub struct Bufs {
    pub next: Vec<Option<Value>>,
    pub memo: Vec<Option<Value>>,
    pub stack: Vec<Value>,
    pub ok: Vec<bool>,
}

impl Bufs {
    pub fn cx<'a>(&'a mut self, state: &'a [Value], nvars: usize, frame: u32) -> Cx<'a> {
        self.next.clear();
        self.next.resize(nvars, None);
        self.memo.clear();
        if self.stack.len() < frame as usize {
            self.stack.resize(frame as usize, Value::Bool(false));
            self.ok.resize(frame as usize, false);
        }
        Cx { state, next: &mut self.next, memo: &mut self.memo, next_fixed: false, stack: &mut self.stack, ok: &mut self.ok, base: 0, frame: frame as usize }
    }
}

impl Cx<'_> {
    #[inline(always)]
    pub fn set(&mut self, slot: u32, v: Value) {
        self.stack[self.base + slot as usize] = v;
    }
}

/// Ends an ENABLED search at its first successor.
pub const ENABLED_STOP: &str = "\u{0}enabled";

pub type K<'k, 'a> = &'k mut dyn FnMut(&mut Cx<'a>) -> R<()>;

/// How a spec's formulas are evaluated. The checker (BFS, fingerprints,
/// symmetry, traces) is shared; the interpreter, the closure compiler and
/// generated Rust are three implementations of this.
pub trait Engine: Sync {
    fn init<'a>(&self, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()>;
    fn next<'a>(&self, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()>;
    fn invariant(&self, i: usize, cx: &mut Cx) -> R<bool>;
    fn constraint(&self, i: usize, cx: &mut Cx) -> R<bool>;
    fn view(&self, cx: &mut Cx) -> R<Value>;
}

impl Engine for Program {
    fn init<'a>(&self, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
        self.run(&self.init.body, cx, k)
    }
    fn next<'a>(&self, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
        self.run(&self.next.body, cx, k)
    }
    fn invariant(&self, i: usize, cx: &mut Cx) -> R<bool> {
        self.eval_bool(&self.invariants[i].body, cx)
    }
    fn constraint(&self, i: usize, cx: &mut Cx) -> R<bool> {
        self.eval_bool(&self.constraints[i].body, cx)
    }
    fn view(&self, cx: &mut Cx) -> R<Value> {
        self.eval(&self.view.as_ref().ok_or("no VIEW")?.body, cx)
    }
}

impl Program {
    pub fn eval_bool(&self, e: &Expr, cx: &mut Cx) -> R<bool> {
        match e {
            Expr::Not(a) => Ok(!self.eval_bool(a, cx)?),
            Expr::And(v) => {
                for a in v.iter() {
                    if !self.eval_bool(a, cx)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Expr::Or(v) => {
                for a in v.iter() {
                    if self.eval_bool(a, cx)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Expr::Implies(a, b) => Ok(!self.eval_bool(a, cx)? || self.eval_bool(b, cx)?),
            Expr::Quant(forall, bounds, body) => self.quant(*forall, bounds, 0, body, cx),
            Expr::Bin(Bin::Eq, a, b) => self.with2(a, b, cx, |x, y| Ok(x == y)),
            Expr::Bin(Bin::Neq, a, b) => self.with2(a, b, cx, |x, y| Ok(x != y)),
            Expr::Bin(Bin::In, a, b) => self.with2(a, b, cx, |x, s| s.contains(x)),
            Expr::Bin(Bin::NotIn, a, b) => self.with2(a, b, cx, |x, s| Ok(!s.contains(x)?)),
            _ => self.eval(e, cx)?.as_bool(),
        }
    }

    /// Evaluates a path into the state or into a constant by reference:
    /// reading `sess[h].tok` should not clone and drop `sess`. Returns
    /// None, having evaluated nothing, when the root is anything else.
    fn eval_ref<'a>(&'a self, e: &'a Expr, st: &'a [Value], cx: &mut Cx) -> R<Option<&'a Value>> {
        Ok(match e {
            Expr::Var(i) => Some(&st[*i as usize]),
            Expr::Const(v) => Some(v),
            Expr::Call(op, args) if args.is_empty() => self.ops[*op as usize].cached.as_ref(),
            Expr::App(f, x) => match self.eval_ref(f, st, cx)? {
                Some(fv) => {
                    let k = self.eval(x, cx)?;
                    Some(fv.apply_ref(&k)?)
                }
                None => None,
            },
            Expr::Field(r, id) => match self.eval_ref(r, st, cx)? {
                Some(rv) => Some(rv.field_ref(*id)?),
                None => None,
            },
            _ => None,
        })
    }

    /// Applies f to both operands, borrowing whichever can be borrowed.
    #[inline]
    fn with2(&self, a: &Expr, b: &Expr, cx: &mut Cx, f: impl FnOnce(&Value, &Value) -> R<bool>) -> R<bool> {
        let st = cx.state;
        match self.eval_ref(a, st, cx)? {
            Some(x) => match self.eval_ref(b, st, cx)? {
                Some(y) => f(x, y),
                None => f(x, &self.eval(b, cx)?),
            },
            None => {
                let x = self.eval(a, cx)?;
                match self.eval_ref(b, st, cx)? {
                    Some(y) => f(&x, y),
                    None => f(&x, &self.eval(b, cx)?),
                }
            }
        }
    }

    fn bind(&self, b: &Bound, x: &Value, cx: &mut Cx) -> R<()> {
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

    fn quant(&self, forall: bool, bounds: &[Bound], i: usize, body: &Expr, cx: &mut Cx) -> R<bool> {
        if i == bounds.len() {
            return self.eval_bool(body, cx);
        }
        let set = self.eval(&bounds[i].set, cx)?.elems()?;
        for x in set.iter() {
            self.bind(&bounds[i], x, cx)?;
            let r = self.quant(forall, bounds, i + 1, body, cx)?;
            if r != forall {
                return Ok(r);
            }
        }
        Ok(forall)
    }

    fn each(&self, bounds: &[Bound], i: usize, cx: &mut Cx, f: &mut dyn FnMut(&mut Cx) -> R<()>) -> R<()> {
        if i == bounds.len() {
            return f(cx);
        }
        let set = self.eval(&bounds[i].set, cx)?.elems()?;
        for x in set.iter() {
            self.bind(&bounds[i], x, cx)?;
            self.each(bounds, i + 1, cx, f)?;
        }
        Ok(())
    }

    pub fn eval(&self, e: &Expr, cx: &mut Cx) -> R<Value> {
        Ok(match e {
            Expr::Const(v) => v.clone(),
            Expr::Local(i) => cx.stack[cx.base + *i as usize].clone(),
            Expr::Var(i) => cx.state[*i as usize].clone(),
            Expr::Primed(i) => match &cx.next[*i as usize] {
                Some(v) => v.clone(),
                None => return Err(format!("{}' is read before the action assigns it", self.vars[*i as usize])),
            },
            Expr::Call(op, args) => return self.call(*op, args, cx),
            Expr::Not(_) | Expr::And(_) | Expr::Or(_) | Expr::Implies(..) | Expr::Quant(..) => {
                Value::Bool(self.eval_bool(e, cx)?)
            }
            Expr::If(c, a, b) => {
                if self.eval_bool(c, cx)? {
                    return self.eval(a, cx);
                } else {
                    return self.eval(b, cx);
                }
            }
            Expr::Case(arms, other) => {
                for (p, v) in arms.iter() {
                    if self.eval_bool(p, cx)? {
                        return self.eval(v, cx);
                    }
                }
                match other {
                    Some(o) => return self.eval(o, cx),
                    None => return Err("no CASE arm matched".into()),
                }
            }
            Expr::Let(defs, body) => {
                for (slot, d) in defs.iter() {
                    let v = self.eval(d, cx)?;
                    cx.set(*slot, v);
                }
                return self.eval(body, cx);
            }
            Expr::Lazy(slots, body) => {
                for (s, _) in slots.iter() {
                    cx.ok[cx.base + *s as usize] = false;
                }
                return self.eval(body, cx);
            }
            Expr::Memo(slot, primed, e) => {
                let i = *slot as usize;
                if *primed && !cx.next_fixed {
                    return self.eval(e, cx);
                }
                if let Some(Some(v)) = cx.memo.get(i) {
                    return Ok(v.clone());
                }
                let v = self.eval(e, cx)?;
                if cx.memo.len() <= i {
                    cx.memo.resize(i + 1, None);
                }
                cx.memo[i] = Some(v.clone());
                v
            }
            Expr::LetRef(slot, id) => {
                let i = cx.base + *slot as usize;
                if !cx.ok[i] {
                    let v = self.eval(&self.lets[*id as usize], cx)?;
                    cx.stack[i] = v;
                    cx.ok[i] = true;
                }
                cx.stack[i].clone()
            }
            Expr::Enabled(a) => {
                let n = cx.next.len();
                let saved = std::mem::replace(cx.next, vec![None; n]);
                let mut found = false;
                let r = self.run(a, cx, &mut |_| {
                    found = true;
                    Err(ENABLED_STOP.into())
                });
                *cx.next = saved;
                match r {
                    Err(e) if e != ENABLED_STOP => return Err(e),
                    _ => Value::Bool(found),
                }
            }
            Expr::SelectSeq(seq, slot, pred) => {
                let Value::Seq(xs) = self.eval(seq, cx)? else { return Err("SelectSeq of a non-sequence".into()) };
                let mut out = Vec::with_capacity(xs.len());
                for x in xs.iter() {
                    cx.set(*slot, x.clone());
                    if self.eval_bool(pred, cx)? {
                        out.push(x.clone());
                    }
                }
                Value::Seq(out.into())
            }
            Expr::Choose(b, p) => {
                let set = self.eval(&b.set, cx)?.elems()?;
                for x in set.iter() {
                    self.bind(b, x, cx)?;
                    if self.eval_bool(p, cx)? {
                        return Ok(x.clone());
                    }
                }
                return Err("CHOOSE found no element satisfying its predicate".into());
            }
            Expr::SetEnum(v) => {
                let mut out = Vec::with_capacity(v.len());
                for a in v.iter() {
                    out.push(self.eval(a, cx)?);
                }
                return Value::set(out);
            }
            Expr::SetFilter(b, p) => {
                let set = self.eval(&b.set, cx)?.elems()?;
                let mut out = Vec::new();
                for x in set.iter() {
                    self.bind(b, x, cx)?;
                    if self.eval_bool(p, cx)? {
                        out.push(x.clone());
                    }
                }
                if out.len() == set.len() {
                    Value::Set(set)
                } else {
                    Value::set_sorted(out)
                }
            }
            Expr::SetMap(bounds, body) => {
                let mut out = Vec::new();
                self.each(bounds, 0, cx, &mut |cx| {
                    out.push(self.eval(body, cx)?);
                    Ok(())
                })?;
                return Value::set(out);
            }
            Expr::FuncCons(bounds, body) => {
                let mut pairs = Vec::new();
                if bounds.len() == 1 && !bounds[0].tuple {
                    let set = self.eval(&bounds[0].set, cx)?.elems()?;
                    pairs.reserve(set.len());
                    for x in set.iter() {
                        cx.set(bounds[0].slots[0], x.clone());
                        pairs.push((x.clone(), self.eval(body, cx)?.normalized()?));
                    }
                    return Ok(Value::func_sorted(pairs));
                }
                self.each(bounds, 0, cx, &mut |cx| {
                    let key: Vec<Value> = bounds
                        .iter()
                        .flat_map(|b| b.slots.iter())
                        .map(|s| cx.stack[cx.base + *s as usize].clone())
                        .collect();
                    let key = if bounds.len() == 1 && bounds[0].tuple || key.len() > 1 {
                        Value::Seq(key.into())
                    } else {
                        key.into_iter().next().unwrap()
                    };
                    pairs.push((key, self.eval(body, cx)?));
                    Ok(())
                })?;
                return Value::func(pairs);
            }
            Expr::Record(fields) => {
                let mut pairs = Vec::with_capacity(fields.len());
                for (f, a) in fields.iter() {
                    pairs.push((Value::Str(*f), self.eval(a, cx)?.normalized()?));
                }
                Value::func_sorted(pairs)
            }
            Expr::RecSet(fields) => {
                let mut fs = Vec::with_capacity(fields.len());
                for (f, a) in fields.iter() {
                    fs.push((*f, self.eval(a, cx)?));
                }
                Value::Lazy(Arc::new(Lazy::RecSet(fs.into())))
            }
            Expr::FuncSet(d, r) => Value::Lazy(Arc::new(Lazy::FuncSet(self.eval(d, cx)?, self.eval(r, cx)?))),
            Expr::Product(v) => {
                let mut sets = Vec::with_capacity(v.len());
                for a in v.iter() {
                    sets.push(self.eval(a, cx)?);
                }
                Value::Lazy(Arc::new(Lazy::Product(sets.into())))
            }
            Expr::Tuple(v) => {
                let mut out = Vec::with_capacity(v.len());
                for a in v.iter() {
                    out.push(self.eval(a, cx)?.normalized()?);
                }
                Value::Seq(out.into())
            }
            Expr::App(f, x) => {
                let st = cx.state;
                if let Some(v) = self.eval_ref(e, st, cx)? {
                    return Ok(v.clone());
                }
                let fv = self.eval(f, cx)?;
                let xv = self.eval(x, cx)?;
                return fv.apply(&xv);
            }
            Expr::Field(r, id) => {
                let st = cx.state;
                if let Some(v) = self.eval_ref(e, st, cx)? {
                    return Ok(v.clone());
                }
                return self.eval(r, cx)?.field(*id);
            }
            Expr::Except(f, ups) => {
                let mut v = self.eval(f, cx)?;
                for u in ups.iter() {
                    v = self.except_path(v, &u.path, u.at, &u.val, cx)?;
                }
                v
            }
            Expr::Bin(op, a, b) => {
                if matches!(op, Bin::Eq | Bin::Neq | Bin::In | Bin::NotIn | Bin::Equiv) {
                    if let Bin::Equiv = op {
                        return Ok(Value::Bool(self.eval_bool(a, cx)? == self.eval_bool(b, cx)?));
                    }
                    return Ok(Value::Bool(self.eval_bool(e, cx)?));
                }
                let x = self.eval(a, cx)?;
                let y = self.eval(b, cx)?;
                return binop(*op, x, y);
            }
            Expr::Un(op, a) => {
                let x = self.eval(a, cx)?;
                match op {
                    Un::Neg => Value::Int(-x.as_int()?),
                    Un::Subset => Value::Lazy(Arc::new(Lazy::Subset(x))),
                    Un::Domain => return x.domain(),
                    Un::IsFcn => Value::Bool(x.domain().is_ok()),
                    Un::Union => {
                        let mut out = Vec::new();
                        for s in x.elems()?.iter() {
                            out.extend(s.elems()?.iter().cloned());
                        }
                        return Value::set(out);
                    }
                }
            }
            Expr::Builtin(bi, args) => return self.builtin(*bi, args, cx),
        })
    }

    fn call(&self, op: u32, args: &[Expr], cx: &mut Cx) -> R<Value> {
        let o = &self.ops[op as usize];
        if let Some(v) = &o.cached {
            return Ok(v.clone());
        }
        let (base, frame) = (cx.base, cx.frame);
        let nb = base + frame;
        let need = nb + (o.frame as usize).max(args.len());
        if cx.stack.len() < need {
            cx.stack.resize(need, Value::Bool(false));
            cx.ok.resize(need, false);
        }
        // Nested calls inside the arguments open their frames above the
        // arguments already placed.
        for (i, a) in args.iter().enumerate() {
            cx.frame = frame + i;
            let v = self.eval(a, cx)?;
            cx.stack[nb + i] = v;
        }
        cx.base = nb;
        cx.frame = o.frame as usize;
        let r = self.eval(&o.body, cx);
        cx.base = base;
        cx.frame = frame;
        r
    }

    fn except_path(&self, f: Value, path: &[PathE], at: u32, val: &Expr, cx: &mut Cx) -> R<Value> {
        let key = match &path[0] {
            PathE::Idx(e) => self.eval(e, cx)?,
            PathE::Field(id) => Value::Str(*id),
        };
        let Ok(old) = f.apply(&key) else { return Ok(f) };
        let new = if path.len() == 1 {
            cx.set(at, old);
            self.eval(val, cx)?.normalized()?
        } else {
            self.except_path(old, &path[1..], at, val, cx)?
        };
        f.except(&key, new)
    }

    fn builtin(&self, bi: Bi, args: &[Expr], cx: &mut Cx) -> R<Value> {
        let mut v = Vec::with_capacity(args.len());
        for a in args {
            v.push(self.eval(a, cx)?);
        }
        builtin_apply(bi, v)
    }

    // ---- actions --------------------------------------------------------

    pub fn run<'a>(&self, a: &Act, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
        match a {
            Act::Guard(e) => {
                if self.eval_bool(e, cx)? {
                    k(cx)?;
                }
                Ok(())
            }
            Act::And(v) => self.run_and(v, cx, k),
            Act::Or(v) => {
                for b in v.iter() {
                    self.run(b, cx, k)?;
                }
                Ok(())
            }
            Act::Exists(bounds, body) => self.run_exists(bounds, 0, body, cx, k),
            Act::If(c, t, e) => {
                if self.eval_bool(c, cx)? {
                    self.run(t, cx, k)
                } else {
                    self.run(e, cx, k)
                }
            }
            Act::Case(arms, other) => {
                for (p, b) in arms.iter() {
                    if self.eval_bool(p, cx)? {
                        return self.run(b, cx, k);
                    }
                }
                match other {
                    Some(o) => self.run(o, cx, k),
                    None => Err("no CASE arm matched".into()),
                }
            }
            Act::Let(defs, body) => {
                for (slot, d) in defs.iter() {
                    let v = self.eval(d, cx)?;
                    cx.set(*slot, v);
                }
                self.run(body, cx, k)
            }
            Act::Lazy(slots, body) => {
                for (s, _) in slots.iter() {
                    cx.ok[cx.base + *s as usize] = false;
                }
                self.run(body, cx, k)
            }
            Act::Assign(var, e) => {
                let v = self.eval(e, cx)?.normalized()?;
                self.assign(*var, v, cx, k)
            }
            Act::AssignIn(var, s) => {
                let set = self.eval(s, cx)?.elems()?;
                for x in set.iter() {
                    self.assign(*var, x.clone(), cx, k)?;
                }
                Ok(())
            }
            Act::Unchanged(vars) => {
                let mut set_here: u128 = 0;
                let mut ok = true;
                for (i, &var) in vars.iter().enumerate() {
                    let cur = &cx.state[var as usize];
                    match &cx.next[var as usize] {
                        None => {
                            cx.next[var as usize] = Some(cur.clone());
                            set_here |= 1 << i;
                        }
                        Some(v) => {
                            if v != cur {
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
            }
        }
    }

    #[inline]
    fn assign<'a>(&self, var: u32, v: Value, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
        let slot = var as usize;
        match &cx.next[slot] {
            None => {
                cx.next[slot] = Some(v);
                let r = k(cx);
                cx.next[slot] = None;
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

    fn run_and<'a>(&self, v: &[Act], cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
        match v {
            [] => k(cx),
            [last] => self.run(last, cx, k),
            [first, rest @ ..] => self.run(first, cx, &mut |cx| self.run_and(rest, cx, k)),
        }
    }

    fn run_exists<'a>(&self, bounds: &[Bound], i: usize, body: &Act, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {
        if i == bounds.len() {
            return self.run(body, cx, k);
        }
        let set = self.eval(&bounds[i].set, cx)?.elems()?;
        for x in set.iter() {
            self.bind(&bounds[i], x, cx)?;
            self.run_exists(bounds, i + 1, body, cx, k)?;
        }
        Ok(())
    }
}

/// A standard-module operator applied to evaluated arguments.
pub fn builtin_apply(bi: Bi, mut v: Vec<Value>) -> R<Value> {
        let seq = |x: &Value| -> R<Arc<[Value]>> {
        match x {
            Value::Seq(s) => Ok(s.clone()),
            _ => Err(format!("expected a sequence, got {x}")),
        }
    };
    Ok(match bi {
        Bi::Cardinality => Value::Int(v[0].elems()?.len() as i64),
        Bi::Len => Value::Int(seq(&v[0])?.len() as i64),
        Bi::Append => {
            let mut s = seq(&v[0])?.to_vec();
            s.push(v.pop().unwrap().normalized()?);
            Value::Seq(s.into())
        }
        Bi::Head => seq(&v[0])?.first().cloned().ok_or("Head of <<>>")?,
        Bi::Tail => {
            let s = seq(&v[0])?;
            if s.is_empty() {
                return Err("Tail of <<>>".into());
            }
            Value::Seq(s[1..].into())
        }
        Bi::SubSeq => {
            let s = seq(&v[0])?;
            let (m, n) = (v[1].as_int()?, v[2].as_int()?);
            if m > n {
                Value::Seq(Arc::from([]))
            } else {
                Value::Seq(s[(m - 1) as usize..n as usize].into())
            }
        }
        Bi::SeqSet => Value::Lazy(Arc::new(Lazy::SeqOf(v.pop().unwrap()))),
        Bi::IsFiniteSet => Value::Bool(v[0].elems().is_ok()),
        Bi::Print => {
            println!("{}  {}", v[0], v[1]);
            v.pop().unwrap()
        }
        Bi::PrintT => {
            println!("{}", v[0]);
            Value::Bool(true)
        }
        Bi::Permutations => {
            let xs = v[0].elems()?;
            if xs.len() > 10 {
                return Err("Permutations of a set larger than 10 elements".into());
            }
            let mut out = Vec::new();
            let mut img: Vec<Value> = xs.to_vec();
            permutations(&mut img, 0, &mut |img| {
                out.push(Value::func_sorted(xs.iter().cloned().zip(img.iter().cloned()).collect()));
            });
            Value::set(out)?
        }
        Bi::Assert => {
            if !v[0].as_bool()? {
                return Err(format!("Assert failed: {}", v[1]));
            }
            Value::Bool(true)
        }
    })
    }

fn set_merge(x: &[Value], y: &[Value], keep_x_only: bool, keep_both: bool, keep_y_only: bool) -> Value {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(x.len() + y.len());
    while i < x.len() && j < y.len() {
        match x[i].cmp(&y[j]) {
            std::cmp::Ordering::Less => {
                if keep_x_only {
                    out.push(x[i].clone());
                }
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                if keep_y_only {
                    out.push(y[j].clone());
                }
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if keep_both {
                    out.push(x[i].clone());
                }
                i += 1;
                j += 1;
            }
        }
    }
    if keep_x_only {
        out.extend_from_slice(&x[i..]);
    }
    if keep_y_only {
        out.extend_from_slice(&y[j..]);
    }
    Value::set_sorted(out)
}

pub fn binop(op: Bin, x: Value, y: Value) -> R<Value> {
    Ok(match op {
        Bin::Lt => Value::Bool(x.as_int()? < y.as_int()?),
        Bin::Le => Value::Bool(x.as_int()? <= y.as_int()?),
        Bin::Gt => Value::Bool(x.as_int()? > y.as_int()?),
        Bin::Ge => Value::Bool(x.as_int()? >= y.as_int()?),
        Bin::Plus => Value::Int(x.as_int()? + y.as_int()?),
        Bin::Minus => Value::Int(x.as_int()? - y.as_int()?),
        Bin::Mul => Value::Int(x.as_int()? * y.as_int()?),
        Bin::Div => {
            let d = y.as_int()?;
            if d == 0 {
                return Err("division by zero".into());
            }
            Value::Int(x.as_int()?.div_euclid(d))
        }
        Bin::Mod => {
            let d = y.as_int()?;
            if d <= 0 {
                return Err("% by a non-positive number".into());
            }
            Value::Int(x.as_int()?.rem_euclid(d))
        }
        Bin::Pow => Value::Int(x.as_int()?.pow(y.as_int()? as u32)),
        Bin::Range => {
            let (a, b) = (x.as_int()?, y.as_int()?);
            Value::set_sorted((a..=b).map(Value::Int).collect())
        }
        Bin::Cup => {
            let (a, b) = (x.elems()?, y.elems()?);
            if b.is_empty() {
                Value::Set(a)
            } else if a.is_empty() {
                Value::Set(b)
            } else {
                set_merge(&a, &b, true, true, true)
            }
        }
        Bin::Cap => set_merge(&x.elems()?, &y.elems()?, false, true, false),
        Bin::SetMinus => {
            let a = x.elems()?;
            if let Value::Lazy(_) = y {
                let mut out = Vec::new();
                for e in a.iter() {
                    if !y.contains(e)? {
                        out.push(e.clone());
                    }
                }
                return Ok(Value::set_sorted(out));
            }
            let b = y.elems()?;
            if b.is_empty() { Value::Set(a) } else { set_merge(&a, &b, true, false, false) }
        }
        Bin::Subseteq | Bin::Subset => {
            let a = x.elems()?;
            for e in a.iter() {
                if !y.contains(e)? {
                    return Ok(Value::Bool(false));
                }
            }
            let strict = matches!(op, Bin::Subset);
            Value::Bool(!strict || a.len() != y.elems()?.len())
        }
        Bin::Supseteq => return binop(Bin::Subseteq, y, x),
        Bin::Concat => match (&x, &y) {
            (Value::Seq(a), Value::Seq(b)) => Value::Seq(a.iter().chain(b.iter()).cloned().collect()),
            _ => return Err(format!("\\o on non-sequences {x}, {y}")),
        },
        Bin::ColonGt => Value::func_sorted(vec![(x.normalized()?, y.normalized()?)]),
        Bin::AtAt => {
            let mut p = x.pairs()?;
            p.extend(y.pairs()?);
            Value::func(p)?
        }
        Bin::Eq | Bin::Neq | Bin::In | Bin::NotIn | Bin::Equiv => unreachable!(),
    })
}

fn permutations(v: &mut Vec<Value>, i: usize, f: &mut dyn FnMut(&[Value])) {
    if i == v.len() {
        f(v);
        return;
    }
    for j in i..v.len() {
        v.swap(i, j);
        permutations(v, i + 1, f);
        v.swap(i, j);
    }
}

pub fn boolean_set() -> Value {
    Value::set_sorted(vec![Value::Bool(false), Value::Bool(true)])
}
