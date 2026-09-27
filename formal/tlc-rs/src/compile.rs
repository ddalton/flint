//! AST -> IR. Resolves every name once, splits action-level syntax (which
//! is enumerated for successors) from state-level syntax (which is only
//! evaluated), and inlines action operators at their call sites.

use crate::ast::*;
use crate::eval::*;
use crate::parser::{Cfg, CfgVal};
use crate::value::{Lazy, Value, R};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone)]
enum Entry {
    Local(String, u32),
    /// a zero-arity LET definition: its slot and its body's id
    Lazy(String, u32, u32),
    /// a LET operator with parameters, and the scope length it closes over
    LetOp(Rc<Def>, usize),
    /// a recursive LET operator, lifted to its own frame: the op, and the
    /// enclosing locals it reads, passed ahead of its own arguments
    Lifted(Rc<Def>, u32, Vec<Capture>),
}

#[derive(Clone)]
struct Capture {
    name: String,
    slot: u32,
    lazy: Option<u32>,
}

impl Capture {
    fn read(&self) -> Expr {
        match self.lazy {
            Some(id) => Expr::LetRef(self.slot, id),
            None => Expr::Local(self.slot),
        }
    }
}

pub struct Compiler {
    defs: HashMap<String, Rc<Def>>,
    vars: HashMap<String, u32>,
    var_names: Vec<String>,
    consts: HashMap<String, Value>,
    subst: HashMap<String, String>,
    names: HashMap<String, u32>,
    name_list: Vec<String>,
    op_idx: HashMap<String, u32>,
    ops: Vec<Option<Op>>,
    pending: Vec<u32>,
    memo: HashMap<(String, bool), bool>,
    temporal_memo: HashMap<String, bool>,
    next_slot: u32,
    inline_depth: u32,
    lets: Vec<Expr>,
}

fn is_builtin_value(n: &str) -> Option<Value> {
    Some(match n {
        "Nat" => Value::Lazy(Arc::new(Lazy::Nat)),
        "Int" => Value::Lazy(Arc::new(Lazy::Int)),
        "STRING" => Value::Lazy(Arc::new(Lazy::Strings)),
        "BOOLEAN" => boolean_set(),
        _ => return None,
    })
}

fn builtin_fn(n: &str) -> Option<(Bi, usize)> {
    Some(match n {
        "Cardinality" => (Bi::Cardinality, 1),
        "Len" => (Bi::Len, 1),
        "Append" => (Bi::Append, 2),
        "Head" => (Bi::Head, 1),
        "Tail" => (Bi::Tail, 1),
        "SubSeq" => (Bi::SubSeq, 3),
        "Seq" => (Bi::SeqSet, 1),
        "IsFiniteSet" => (Bi::IsFiniteSet, 1),
        "Print" => (Bi::Print, 2),
        "PrintT" => (Bi::PrintT, 1),
        "Assert" => (Bi::Assert, 2),
        "Permutations" => (Bi::Permutations, 1),
        _ => return None,
    })
}

fn bin(op: &str) -> Option<Bin> {
    Some(match op {
        "=" => Bin::Eq,
        "#" => Bin::Neq,
        "<" => Bin::Lt,
        "<=" => Bin::Le,
        ">" => Bin::Gt,
        ">=" => Bin::Ge,
        "\\in" => Bin::In,
        "\\notin" => Bin::NotIn,
        "\\subseteq" => Bin::Subseteq,
        "\\subset" => Bin::Subset,
        "\\supseteq" => Bin::Supseteq,
        "\\cup" => Bin::Cup,
        "\\cap" => Bin::Cap,
        "\\" => Bin::SetMinus,
        ".." => Bin::Range,
        "+" => Bin::Plus,
        "-" => Bin::Minus,
        "*" => Bin::Mul,
        "\\div" => Bin::Div,
        "%" => Bin::Mod,
        "^" => Bin::Pow,
        "\\o" => Bin::Concat,
        ":>" => Bin::ColonGt,
        "@@" => Bin::AtAt,
        "<=>" => Bin::Equiv,
        _ => return None,
    })
}

/// Every direct sub-expression, including LET definition bodies.
fn children(a: &Ast) -> Vec<&Ast> {
    use Ast::*;
    match a {
        Num(_) | Str(_) | Bool(_) | Ident(_) | At => vec![],
        Lambda(_, x) => vec![x],
        Apply(_, v) | And(v) | Or(v) | Product(v) | SetEnum(v) | Tuple(v) | Temporal(_, v) => v.iter().collect(),
        Prime(x) | Unchanged(x) | Not(x) | Neg(x) | Prefix(_, x) | Field(x, _) => vec![x],
        Bin(_, x, y) | FuncSet(x, y) | BoxAction(x, y) => vec![x, y],
        If(c, x, y) => vec![c, x, y],
        Case(arms, o) => {
            let mut v: Vec<&Ast> = arms.iter().flat_map(|(p, e)| [p, e]).collect();
            v.extend(o.as_deref());
            v
        }
        Let(defs, body) => {
            let mut v: Vec<&Ast> = defs.iter().map(|d| &d.body).collect();
            v.push(body);
            v
        }
        Quant(_, bs, body) | FuncCons(bs, body) => {
            let mut v: Vec<&Ast> = bs.iter().map(|b| &b.set).collect();
            v.push(body);
            v
        }
        SetMap(body, bs) => {
            let mut v: Vec<&Ast> = bs.iter().map(|b| &b.set).collect();
            v.push(body);
            v
        }
        Choose(b, body) | SetFilter(b, body) => vec![&b.set, body],
        Record(fs) | RecordSet(fs) => fs.iter().map(|(_, e)| e).collect(),
        Except(f, ups) => {
            let mut v = vec![&**f];
            for (path, e) in ups {
                for p in path {
                    if let PathEl::Idx(ix) = p {
                        v.extend(ix.iter());
                    }
                }
                v.push(e);
            }
            v
        }
        App(f, args) => {
            let mut v = vec![&**f];
            v.extend(args.iter());
            v
        }
    }
}

