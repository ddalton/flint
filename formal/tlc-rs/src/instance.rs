//! `I == INSTANCE M WITH x <- e, ...`, by rewriting: every definition D of
//! M (and of the modules M extends) becomes a definition `I!D` of the
//! instantiating module, its body renamed so that
//!  - a reference to another definition of M is to its `I!` copy;
//!  - a constant or variable x of M named in WITH is a reference to a new
//!    zero-argument definition `I!__sub_x == e`, compiled in the
//!    instantiating module's scope, so nothing M binds can capture a name
//!    in e (and `x'` is `e'`: e evaluated in the next state);
//!  - a constant or variable of M not named in WITH is left as it is: the
//!    same name in the instantiating module (TLA+'s implicit substitution);
//!  - names M binds (parameters, quantifiers, LET) shadow all of the above.
//! A bare `INSTANCE M` does the same with no `I!` prefix.

use crate::ast::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub struct Expanded {
    pub defs: Vec<Rc<Def>>,
    pub assumes: Vec<Ast>,
    /// each renamed definition: its module of origin and name there
    pub origins: Vec<(String, (String, String))>,
}

struct Rw {
    prefix: String,
    defs: HashSet<String>,
    subs: HashMap<String, String>,
}

impl Rw {
    fn name(&self, n: &str, bound: &[String]) -> Option<String> {
        if bound.iter().any(|b| b == n) {
            return None;
        }
        if let Some(s) = self.subs.get(n) {
            return Some(s.clone());
        }
        if self.defs.contains(n) {
            return Some(format!("{}{n}", self.prefix));
        }
        None
    }

    fn def(&self, d: &Def, bound: &mut Vec<String>, name: String) -> Rc<Def> {
        let n = bound.len();
        bound.extend(d.params.iter().cloned());
        let body = self.ast(&d.body, bound);
        bound.truncate(n);
        Rc::new(Def { name, params: d.params.clone(), op_arity: d.op_arity.clone(), body })
    }

    fn bound(&self, b: &Bound, bound: &mut Vec<String>) -> Bound {
        Bound { names: b.names.clone(), tuple: b.tuple, set: self.ast(&b.set, bound) }
    }

    /// Binders: the sets are rewritten outside the scope they open.
    fn scoped(&self, bs: &[Bound], body: &Ast, bound: &mut Vec<String>) -> (Vec<Bound>, Ast) {
        let bs: Vec<Bound> = bs.iter().map(|b| self.bound(b, bound)).collect();
        let n = bound.len();
        bound.extend(bs.iter().flat_map(|b| b.names.iter().cloned()));
        let body = self.ast(body, bound);
        bound.truncate(n);
        (bs, body)
    }

    fn all(&self, v: &[Ast], bound: &mut Vec<String>) -> Vec<Ast> {
        v.iter().map(|x| self.ast(x, bound)).collect()
    }