impl Compiler {
    /// `intern_order`: names in the order TLC interns them, so interned ids
    /// order strings and model values as TLC does.
    pub fn new(modules: &[Module], cfg: &Cfg, intern_order: &[String]) -> R<Compiler> {
        let mut c = Compiler {
            defs: HashMap::new(),
            vars: HashMap::new(),
            var_names: vec![],
            consts: HashMap::new(),
            subst: cfg.substitutions.iter().cloned().collect(),
            names: HashMap::new(),
            name_list: vec![],
            op_idx: HashMap::new(),
            ops: vec![],
            pending: vec![],
            memo: HashMap::new(),
            temporal_memo: HashMap::new(),
            next_slot: 0,
            inline_depth: 0,
            lets: vec![],
        };
        for n in intern_order {
            c.intern(n);
        }
        let mut declared = Vec::new();
        for m in modules {
            for v in &m.variables {
                c.vars.insert(v.clone(), c.var_names.len() as u32);
                c.var_names.push(v.clone());
            }
            for d in &m.defs {
                c.defs.insert(d.name.clone(), d.clone());
            }
            declared.extend(m.constants.iter().cloned());
        }
        for (name, v) in &cfg.constants {
            let val = c.cfg_value(v)?;
            c.consts.insert(name.clone(), val);
        }
        for d in &declared {
            if !c.consts.contains_key(d) && !c.subst.contains_key(d) {
                return Err(format!("constant {d} has no value in the cfg"));
            }
        }
        Ok(c)
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.names.get(s) {
            return i;
        }
        let i = self.name_list.len() as u32;
        self.names.insert(s.to_string(), i);
        self.name_list.push(s.to_string());
        i
    }

    fn cfg_value(&mut self, v: &CfgVal) -> R<Value> {
        Ok(match v {
            CfgVal::Int(n) => Value::Int(*n),
            CfgVal::Bool(b) => Value::Bool(*b),
            CfgVal::Str(s) => Value::Str(self.intern(s)),
            CfgVal::Model(m) => Value::Model(self.intern(m)),
            CfgVal::Set(xs) => Value::set(xs.iter().map(|x| self.cfg_value(x)).collect::<R<_>>()?)?,
            CfgVal::Tuple(xs) => Value::Seq(xs.iter().map(|x| self.cfg_value(x)).collect::<R<Vec<_>>>()?.into()),
        })
    }

    fn slot(&mut self) -> u32 {
        let s = self.next_slot;
        self.next_slot += 1;
        s
    }

    fn op_index(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.op_idx.get(name) {
            return i;
        }
        let i = self.ops.len() as u32;
        self.op_idx.insert(name.to_string(), i);
        self.ops.push(None);
        self.pending.push(i);
        i
    }

    /// Compile a standalone expression: a new frame starting at slot 0.
    pub fn rooted_expr(&mut self, name: &str) -> R<Rooted<Expr>> {
        self.next_slot = 0;
        let body = self.expr(&Ast::Ident(name.to_string()), &mut vec![], false)?;
        Ok(Rooted { name: name.to_string(), frame: self.next_slot, body })
    }

    pub fn rooted_act(&mut self, name: &str, a: &Ast, init: bool) -> R<Rooted<Act>> {
        self.next_slot = 0;
        let body = self.act(a, &mut vec![], init)?;
        Ok(Rooted { name: name.to_string(), frame: self.next_slot, body })
    }

    fn drain_pending(&mut self) -> R<()> {
        while let Some(i) = self.pending.pop() {
            let name = self.op_idx.iter().find(|(_, v)| **v == i).unwrap().0.clone();
            let d = self.defs[&name].clone();
            let saved = self.next_slot;
            self.next_slot = 0;
            let mut sc: Vec<Entry> = d.params.iter().map(|p| Entry::Local(p.clone(), self.slot())).collect();
            let body = self.expr(&d.body, &mut sc, false).map_err(|e| format!("in {name}: {e}"))?;
            self.ops[i as usize] = Some(Op { name, nparams: d.params.len(), frame: self.next_slot, body, cached: None });
            self.next_slot = saved;
        }
        Ok(())
    }

    // ---- classification -------------------------------------------------

    /// Does `a` mention a primed variable (next mode) or an unprimed state
    /// variable (init mode), directly or through an operator it calls?
    fn is_action(&mut self, a: &Ast, init: bool) -> bool {
        match a {
            Ast::Prime(_) | Ast::Unchanged(_) => !init,
            Ast::Ident(n) if init && self.vars.contains_key(n) => true,
            Ast::Ident(n) | Ast::Apply(n, _) => {
                if let Ast::Apply(_, args) = a {
                    if args.iter().any(|x| self.is_action(x, init)) {
                        return true;
                    }
                }
                self.op_is_action(n, init)
            }
            _ => children(a).into_iter().any(|x| self.is_action(x, init)),
        }
    }

    fn op_is_action(&mut self, n: &str, init: bool) -> bool {
        let Some(d) = self.defs.get(n).cloned() else { return false };
        let key = (n.to_string(), init);
        if let Some(&b) = self.memo.get(&key) {
            return b;
        }
        self.memo.insert(key.clone(), false);
        let b = self.is_action(&d.body, init);
        self.memo.insert(key, b);
        b
    }

    fn is_temporal(&mut self, a: &Ast) -> bool {
        match a {
            Ast::Temporal(..) | Ast::BoxAction(..) => true,
            Ast::Ident(n) | Ast::Apply(n, _) => {
                let Some(d) = self.defs.get(n).cloned() else { return false };
                if let Some(&b) = self.temporal_memo.get(n) {
                    return b;
                }
                self.temporal_memo.insert(n.clone(), false);
                let b = self.is_temporal(&d.body);
                self.temporal_memo.insert(n.clone(), b);
                b
            }
            _ => children(a).into_iter().any(|x| self.is_temporal(x)),
        }
    }

    /// `Spec == Init /\ [][Next]_vars /\ Fairness` -> (Init, Next).
    /// `Spec == Init /\ [][Next]_vars /\ Fairness` -> (Init, Next,
    /// the remaining temporal conjuncts: fairness).
    pub fn spec_parts(&mut self, spec: &str) -> R<(Ast, Ast)> {
        let (i, n, _) = self.spec_parts_fair(spec)?;
        Ok((i, n))
    }

    pub fn spec_parts_fair(&mut self, spec: &str) -> R<(Ast, Ast, Vec<Ast>)> {
        let d = self.defs.get(spec).cloned().ok_or(format!("no definition {spec}"))?;
        // `SpecLive == Spec /\ Fairness`: expand any conjunct that is an
        // operator whose own conjuncts contain the [][Next]_v.
        fn flatten(c: &Compiler, a: &Ast, out: &mut Vec<Ast>, depth: u32) {
            match a {
                Ast::And(v) => v.iter().for_each(|x| flatten(c, x, out, depth)),
                Ast::Ident(n) if depth < 16 && c.defs.get(n).is_some_and(|d| d.params.is_empty()) => {
                    let mut inner = Vec::new();
                    flatten(c, &c.defs[n].body, &mut inner, depth + 1);
                    let has_next = inner.iter().any(|x| matches!(x, Ast::Temporal("[]", v) if matches!(v.as_slice(), [Ast::BoxAction(..)])));
                    if has_next { out.extend(inner) } else { out.push(a.clone()) }
                }
                _ => out.push(a.clone()),
            }
        }
        let mut conj = Vec::new();
        flatten(self, &d.body, &mut conj, 0);
        let (mut init, mut next, mut fair) = (None, None, Vec::new());
        for c in conj {
            match &c {
                Ast::Temporal("[]", v) if matches!(v.as_slice(), [Ast::BoxAction(..)]) => {
                    let Ast::BoxAction(a, _) = &v[0] else { unreachable!() };
                    next = Some((**a).clone());
                }
                _ if self.is_temporal(&c) => fair.push(c.clone()),
                _ => init = Some(c),
            }
        }
        Ok((init.ok_or("no Init conjunct in the spec")?, next.ok_or("no [][Next]_v conjunct in the spec")?, fair))
    }

    // ---- temporal properties and fairness -------------------------------

    pub fn rooted_property(&mut self, name: &str) -> R<Property> {
        self.next_slot = 0;
        let body = self.temporal(&Ast::Ident(name.to_string()), &mut vec![]).map_err(|e| format!("PROPERTY {name}: {e}"))?;
        Ok(Property { name: name.to_string(), frame: self.next_slot, body })
    }

    pub fn rooted_fairness(&mut self, conj: &[Ast]) -> R<Option<Fairness>> {
        if conj.is_empty() {
            return Ok(None);
        }
        self.next_slot = 0;
        let mut v = Vec::new();
        for c in conj {
            v.push(self.fair_tree(c, &mut vec![]).map_err(|e| format!("fairness: {e}"))?);
        }
        Ok(Some(Fairness { frame: self.next_slot, body: FairTree::And(v) }))
    }

    /// Binds an operator's parameters to its arguments, for inlining a
    /// temporal operator: the parameters become slots set before checking.
    fn bind_params(&mut self, d: &Rc<Def>, args: &[Ast], sc: &mut Vec<Entry>) -> R<(Box<[(u32, Expr)]>, Vec<Entry>)> {
        if d.params.len() != args.len() {
            return Err(format!("{} takes {} arguments", d.name, d.params.len()));
        }
        let mut binds = Vec::new();
        let mut inner = Vec::new();
        for (p, a) in d.params.iter().zip(args) {
            let e = self.expr(a, sc, false)?;
            let s = self.slot();
            binds.push((s, e));
            inner.push(Entry::Local(p.clone(), s));
        }
        Ok((binds.into(), inner))
    }

    fn temporal(&mut self, a: &Ast, sc: &mut Vec<Entry>) -> R<TProp> {
        match a {
            Ast::And(v) => Ok(TProp::And(v.iter().map(|x| self.temporal(x, sc)).collect::<R<_>>()?)),
            Ast::Quant(true, bounds, body) => {
                let n = sc.len();
                let bs = self.bounds(bounds, sc, false)?;
                let body = self.temporal(body, sc);
                sc.truncate(n);
                let mut t = body?;
                for b in bs.into_vec().into_iter().rev() {
                    t = TProp::ForAll(Box::new(b), Box::new(t));
                }
                Ok(t)
            }
            Ast::Temporal("~>", v) => Ok(TProp::LeadsTo(self.expr(&v[0], sc, false)?, self.expr(&v[1], sc, false)?)),
            Ast::Temporal("[]", v) => match &v[0] {
                Ast::Temporal("<>", w) if !self.is_temporal(&w[0]) => Ok(TProp::AlwaysEventually(self.expr(&w[0], sc, false)?)),
                Ast::Bin("=>", p, q) if matches!(&**q, Ast::Temporal("<>", w) if !self.is_temporal(&w[0])) => {
                    let Ast::Temporal(_, w) = &**q else { unreachable!() };
                    Ok(TProp::LeadsTo(self.expr(p, sc, false)?, self.expr(&w[0], sc, false)?))
                }
                Ast::BoxAction(act, sub) => Ok(TProp::ActionBox(self.expr(act, sc, false)?, self.expr(sub, sc, false)?)),
                x if !self.is_temporal(x) => Ok(TProp::Always(self.expr(x, sc, false)?)),
                _ => Err("unsupported shape under []".into()),
            },
            Ast::Temporal("<>", v) => match &v[0] {
                Ast::Temporal("[]", w) if !self.is_temporal(&w[0]) => Ok(TProp::EventuallyAlways(self.expr(&w[0], sc, false)?)),
                _ => Err("unsupported shape under <>".into()),
            },
            Ast::Ident(n) | Ast::Apply(n, _) if Self::lookup(sc, n).is_none() && self.defs.contains_key(n) => {
                let args: &[Ast] = if let Ast::Apply(_, a) = a { a } else { &[] };
                let d = self.defs[n].clone();
                let (binds, mut inner) = self.bind_params(&d, args, sc)?;
                let body = self.temporal(&d.body, &mut inner)?;
                Ok(if binds.is_empty() { body } else { TProp::Let(binds, Box::new(body)) })
            }
            _ => Err(format!("unsupported temporal formula {a:?}")),
        }
    }