    fn ast(&self, a: &Ast, bound: &mut Vec<String>) -> Ast {
        let b = |x: Ast| Box::new(x);
        match a {
            Ast::Num(_) | Ast::Str(_) | Ast::Bool(_) | Ast::At => a.clone(),
            Ast::Ident(n) => Ast::Ident(self.name(n, bound).unwrap_or_else(|| n.clone())),
            Ast::Apply(n, args) => {
                // a substituted constant is never an operator with arguments
                let n = if self.subs.contains_key(n) { None } else { self.name(n, bound) }.unwrap_or_else(|| n.clone());
                Ast::Apply(n, self.all(args, bound))
            }
            Ast::Prime(x) => Ast::Prime(b(self.ast(x, bound))),
            Ast::Unchanged(x) => Ast::Unchanged(b(self.ast(x, bound))),
            Ast::Not(x) => Ast::Not(b(self.ast(x, bound))),
            Ast::Neg(x) => Ast::Neg(b(self.ast(x, bound))),
            Ast::And(v) => Ast::And(self.all(v, bound)),
            Ast::Or(v) => Ast::Or(self.all(v, bound)),
            Ast::Bin(op, l, r) => Ast::Bin(op, b(self.ast(l, bound)), b(self.ast(r, bound))),
            Ast::Product(v) => Ast::Product(self.all(v, bound)),
            Ast::Prefix(op, x) => Ast::Prefix(op, b(self.ast(x, bound))),
            Ast::If(c, t, e) => Ast::If(b(self.ast(c, bound)), b(self.ast(t, bound)), b(self.ast(e, bound))),
            Ast::Case(arms, other) => Ast::Case(
                arms.iter().map(|(p, e)| (self.ast(p, bound), self.ast(e, bound))).collect(),
                other.as_ref().map(|o| b(self.ast(o, bound))),
            ),
            Ast::Let(defs, body) => {
                let n = bound.len();
                bound.extend(defs.iter().map(|d| d.name.clone()));
                let defs = defs.iter().map(|d| self.def(d, bound, d.name.clone())).collect();
                let body = self.ast(body, bound);
                bound.truncate(n);
                Ast::Let(defs, b(body))
            }
            Ast::Quant(forall, bs, body) => {
                let (bs, body) = self.scoped(bs, body, bound);
                Ast::Quant(*forall, bs, b(body))
            }
            Ast::Choose(bd, body) | Ast::SetFilter(bd, body) => {
                let (mut bs, body) = self.scoped(std::slice::from_ref(&**bd), body, bound);
                let bd = Box::new(bs.pop().unwrap());
                if matches!(a, Ast::Choose(..)) { Ast::Choose(bd, b(body)) } else { Ast::SetFilter(bd, b(body)) }
            }
            Ast::SetMap(body, bs) => {
                let (bs, body) = self.scoped(bs, body, bound);
                Ast::SetMap(b(body), bs)
            }
            Ast::FuncCons(bs, body) => {
                let (bs, body) = self.scoped(bs, body, bound);
                Ast::FuncCons(bs, b(body))
            }
            Ast::SetEnum(v) => Ast::SetEnum(self.all(v, bound)),
            Ast::FuncSet(d, r) => Ast::FuncSet(b(self.ast(d, bound)), b(self.ast(r, bound))),
            Ast::Record(fs) => Ast::Record(fs.iter().map(|(f, e)| (f.clone(), self.ast(e, bound))).collect()),
            Ast::RecordSet(fs) => Ast::RecordSet(fs.iter().map(|(f, e)| (f.clone(), self.ast(e, bound))).collect()),
            Ast::Except(f, ups) => Ast::Except(
                b(self.ast(f, bound)),
                ups.iter()
                    .map(|(path, v)| {
                        let path = path
                            .iter()
                            .map(|el| match el {
                                PathEl::Idx(ix) => PathEl::Idx(self.all(ix, bound)),
                                PathEl::Field(f) => PathEl::Field(f.clone()),
                            })
                            .collect();
                        (path, self.ast(v, bound))
                    })
                    .collect(),
            ),
            Ast::App(f, args) => Ast::App(b(self.ast(f, bound)), self.all(args, bound)),
            Ast::Field(r, f) => Ast::Field(b(self.ast(r, bound)), f.clone()),
            Ast::Tuple(v) => Ast::Tuple(self.all(v, bound)),
            Ast::Lambda(ps, body) => {
                let n = bound.len();
                bound.extend(ps.iter().cloned());
                let body = self.ast(body, bound);
                bound.truncate(n);
                Ast::Lambda(ps.clone(), b(body))
            }
            Ast::BoxAction(x, v) => Ast::BoxAction(b(self.ast(x, bound)), b(self.ast(v, bound))),
            Ast::Temporal(k, v) => Ast::Temporal(k, self.all(v, bound)),
        }
    }
}

/// `group`: the instantiated module and the modules it extends, each with
/// its own instances already expanded into its definitions.
pub fn expand(inst: &Instance, group: &[Module]) -> Result<Expanded, String> {
    let prefix = if inst.name.is_empty() { String::new() } else { format!("{}!", inst.name) };
    let declared: HashSet<&str> =
        group.iter().flat_map(|m| m.constants.iter().chain(m.variables.iter())).map(|s| s.as_str()).collect();
    let mut subs = HashMap::new();
    let mut defs = Vec::new();
    for (x, e) in &inst.subs {
        if !declared.contains(x.as_str()) {
            return Err(format!("INSTANCE {}: {x} is not a constant or variable of {}", inst.module, inst.module));
        }
        let name = format!("{}!__sub_{x}", if inst.name.is_empty() { &inst.module } else { &inst.name });
        defs.push(Rc::new(Def { name: name.clone(), params: vec![], op_arity: vec![], body: e.clone() }));
        subs.insert(x.clone(), name);
    }
    let rw = Rw { prefix: prefix.clone(), defs: group.iter().flat_map(|m| m.defs.iter().map(|d| d.name.clone())).collect(), subs };
    let mut origins = Vec::new();
    for m in group {
        for d in &m.defs {
            let name = format!("{prefix}{}", d.name);
            let origin = m.origins.get(&d.name).cloned().unwrap_or_else(|| (m.name.clone(), d.name.clone()));
            origins.push((name.clone(), origin));
            defs.push(rw.def(d, &mut Vec::new(), name));
        }
    }
    let assumes = group.iter().flat_map(|m| m.assumes.iter()).map(|a| rw.ast(a, &mut Vec::new())).collect();
    Ok(Expanded { defs, assumes, origins })
}