    fn fair_tree(&mut self, a: &Ast, sc: &mut Vec<Entry>) -> R<FairTree> {
        match a {
            Ast::And(v) => Ok(FairTree::And(v.iter().map(|x| self.fair_tree(x, sc)).collect::<R<_>>()?)),
            Ast::Quant(true, bounds, body) => {
                let n = sc.len();
                let bs = self.bounds(bounds, sc, false)?;
                let body = self.fair_tree(body, sc);
                sc.truncate(n);
                let mut t = body?;
                for b in bs.into_vec().into_iter().rev() {
                    t = FairTree::ForAll(Box::new(b), Box::new(t));
                }
                Ok(t)
            }
            Ast::Temporal(k @ ("WF" | "SF"), v) => {
                let sub = self.expr(&v[0], sc, false)?;
                let act = self.act(&v[1], sc, false)?;
                Ok(FairTree::Fair { strong: *k == "SF", sub, act })
            }
            Ast::Ident(n) | Ast::Apply(n, _) if Self::lookup(sc, n).is_none() && self.defs.contains_key(n) => {
                let args: &[Ast] = if let Ast::Apply(_, a) = a { a } else { &[] };
                let d = self.defs[n].clone();
                let (binds, mut inner) = self.bind_params(&d, args, sc)?;
                let body = self.fair_tree(&d.body, &mut inner)?;
                Ok(if binds.is_empty() { body } else { FairTree::Let(binds, Box::new(body)) })
            }
            _ => Err(format!("unsupported fairness conjunct {a:?}")),
        }
    }

    // ---- lookup ---------------------------------------------------------

    fn lookup<'s>(sc: &'s [Entry], n: &str) -> Option<&'s Entry> {
        sc.iter().rev().find(|e| match e {
            Entry::Local(x, _) | Entry::Lazy(x, _, _) => x == n,
            Entry::LetOp(d, _) | Entry::Lifted(d, _, _) => d.name == n,
        })
    }

    /// Inline an operator body: parameters become fresh slots bound to the
    /// arguments, and the body sees only its own lexical scope.
    fn inline<T>(
        &mut self,
        d: &Rc<Def>,
        outer: &[Entry],
        args: &[Ast],
        sc: &mut Vec<Entry>,
        init: bool,
        f: fn(&mut Self, &Ast, &mut Vec<Entry>, bool) -> R<T>,
    ) -> R<(Box<[(u32, Expr)]>, T)> {
        if d.params.len() != args.len() {
            return Err(format!("{} takes {} arguments", d.name, d.params.len()));
        }
        self.inline_depth += 1;
        if self.inline_depth > 64 {
            return Err(format!("{} recurses; only top-level RECURSIVE operators are supported", d.name));
        }
        let mut binds = Vec::new();
        let mut inner = outer.to_vec();
        for (p, a) in d.params.iter().zip(args) {
            let e = self.expr(a, sc, init)?;
            let s = self.slot();
            binds.push((s, e));
            inner.push(Entry::Local(p.clone(), s));
        }
        let body = f(self, &d.body, &mut inner, init);
        self.inline_depth -= 1;
        Ok((binds.into(), body?))
    }

    // ---- actions --------------------------------------------------------

    fn var_of(&self, a: &Ast, sc: &[Entry], init: bool) -> Option<u32> {
        let n = match (a, init) {
            (Ast::Prime(x), false) => match &**x {
                Ast::Ident(n) => n,
                _ => return None,
            },
            (Ast::Ident(n), true) => n,
            _ => return None,
        };
        if Self::lookup(sc, n).is_some() {
            return None;
        }
        self.vars.get(n).copied()
    }

    fn unchanged_vars(&self, a: &Ast, out: &mut Vec<u32>) -> R<()> {
        match a {
            Ast::Ident(n) if self.vars.contains_key(n) => out.push(self.vars[n]),
            Ast::Ident(n) if self.defs.contains_key(n) => self.unchanged_vars(&self.defs[n].body.clone(), out)?,
            Ast::Tuple(v) => {
                for x in v {
                    self.unchanged_vars(x, out)?;
                }
            }
            _ => return Err(format!("UNCHANGED of an unsupported expression {a:?}")),
        }
        Ok(())
    }

    fn act(&mut self, a: &Ast, sc: &mut Vec<Entry>, init: bool) -> R<Act> {
        if !self.is_action(a, init) {
            return Ok(Act::Guard(self.expr(a, sc, init)?));
        }
        Ok(match a {
            Ast::And(v) => Act::And(v.iter().map(|x| self.act(x, sc, init)).collect::<R<_>>()?),
            Ast::Or(v) => Act::Or(v.iter().map(|x| self.act(x, sc, init)).collect::<R<_>>()?),
            Ast::Bin("=", l, r) if self.var_of(l, sc, init).is_some() => {
                Act::Assign(self.var_of(l, sc, init).unwrap(), self.expr(r, sc, init)?)
            }
            Ast::Bin("\\in", l, r) if self.var_of(l, sc, init).is_some() => {
                Act::AssignIn(self.var_of(l, sc, init).unwrap(), self.expr(r, sc, init)?)
            }
            Ast::Unchanged(x) if !init => {
                let mut vs = Vec::new();
                self.unchanged_vars(x, &mut vs)?;
                if vs.len() > 128 {
                    return Err("UNCHANGED of more than 128 variables".into());
                }
                Act::Unchanged(vs.into())
            }
            Ast::Quant(false, bounds, body) => {
                let n = sc.len();
                let bs = self.bounds(bounds, sc, init)?;
                let body = self.act(body, sc, init);
                sc.truncate(n);
                Act::Exists(bs, Box::new(body?))
            }
            Ast::If(c, t, e) => Act::If(
                self.expr(c, sc, init)?,
                Box::new(self.act(t, sc, init)?),
                Box::new(self.act(e, sc, init)?),
            ),
            Ast::Case(arms, other) => Act::Case(
                arms.iter().map(|(p, e)| Ok((self.expr(p, sc, init)?, self.act(e, sc, init)?))).collect::<R<_>>()?,
                match other {
                    Some(o) => Some(Box::new(self.act(o, sc, init)?)),
                    None => None,
                },
            ),
            Ast::Let(defs, body) => {
                let n = sc.len();
                let slots = self.let_defs(defs, sc, init)?;
                let body = self.act(body, sc, init);
                sc.truncate(n);
                Act::Lazy(slots, Box::new(body?))
            }
            Ast::Ident(name) | Ast::Apply(name, _) => {
                let args: &[Ast] = match a {
                    Ast::Apply(_, args) => args,
                    _ => &[],
                };
                if let Some(Entry::LetOp(d, len)) = Self::lookup(sc, name).cloned() {
                    let outer = sc[..len].to_vec();
                    let (binds, body) = self.inline(&d, &outer, args, sc, init, Self::act)?;
                    Act::Let(binds, Box::new(body))
                } else if Self::lookup(sc, name).is_none() && self.defs.contains_key(name) {
                    let d = self.defs[name].clone();
                    let (binds, body) = self.inline(&d, &[], args, sc, init, Self::act)?;
                    if binds.is_empty() { body } else { Act::Let(binds, Box::new(body)) }
                } else {
                    Act::Guard(self.expr(a, sc, init)?)
                }
            }
            _ => Act::Guard(self.expr(a, sc, init)?),
        })
    }

    fn let_defs(&mut self, defs: &[Rc<Def>], sc: &mut Vec<Entry>, init: bool) -> R<Box<[(u32, u32)]>> {
        let mut slots = Vec::new();
        for d in defs {
            if d.params.is_empty() {
                let e = self.expr(&d.body, sc, init)?;
                let id = self.lets.len() as u32;
                self.lets.push(e);
                let s = self.slot();
                slots.push((s, id));
                sc.push(Entry::Lazy(d.name.clone(), s, id));
            } else if Self::mentions(&d.body, &d.name) {
                let e = self.lift(d, sc, init)?;
                sc.push(e);
            } else {
                let len = sc.len();
                sc.push(Entry::LetOp(d.clone(), len));
            }
        }
        Ok(slots.into())
    }

    fn mentions(a: &Ast, name: &str) -> bool {
        match a {
            Ast::Ident(n) if n == name => true,
            Ast::Apply(n, _) if n == name => true,
            _ => children(a).into_iter().any(|x| Self::mentions(x, name)),
        }
    }

    /// LET RECURSIVE: compile the operator as its own frame. It cannot be
    /// inlined (its depth is decided at run time), and a frame cannot see
    /// the caller's slots, so the locals it reads become leading arguments.
    fn lift(&mut self, d: &Rc<Def>, sc: &[Entry], init: bool) -> R<Entry> {
        let mut captured: Vec<Capture> = Vec::new();
        for e in sc.iter().rev() {
            let (n, slot, lazy) = match e {
                Entry::Local(n, s) => (n, *s, None),
                Entry::Lazy(n, s, id) => (n, *s, Some(*id)),
                _ => continue,
            };
            if !d.params.contains(n) && Self::mentions(&d.body, n) && !captured.iter().any(|c| &c.name == n) {
                captured.push(Capture { name: n.clone(), slot, lazy });
            }
        }
        let idx = self.ops.len() as u32;
        self.ops.push(None);
        let saved = self.next_slot;
        self.next_slot = 0;
        let inner_caps: Vec<Capture> =
            captured.iter().map(|c| Capture { name: c.name.clone(), slot: self.slot(), lazy: None }).collect();
        let mut inner: Vec<Entry> = inner_caps.iter().map(|c| Entry::Local(c.name.clone(), c.slot)).collect();
        let inner_caps_len = inner_caps.len();
        inner.push(Entry::Lifted(d.clone(), idx, inner_caps));
        for p in &d.params {
            let s = self.slot();
            inner.push(Entry::Local(p.clone(), s));
        }
        let nparams = inner_caps_len + d.params.len();
        let body = self.expr(&d.body, &mut inner, init);
        let frame = self.next_slot;
        self.next_slot = saved;
        self.ops[idx as usize] = Some(Op { name: format!("{}#let", d.name), nparams, frame, body: body?, cached: None });
        Ok(Entry::Lifted(d.clone(), idx, captured))
    }

    fn call_lifted(&mut self, idx: u32, caps: &[Capture], args: &[Ast], sc: &mut Vec<Entry>, init: bool) -> R<Expr> {
        let mut es: Vec<Expr> = caps.iter().map(Capture::read).collect();
        for a in args {
            es.push(self.expr(a, sc, init)?);
        }
        Ok(Expr::Call(idx, es.into()))
    }

    /// Binds each bound's names in `sc` (left to right, so a later set may
    /// mention an earlier variable).
    fn bounds(&mut self, bs: &[crate::ast::Bound], sc: &mut Vec<Entry>, init: bool) -> R<Box<[crate::eval::Bound]>> {
        let mut out = Vec::new();
        for b in bs {
            let set = self.expr(&b.set, sc, init)?;
            let slots: Vec<u32> = b.names.iter().map(|_| self.slot()).collect();
            for (n, s) in b.names.iter().zip(&slots) {
                sc.push(Entry::Local(n.clone(), *s));
            }
            out.push(crate::eval::Bound { slots: slots.into(), tuple: b.tuple, set });
        }
        Ok(out.into())
    }

    // ---- expressions ----------------------------------------------------

    fn expr(&mut self, a: &Ast, sc: &mut Vec<Entry>, init: bool) -> R<Expr> {
        let bx = |e: Expr| Box::new(e);
        Ok(match a {
            Ast::Num(n) => Expr::Const(Value::Int(*n)),
            Ast::Str(s) => Expr::Const(Value::Str(self.intern(s))),
            Ast::Bool(b) => Expr::Const(Value::Bool(*b)),
            Ast::At => match Self::lookup(sc, "@") {
                Some(Entry::Local(_, s)) => Expr::Local(*s),
                _ => return Err("@ outside EXCEPT".into()),
            },
            Ast::Ident(n) => self.ident(n, sc, init)?,
            Ast::Apply(n, args) if n == "SelectSeq" && args.len() == 2 && Self::lookup(sc, n).is_none() => {
                let seq = self.expr(&args[0], sc, init)?;
                let slot = self.slot();
                let pred = match &args[1] {
                    Ast::Lambda(ps, body) if ps.len() == 1 => {
                        sc.push(Entry::Local(ps[0].clone(), slot));
                        let b = self.expr(body, sc, init);
                        sc.pop();
                        b?
                    }
                    Ast::Ident(op) if self.defs.get(op).is_some_and(|d| d.params.len() == 1) => {
                        Expr::Call(self.op_index(op), Box::new([Expr::Local(slot)]))
                    }
                    _ => return Err("SelectSeq wants a one-parameter LAMBDA or operator".into()),
                };
                Expr::SelectSeq(bx(seq), slot, bx(pred))
            }
            Ast::Lambda(..) => return Err("LAMBDA outside SelectSeq is not supported".into()),
            Ast::Apply(n, args) => {
                if let Some(Entry::Lifted(_, idx, caps)) = Self::lookup(sc, n).cloned() {
                    self.call_lifted(idx, &caps, args, sc, init)?
                } else if let Some(Entry::LetOp(d, len)) = Self::lookup(sc, n).cloned() {
                    let outer = sc[..len].to_vec();
                    let (binds, body) = self.inline(&d, &outer, args, sc, init, Self::expr)?;
                    Expr::Let(binds, bx(body))
                } else if self.defs.contains_key(n) {
                    let d = self.defs[n].clone();
                    if d.params.len() != args.len() {
                        return Err(format!("{n} takes {} arguments", d.params.len()));
                    }
                    let args = args.iter().map(|x| self.expr(x, sc, init)).collect::<R<_>>()?;
                    Expr::Call(self.op_index(n), args)
                } else if let Some((bi, arity)) = builtin_fn(n) {
                    if args.len() != arity {
                        return Err(format!("{n} takes {arity} arguments"));
                    }
                    Expr::Builtin(bi, args.iter().map(|x| self.expr(x, sc, init)).collect::<R<_>>()?)
                } else {
                    return Err(format!("unknown operator {n}"));
                }
            }
            Ast::Prime(x) => match &**x {
                Ast::Ident(n) if self.vars.contains_key(n) && Self::lookup(sc, n).is_none() => {
                    Expr::Primed(self.vars[n])
                }
                _ => return Err(format!("priming a non-variable {x:?} is not supported")),
            },
            // In a step predicate (a `[][A]_v` property), UNCHANGED e is e' = e.
            Ast::Unchanged(x) => {
                let mut vs = Vec::new();
                self.unchanged_vars(x, &mut vs)?;
                Expr::And(vs.into_iter().map(|v| Expr::Bin(Bin::Eq, bx(Expr::Primed(v)), bx(Expr::Var(v)))).collect())
            }
            Ast::Not(x) => Expr::Not(bx(self.expr(x, sc, init)?)),
            Ast::Neg(x) => Expr::Un(Un::Neg, bx(self.expr(x, sc, init)?)),
            Ast::And(v) => Expr::And(v.iter().map(|x| self.expr(x, sc, init)).collect::<R<_>>()?),
            Ast::Or(v) => Expr::Or(v.iter().map(|x| self.expr(x, sc, init)).collect::<R<_>>()?),
            Ast::Bin("=>", l, r) => Expr::Implies(bx(self.expr(l, sc, init)?), bx(self.expr(r, sc, init)?)),
            Ast::Bin(op, l, r) => {
                let b = bin(op).ok_or(format!("unsupported operator {op}"))?;
                Expr::Bin(b, bx(self.expr(l, sc, init)?), bx(self.expr(r, sc, init)?))
            }
            Ast::Product(v) => Expr::Product(v.iter().map(|x| self.expr(x, sc, init)).collect::<R<_>>()?),
            Ast::Prefix(op, x) => {
                let e = bx(self.expr(x, sc, init)?);
                match *op {
                    "SUBSET" => Expr::Un(Un::Subset, e),
                    "UNION" => Expr::Un(Un::Union, e),
                    "DOMAIN" => Expr::Un(Un::Domain, e),
                    _ => return Err(format!("unsupported prefix {op}")),
                }
            }
            Ast::If(c, t, e) => {
                Expr::If(bx(self.expr(c, sc, init)?), bx(self.expr(t, sc, init)?), bx(self.expr(e, sc, init)?))
            }
            Ast::Case(arms, other) => Expr::Case(
                arms.iter().map(|(p, e)| Ok((self.expr(p, sc, init)?, self.expr(e, sc, init)?))).collect::<R<_>>()?,
                match other {
                    Some(o) => Some(bx(self.expr(o, sc, init)?)),
                    None => None,
                },
            ),
            Ast::Let(defs, body) => {
                let n = sc.len();
                let slots = self.let_defs(defs, sc, init)?;
                let body = self.expr(body, sc, init);
                sc.truncate(n);
                Expr::Lazy(slots, bx(body?))
            }
            Ast::Quant(forall, bounds, body) => {
                let n = sc.len();
                let bs = self.bounds(bounds, sc, init)?;
                let body = self.expr(body, sc, init);
                sc.truncate(n);
                Expr::Quant(*forall, bs, bx(body?))
            }
            Ast::Choose(b, body) | Ast::SetFilter(b, body) => {
                let n = sc.len();
                let mut bs = self.bounds(std::slice::from_ref(b), sc, init)?.into_vec();
                let body = self.expr(body, sc, init);
                sc.truncate(n);
                let b = Box::new(bs.pop().unwrap());
                if matches!(a, Ast::Choose(..)) { Expr::Choose(b, bx(body?)) } else { Expr::SetFilter(b, bx(body?)) }
            }
            Ast::SetMap(body, bounds) | Ast::FuncCons(bounds, body) => {
                let n = sc.len();
                let bs = self.bounds(bounds, sc, init)?;
                let body = self.expr(body, sc, init);
                sc.truncate(n);
                if matches!(a, Ast::SetMap(..)) { Expr::SetMap(bs, bx(body?)) } else { Expr::FuncCons(bs, bx(body?)) }
            }
            Ast::SetEnum(v) => {
                let es: Vec<Expr> = v.iter().map(|x| self.expr(x, sc, init)).collect::<R<_>>()?;
                Expr::SetEnum(es.into())
            }
            Ast::FuncSet(d, r) => Expr::FuncSet(bx(self.expr(d, sc, init)?), bx(self.expr(r, sc, init)?)),
            Ast::Record(fs) | Ast::RecordSet(fs) => {
                let mut v = Vec::new();
                for (f, e) in fs {
                    let id = self.intern(f);
                    v.push((id, self.expr(e, sc, init)?));
                }
                v.sort_by_key(|(id, _)| *id);
                if matches!(a, Ast::Record(_)) { Expr::Record(v.into()) } else { Expr::RecSet(v.into()) }
            }
            Ast::Except(f, ups) => {
                let base = self.expr(f, sc, init)?;
                let mut us = Vec::new();
                for (path, val) in ups {
                    let mut p = Vec::new();
                    for el in path {
                        p.push(match el {
                            PathEl::Field(f) => PathE::Field(self.intern(f)),
                            PathEl::Idx(ix) => PathE::Idx(self.index_arg(ix, sc, init)?),
                        });
                    }
                    let at = self.slot();
                    sc.push(Entry::Local("@".into(), at));
                    let v = self.expr(val, sc, init);
                    sc.pop();
                    us.push(Update { path: p.into(), at, val: v? });
                }
                Expr::Except(bx(base), us.into())
            }
            Ast::App(f, args) => Expr::App(bx(self.expr(f, sc, init)?), bx(self.index_arg(args, sc, init)?)),
            Ast::Field(r, f) => {
                let id = self.intern(f);
                Expr::Field(bx(self.expr(r, sc, init)?), id)
            }
            Ast::Tuple(v) => Expr::Tuple(v.iter().map(|x| self.expr(x, sc, init)).collect::<R<_>>()?),
            Ast::Temporal("ENABLED", v) => Expr::Enabled(Box::new(self.act(&v[0], sc, false)?)),
            Ast::BoxAction(..) | Ast::Temporal(..) => return Err("a temporal formula where a value is expected".into()),
        })
    }

    /// `f[a, b]` applies f to the tuple <<a, b>>.
    fn index_arg(&mut self, args: &[Ast], sc: &mut Vec<Entry>, init: bool) -> R<Expr> {
        if args.len() == 1 {
            self.expr(&args[0], sc, init)
        } else {
            Ok(Expr::Tuple(args.iter().map(|x| self.expr(x, sc, init)).collect::<R<_>>()?))
        }
    }

    fn ident(&mut self, n: &str, sc: &mut Vec<Entry>, init: bool) -> R<Expr> {
        match Self::lookup(sc, n).cloned() {
            Some(Entry::Local(_, s)) => return Ok(Expr::Local(s)),
            Some(Entry::Lazy(_, s, id)) => return Ok(Expr::LetRef(s, id)),
            Some(Entry::LetOp(d, len)) => {
                let outer = sc[..len].to_vec();
                let (binds, body) = self.inline(&d, &outer, &[], sc, init, Self::expr)?;
                return Ok(Expr::Let(binds, Box::new(body)));
            }
            Some(Entry::Lifted(_, idx, caps)) => return self.call_lifted(idx, &caps, &[], sc, init),
            None => {}
        }
        if let Some(&v) = self.vars.get(n) {
            return Ok(if init { Expr::Primed(v) } else { Expr::Var(v) });
        }
        if let Some(op) = self.subst.get(n).cloned() {
            return self.ident(&op, sc, init);
        }
        if let Some(v) = self.consts.get(n) {
            return Ok(Expr::Const(v.clone()));
        }
        if let Some(d) = self.defs.get(n).cloned() {
            if !d.params.is_empty() {
                return Err(format!("{n} used without its arguments"));
            }
            // Init-mode references to a state-level operator must see the
            // variables being assigned, so they are inlined.
            if init && self.op_is_action(n, true) {
                let (_, body) = self.inline(&d, &[], &[], sc, init, Self::expr)?;
                return Ok(body);
            }
            return Ok(Expr::Call(self.op_index(n), Box::new([])));
        }
        if let Some(v) = is_builtin_value(n) {
            return Ok(Expr::Const(v));
        }
        Err(format!("unknown identifier {n}"))
    }

    // ---- finishing --------------------------------------------------------

    pub fn finish(
        mut self,
        init: Rooted<Act>,
        next: Rooted<Act>,
        invariants: Vec<Rooted<Expr>>,
        constraints: Vec<Rooted<Expr>>,
        assumes: &[Ast],
        check_deadlock: bool,
        symmetry: Option<Rooted<Expr>>,
        view: Option<Rooted<Expr>>,
    ) -> R<Program> {
        let mut assume_exprs = Vec::new();
        for a in assumes {
            self.next_slot = 0;
            let e = self.expr(a, &mut vec![], false)?;
            assume_exprs.push((e, self.next_slot));
        }
        self.drain_pending()?;
        let _ = crate::value::NAMES.set(std::mem::take(&mut self.name_list));
        let ops: Vec<Op> = self.ops.into_iter().map(|o| o.unwrap()).collect();
        let mut p = Program { vars: self.var_names, ops, lets: std::mem::take(&mut self.lets), init, next, invariants, constraints, check_deadlock, symmetry: vec![], view, cmp_order: vec![], properties: vec![], fairness: None };

        // Constant folding: a zero-arity operator that reaches no state
        // variable is evaluated once, here.
        p.cmp_order = (0..p.vars.len()).collect();
        let n = p.ops.len();
        let mut is_const = vec![None::<bool>; n];
        fn expr_const(e: &Expr, p: &Program, memo: &mut Vec<Option<bool>>) -> bool {
            match e {
                Expr::Var(_) | Expr::Primed(_) => false,
                Expr::LetRef(_, id) => expr_const(&p.lets[*id as usize], p, memo),
                Expr::Call(op, args) => {
                    op_const(*op as usize, p, memo) && args.iter().all(|a| expr_const(a, p, memo))
                }
                _ => {
                    let mut ok = true;
                    visit(e, &mut |x| ok &= expr_const(x, p, memo));
                    ok
                }
            }
        }
        fn op_const(i: usize, p: &Program, memo: &mut Vec<Option<bool>>) -> bool {
            if let Some(b) = memo[i] {
                return b;
            }
            memo[i] = Some(true); // recursion: assume, then refine
            let b = expr_const(&p.ops[i].body, p, memo);
            memo[i] = Some(b);
            b
        }
        for i in 0..n {
            op_const(i, &p, &mut is_const);
        }
        let empty: Vec<Value> = vec![];
        let mut bufs = Bufs::default();
        for i in 0..n {
            if is_const[i] == Some(true) && self.defs.get(&p.ops[i].name).is_some_and(|d| d.params.is_empty()) {
                let mut cx = bufs.cx(&empty, p.vars.len(), 0);
                if let Ok(v) = p.eval(&Expr::Call(i as u32, Box::new([])), &mut cx) {
                    p.ops[i].cached = Some(v);
                }
            }
        }
        if let Some(sym) = symmetry {
            let nnames = crate::value::NAMES.get().map_or(0, |n| n.len());
            let mut cx = bufs.cx(&empty, p.vars.len(), sym.frame);
            let set = p.eval(&sym.body, &mut cx).map_err(|e| format!("evaluating SYMMETRY {}: {e}", sym.name))?;
            for f in set.elems()?.iter() {
                let mut perm: Vec<u32> = (0..nnames as u32).collect();
                for (k, v) in f.pairs()? {
                    match (k, v) {
                        (Value::Model(a), Value::Model(b)) => perm[a as usize] = b,
                        (k, v) => return Err(format!("SYMMETRY permutes {k} to {v}: only model values can be permuted")),
                    }
                }
                if perm.iter().enumerate().any(|(i, &x)| i as u32 != x) && !p.symmetry.iter().any(|q| **q == *perm) {
                    p.symmetry.push(perm.into());
                }
            }
        }
        // TLC checks the group the given permutations generate, not the set
        // itself (MVPerms.permutationSubgroup): Permutations(A) \cup
        // Permutations(B) also permutes A and B together.
        let mut i = 0;
        while i < p.symmetry.len() {
            for j in 0..=i {
                for (x, y) in [(i, j), (j, i)] {
                    let c: Box<[u32]> = p.symmetry[x].iter().map(|&m| p.symmetry[y][m as usize]).collect();
                    if c.iter().enumerate().any(|(k, &v)| k as u32 != v) && !p.symmetry.contains(&c) {
                        p.symmetry.push(c);
                    }
                }
            }
            i += 1;
        }
        for (e, frame) in assume_exprs {
            let mut cx = bufs.cx(&empty, p.vars.len(), frame);
            if !p.eval_bool(&e, &mut cx)? {
                return Err("an ASSUME is false".into());
            }
        }
        Ok(p)
    }
}

/// Calls `f` on each direct sub-expression.
pub fn visit(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    let bounds = |bs: &[crate::eval::Bound], f: &mut dyn FnMut(&Expr)| bs.iter().for_each(|b| f(&b.set));
    match e {
        Expr::Const(_) | Expr::Local(_) | Expr::Var(_) | Expr::Primed(_) | Expr::LetRef(..) | Expr::Enabled(_) => {}
        Expr::Lazy(_, body) => f(body),
        Expr::SelectSeq(a, _, b) => {
            f(a);
            f(b)
        }
        Expr::Call(_, v) | Expr::And(v) | Expr::Or(v) | Expr::SetEnum(v) | Expr::Product(v) | Expr::Tuple(v)
        | Expr::Builtin(_, v) => v.iter().for_each(f),
        Expr::Not(a) | Expr::Un(_, a) | Expr::Field(a, _) => f(a),
        Expr::Implies(a, b) | Expr::FuncSet(a, b) | Expr::App(a, b) | Expr::Bin(_, a, b) => {
            f(a);
            f(b)
        }
        Expr::If(a, b, c) => {
            f(a);
            f(b);
            f(c)
        }
        Expr::Case(arms, o) => {
            arms.iter().for_each(|(p, v)| {
                f(p);
                f(v)
            });
            if let Some(o) = o {
                f(o)
            }
        }
        Expr::Let(defs, body) => {
            defs.iter().for_each(|(_, d)| f(d));
            f(body)
        }
        Expr::Quant(_, bs, body) | Expr::SetMap(bs, body) | Expr::FuncCons(bs, body) => {
            bounds(bs, f);
            f(body)
        }
        Expr::Choose(b, body) | Expr::SetFilter(b, body) => {
            f(&b.set);
            f(body)
        }
        Expr::Record(fs) | Expr::RecSet(fs) => fs.iter().for_each(|(_, x)| f(x)),
        Expr::Except(base, ups) => {
            f(base);
            for u in ups.iter() {
                for p in u.path.iter() {
                    if let PathE::Idx(x) = p {
                        f(x)
                    }
                }
                f(&u.val)
            }
        }
    }
}
