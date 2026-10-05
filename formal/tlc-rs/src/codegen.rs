//! Spec -> Rust. Emits a Cargo crate whose binary is a checker for one
//! spec + cfg: the same BFS, fingerprints, symmetry and traces (this
//! library), with every formula compiled to straight Rust.
//!
//! What changes against the interpreter: a bound variable is a Rust local
//! holding a `&Value` (no frame stack), a quantifier is a loop, a lazy LET
//! is a `OnceCell`, and the action tree is nested blocks — a continuation
//! is wrapped in a local closure only where a branch would duplicate it.
//! The generated crate re-derives constants from the spec at startup and
//! refuses to run if the spec or cfg no longer hash to what it was built
//! from.

use crate::cli::Loaded;
use crate::eval::*;
use crate::value::Value;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

// ---- runtime support the generated code calls ---------------------------

pub mod rt {
    pub use crate::cli::Generated;
    pub use crate::eval::{binop, boolean_set, builtin_apply, Bi, Bin, Cx, Engine, Program, K};
    pub use crate::value::{Lazy, Value, R};
    pub use std::cell::OnceCell;
    pub use std::sync::Arc;

    /// What a generated formula returns where it reaches a construct the
    /// generator does not compile (ENABLED). The engine catches exactly
    /// this error and asks the interpreter for that invariant, constraint
    /// or step property instead; anywhere else it is reported as is.
    pub const UNSUPPORTED: &str = "ENABLED is not supported in generated checkers; use -engine interp";

    /// Constants the generated code refers to by index.
    pub struct G {
        pub k: Vec<Value>,
        pub c: Vec<Option<Value>>,
    }

    impl G {
        pub fn new(p: &Program) -> G {
            G { k: super::const_pool(p), c: p.ops.iter().map(|o| o.cached.clone()).collect() }
        }

        /// This thread's own copy of the pool (built on first use, never
        /// freed): every worker shares the one `G`, and a clone of a
        /// constant is an atomic update of a count all of them write, so
        /// past a few workers the cores spend their time moving that cache
        /// line. A copy per thread makes those counts private.
        pub fn local(&self) -> &'static G {
            thread_local! {
                static MINE: std::cell::Cell<Option<(usize, &'static G)>> = const { std::cell::Cell::new(None) };
            }
            let key = self as *const G as usize;
            MINE.with(|m| match m.get() {
                Some((k, g)) if k == key => g,
                _ => {
                    let g: &'static G = Box::leak(Box::new(G {
                        k: self.k.iter().map(Value::deep_clone).collect(),
                        c: self.c.iter().map(|o| o.as_ref().map(Value::deep_clone)).collect(),
                    }));
                    m.set(Some((key, g)));
                    g
                }
            })
        }
    }

    #[inline(always)]
    pub fn primed<'a>(v: &'a Option<Value>, name: &str) -> R<&'a Value> {
        v.as_ref().ok_or_else(|| format!("{name}' is read before the action assigns it"))
    }

    #[inline(always)]
    pub fn lazy<'a>(c: &'a OnceCell<Value>, f: impl FnOnce() -> R<Value>) -> R<&'a Value> {
        if let Some(v) = c.get() {
            return Ok(v);
        }
        let v = f()?;
        Ok(c.get_or_init(|| v))
    }

    /// One `![k] = ...` step of an EXCEPT; a key outside the domain leaves
    /// the function unchanged, as in TLA+.
    #[inline]
    pub fn upd(f: Value, k: &Value, g: impl FnOnce(&Value) -> R<Value>) -> R<Value> {
        let Ok(old) = f.apply_ref(k) else { return Ok(f) };
        let new = g(old)?.normalized()?;
        f.except(k, new)
    }

    #[inline]
    pub fn destructure(x: &Value, n: usize) -> R<&[Value]> {
        match x {
            Value::Seq(s) if s.len() == n => Ok(s),
            _ => Err(format!("cannot destructure {x} as a {n}-tuple")),
        }
    }

    pub fn union(x: Value) -> R<Value> {
        let mut out = Vec::new();
        for s in x.elems()?.iter() {
            out.extend(s.elems()?.iter().cloned());
        }
        Value::set(out)
    }
}

// ---- the constant pool: one walk, shared by generator and runtime --------

fn walk_expr<'e>(e: &'e Expr, f: &mut dyn FnMut(&'e Value)) {
    let bound = |bs: &'e [Bound], f: &mut dyn FnMut(&'e Value)| bs.iter().for_each(|b| walk_expr(&b.set, f));
    match e {
        Expr::Const(v) => {
            if !matches!(v, Value::Bool(_) | Value::Int(_) | Value::Str(_) | Value::Model(_)) {
                f(v)
            }
        }
        Expr::Local(_) | Expr::Var(_) | Expr::Primed(_) | Expr::LetRef(..) => {}
        Expr::Memo(_, _, e) => walk_expr(e, f),
        Expr::Enabled(a) => walk_act(a, f),
        Expr::Call(_, v) | Expr::And(v) | Expr::Or(v) | Expr::SetEnum(v) | Expr::Product(v) | Expr::Tuple(v)
        | Expr::Builtin(_, v) => v.iter().for_each(|x| walk_expr(x, f)),
        Expr::Not(a) | Expr::Un(_, a) | Expr::Field(a, _) | Expr::Lazy(_, a) => walk_expr(a, f),
        Expr::Implies(a, b) | Expr::FuncSet(a, b) | Expr::App(a, b) | Expr::Bin(_, a, b) | Expr::SelectSeq(a, _, b) => {
            walk_expr(a, f);
            walk_expr(b, f)
        }
        Expr::If(a, b, c) => {
            walk_expr(a, f);
            walk_expr(b, f);
            walk_expr(c, f)
        }
        Expr::Case(arms, o) => {
            for (p, v) in arms.iter() {
                walk_expr(p, f);
                walk_expr(v, f);
            }
            if let Some(o) = o {
                walk_expr(o, f)
            }
        }
        Expr::Let(defs, body) => {
            defs.iter().for_each(|(_, d)| walk_expr(d, f));
            walk_expr(body, f)
        }
        Expr::Quant(_, bs, body) | Expr::SetMap(bs, body) | Expr::FuncCons(bs, body) => {
            bound(bs, f);
            walk_expr(body, f)
        }
        Expr::Choose(b, body) | Expr::SetFilter(b, body) => {
            walk_expr(&b.set, f);
            walk_expr(body, f)
        }
        Expr::Record(fs) | Expr::RecSet(fs) => fs.iter().for_each(|(_, x)| walk_expr(x, f)),
        Expr::Except(base, ups) => {
            walk_expr(base, f);
            for u in ups.iter() {
                for p in u.path.iter() {
                    if let PathE::Idx(x) = p {
                        walk_expr(x, f)
                    }
                }
                walk_expr(&u.val, f)
            }
        }
    }
}

fn walk_act<'e>(a: &'e Act, f: &mut dyn FnMut(&'e Value)) {
    match a {
        Act::Guard(e) | Act::Assign(_, e) | Act::AssignIn(_, e) => walk_expr(e, f),
        Act::And(v) | Act::Or(v) => v.iter().for_each(|x| walk_act(x, f)),
        Act::Exists(bs, body) => {
            bs.iter().for_each(|b| walk_expr(&b.set, f));
            walk_act(body, f)
        }
        Act::If(c, t, e) => {
            walk_expr(c, f);
            walk_act(t, f);
            walk_act(e, f)
        }
        Act::Case(arms, o) => {
            for (p, b) in arms.iter() {
                walk_expr(p, f);
                walk_act(b, f);
            }
            if let Some(o) = o {
                walk_act(o, f)
            }
        }
        Act::Let(defs, body) => {
            defs.iter().for_each(|(_, d)| walk_expr(d, f));
            walk_act(body, f)
        }
        Act::Lazy(_, body) => walk_act(body, f),
        Act::Unchanged(_) => {}
    }
}

fn walk_program<'p>(p: &'p Program, f: &mut dyn FnMut(&'p Value)) {
    p.ops.iter().for_each(|o| walk_expr(&o.body, f));
    p.lets.iter().for_each(|e| walk_expr(e, f));
    walk_act(&p.init.body, f);
    walk_act(&p.next.body, f);
    p.invariants.iter().for_each(|r| walk_expr(&r.body, f));
    p.constraints.iter().for_each(|r| walk_expr(&r.body, f));
    if let Some(v) = &p.view {
        walk_expr(&v.body, f)
    }
    // Last, so the indices above do not move: the step properties the
    // generator compiles (`[][A]_v`).
    p.properties.iter().for_each(|pr| walk_tprop(&pr.body, f));
}

fn walk_tprop<'p>(t: &'p crate::eval::TProp, f: &mut dyn FnMut(&'p Value)) {
    use crate::eval::TProp;
    match t {
        TProp::ForAll(_, body) | TProp::Let(_, body) => walk_tprop(body, f),
        TProp::And(v) => v.iter().for_each(|x| walk_tprop(x, f)),
        TProp::ActionBox(a, sub) => {
            walk_expr(a, f);
            walk_expr(sub, f)
        }
        _ => {}
    }
}

pub fn const_pool(p: &Program) -> Vec<Value> {
    let mut out = Vec::new();
    walk_program(p, &mut |v| out.push(v.clone()));
    out
}

// ---- the generator --------------------------------------------------------

struct Gen<'p> {
    p: &'p Program,
    pool: HashMap<*const Value, usize>,
    n: usize,
    /// how generated code names the primed-variable array
    nx: &'static str,
    /// how it passes that array to an operator function
    nx_arg: &'static str,
}

fn is_scalar(v: &Value) -> bool {
    matches!(v, Value::Bool(_) | Value::Int(_) | Value::Str(_) | Value::Model(_))
}

fn scalar(v: &Value) -> String {
    match v {
        Value::Bool(b) => format!("Value::Bool({b})"),
        Value::Int(n) => format!("Value::Int({n})"),
        Value::Str(s) => format!("Value::Str({s})"),
        Value::Model(s) => format!("Value::Model({s})"),
        _ => unreachable!(),
    }
}

impl Gen<'_> {
    fn fresh(&mut self) -> usize {
        self.n += 1;
        self.n
    }

    /// Code of type `&Value`, when `e` can be borrowed.
    fn r#ref(&mut self, e: &Expr) -> Option<String> {
        Some(match e {
            Expr::Local(i) => format!("s{i}"),
            Expr::Var(i) => format!("(&st[{i}])"),
            Expr::Const(v) if !is_scalar(v) => format!("(&g.k[{}])", self.pool[&(v as *const Value)]),
            Expr::Call(op, args) if args.is_empty() && self.p.ops[*op as usize].cached.is_some() => {
                format!("g.c[{op}].as_ref().unwrap()")
            }
            Expr::LetRef(slot, _) => format!("lazy(&l{slot}, || f{slot}({}))?", self.nx_arg),
            Expr::App(f, x) => {
                let f = self.r#ref(f)?;
                let x = self.arg(x);
                format!("({f}).apply_ref({x})?")
            }
            Expr::Field(r, id) => {
                let r = self.r#ref(r)?;
                format!("({r}).field_ref({id})?")
            }
            _ => return None,
        })
    }

    /// Code of type `&Value`, borrowing when possible, else a temporary.
    fn arg(&mut self, e: &Expr) -> String {
        match self.r#ref(e) {
            Some(r) => r,
            None => format!("&({})", self.val(e)),
        }
    }

    fn bools(&mut self, v: &[Expr], op: &str) -> String {
        let parts: Vec<String> = v.iter().map(|x| format!("({})", self.boolean(x))).collect();
        if parts.is_empty() {
            return (op == "&&").to_string();
        }
        format!("({})", parts.join(&format!(" {op} ")))
    }

    fn boolean(&mut self, e: &Expr) -> String {
        match e {
            Expr::Const(Value::Bool(b)) => b.to_string(),
            Expr::Not(a) => format!("!({})", self.boolean(a)),
            Expr::And(v) => self.bools(v, "&&"),
            Expr::Or(v) => self.bools(v, "||"),
            Expr::Implies(a, b) => format!("(!({}) || ({}))", self.boolean(a), self.boolean(b)),
            Expr::Bin(Bin::Equiv, a, b) => format!("(({}) == ({}))", self.boolean(a), self.boolean(b)),
            Expr::Bin(Bin::Eq, a, b) => format!("(({}) == ({}))", self.arg(a), self.arg(b)),
            Expr::Bin(Bin::Neq, a, b) => format!("(({}) != ({}))", self.arg(a), self.arg(b)),
            Expr::Bin(Bin::In, a, b) => {
                let x = self.arg(a);
                format!("({}).contains({x})?", self.arg(b))
            }
            Expr::Bin(Bin::NotIn, a, b) => {
                let x = self.arg(a);
                format!("!({}).contains({x})?", self.arg(b))
            }
            Expr::Bin(op @ (Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge), a, b) => {
                let o = match op {
                    Bin::Lt => "<",
                    Bin::Le => "<=",
                    Bin::Gt => ">",
                    _ => ">=",
                };
                format!("(({}).as_int()? {o} ({}).as_int()?)", self.arg(a), self.arg(b))
            }
            Expr::Quant(forall, bs, body) => {
                let q = self.fresh();
                let body = self.boolean(body);
                let test = if *forall { format!("if !({body}) {{ break 'q{q} false; }}") } else { format!("if {body} {{ break 'q{q} true; }}") };
                let inner = self.loops(bs, &test);
                format!("('q{q}: {{ {inner} {forall} }})")
            }
            _ => format!("({}).as_bool()?", self.arg(e)),
        }
    }

    /// Nested loops binding `bs`, with `body` innermost.
    fn loops(&mut self, bs: &[Bound], body: &str) -> String {
        let mut code = body.to_string();
        for b in bs.iter().rev() {
            let n = self.fresh();
            let set = self.val(&b.set);
            let bind = if b.tuple {
                let mut s = format!("let t{n} = destructure(x{n}, {})?; ", b.slots.len());
                for (j, slot) in b.slots.iter().enumerate() {
                    let _ = write!(s, "let s{slot}: &Value = &t{n}[{j}]; ");
                }
                s
            } else {
                format!("let s{}: &Value = x{n}; ", b.slots[0])
            };
            code = format!("{{ let set{n} = ({set}).elems()?; for x{n} in set{n}.iter() {{ {bind}{code} }} }}");
        }
        code
    }

    fn val(&mut self, e: &Expr) -> String {
        match e {
            Expr::Memo(_, _, e) => self.val(e),
            Expr::Const(v) if is_scalar(v) => scalar(v),
            Expr::Primed(i) => format!("primed(&{}[{i}], {:?})?.clone()", self.nx, self.p.vars[*i as usize]),
            Expr::Call(op, args) if !(args.is_empty() && self.p.ops[*op as usize].cached.is_some()) => {
                let args: Vec<String> = args.iter().map(|a| self.arg(a)).collect();
                let mut s = format!("op_{op}(g, st, {}", self.nx_arg);
                for a in args {
                    let _ = write!(s, ", {a}");
                }
                s + ")?"
            }
            Expr::Not(_) | Expr::And(_) | Expr::Or(_) | Expr::Implies(..) | Expr::Quant(..) => {
                format!("Value::Bool({})", self.boolean(e))
            }
            Expr::Bin(Bin::Eq | Bin::Neq | Bin::In | Bin::NotIn | Bin::Equiv | Bin::Lt | Bin::Le | Bin::Gt | Bin::Ge, ..) => {
                format!("Value::Bool({})", self.boolean(e))
            }
            Expr::If(c, a, b) => format!("(if {} {{ {} }} else {{ {} }})", self.boolean(c), self.val(a), self.val(b)),
            Expr::Case(arms, other) => {
                let mut s = String::from("(");
                for (p, v) in arms.iter() {
                    let _ = write!(s, "if {} {{ {} }} else ", self.boolean(p), self.val(v));
                }
                match other {
                    Some(o) => {
                        let _ = write!(s, "{{ {} }})", self.val(o));
                    }
                    None => s.push_str("{ return Err(\"no CASE arm matched\".into()) })"),
                }
                s
            }
            Expr::Let(defs, body) => {
                let mut s = String::from("{ ");
                for (slot, d) in defs.iter() {
                    let _ = write!(s, "let s{slot}_o: Value = {}; let s{slot}: &Value = &s{slot}_o; ", self.val(d));
                }
                s + &self.val(body) + " }"
            }
            Expr::Lazy(slots, body) => {
                let s = self.lazy_defs(slots);
                s + &self.val(body) + " }"
            }
            // An expression of type Value that returns UNSUPPORTED (a bare
            // `return` is of type `!`, and `&!` has no methods: the crate
            // did not compile).
            Expr::Enabled(_) => "(Err::<Value, String>(UNSUPPORTED.into())?)".to_string(),
            Expr::SelectSeq(seq, slot, pred) => {
                let n = self.fresh();
                let seq = self.val(seq);
                let pred = self.boolean(pred);
                format!(
                    "{{ let Value::Seq(q{n}) = {seq} else {{ return Err(\"SelectSeq of a non-sequence\".into()) }}; \
                     let mut o{n} = Vec::with_capacity(q{n}.len()); \
                     for s{slot} in q{n}.iter() {{ if {pred} {{ o{n}.push(s{slot}.clone()); }} }} Value::Seq(o{n}.into()) }}"
                )
            }
            Expr::Choose(b, p) => {
                let c = self.fresh();
                let test = format!("if {} {{ break 'c{c} x_choose.clone(); }}", self.boolean(p));
                // bind the chosen element under a known name
                let inner = self.loops(std::slice::from_ref(b), &format!("let x_choose: &Value = {}; {test}", bound_name(b)));
                format!("('c{c}: {{ {inner} return Err(\"CHOOSE found no element satisfying its predicate\".into()) }})")
            }
            Expr::SetEnum(v) => {
                let parts: Vec<String> = v.iter().map(|x| self.val(x)).collect();
                format!("Value::set(vec![{}])?", parts.join(", "))
            }
            Expr::SetFilter(b, p) => {
                let n = self.fresh();
                let test = format!("if {} {{ o{n}.push({}.clone()); }}", self.boolean(p), bound_name(b));
                let inner = self.loops(std::slice::from_ref(b), &test);
                format!("{{ let mut o{n}: Vec<Value> = Vec::new(); {inner} Value::set_sorted(o{n}) }}")
            }
            Expr::SetMap(bs, body) => {
                let n = self.fresh();
                let push = format!("o{n}.push({});", self.val(body));
                let inner = self.loops(bs, &push);
                format!("{{ let mut o{n}: Vec<Value> = Vec::new(); {inner} Value::set(o{n})? }}")
            }
            Expr::FuncCons(bs, body) => {
                let n = self.fresh();
                let key = if bs.len() == 1 && !bs[0].tuple {
                    format!("{}.clone()", bound_name(&bs[0]))
                } else {
                    let names: Vec<String> =
                        bs.iter().flat_map(|b| b.slots.iter()).map(|s| format!("s{s}.clone()")).collect();
                    format!("Value::Seq(vec![{}].into())", names.join(", "))
                };
                let push = format!("o{n}.push(({key}, ({}).normalized()?));", self.val(body));
                let inner = self.loops(bs, &push);
                format!("{{ let mut o{n}: Vec<(Value, Value)> = Vec::new(); {inner} Value::func(o{n})? }}")
            }
            Expr::Record(fields) => {
                let parts: Vec<String> =
                    fields.iter().map(|(f, a)| format!("(Value::Str({f}), ({}).normalized()?)", self.val(a))).collect();
                format!("Value::func_sorted(vec![{}])", parts.join(", "))
            }
            Expr::RecSet(fields) => {
                let parts: Vec<String> = fields.iter().map(|(f, a)| format!("({f}u32, {})", self.val(a))).collect();
                format!("Value::Lazy(Arc::new(Lazy::RecSet(vec![{}].into())))", parts.join(", "))
            }
            Expr::FuncSet(d, r) => format!("Value::Lazy(Arc::new(Lazy::FuncSet({}, {})))", self.val(d), self.val(r)),
            Expr::Product(v) => {
                let parts: Vec<String> = v.iter().map(|x| self.val(x)).collect();
                format!("Value::Lazy(Arc::new(Lazy::Product(vec![{}].into())))", parts.join(", "))
            }
            Expr::Tuple(v) => {
                let parts: Vec<String> = v.iter().map(|x| format!("({}).normalized()?", self.val(x))).collect();
                format!("Value::Seq(vec![{}].into())", parts.join(", "))
            }
            Expr::App(f, x) => {
                let f = self.arg(f);
                let x = self.arg(x);
                format!("({f}).apply_ref({x})?.clone()")
            }
            Expr::Field(r, id) => format!("({}).field_ref({id})?.clone()", self.arg(r)),
            Expr::Except(f, ups) => {
                let n = self.fresh();
                let mut s = format!("{{ let mut v{n}: Value = {}; ", self.val(f));
                for u in ups.iter() {
                    let val = self.val(&u.val);
                    let mut inner = format!("{{ let s{}: &Value = o_last; Ok({val}) }}", u.at);
                    for (depth, p) in u.path.iter().enumerate().rev() {
                        let key = match p {
                            PathE::Idx(e) => self.arg(e),
                            PathE::Field(id) => format!("&Value::Str({id})"),
                        };
                        let param = if depth + 1 == u.path.len() { "o_last".to_string() } else { format!("o{n}_{depth}") };
                        let base = if depth == 0 { format!("v{n}") } else { format!("o{n}_{}.clone()", depth - 1) };
                        inner = format!("upd({base}, {key}, |{param}: &Value| -> R<Value> {{ {inner} }})");
                    }
                    let _ = write!(s, "v{n} = {inner}?; ");
                }
                s + &format!("v{n} }}")
            }
            Expr::Bin(Bin::Plus, a, b) => format!("Value::Int(({}).as_int()? + ({}).as_int()?)", self.arg(a), self.arg(b)),
            Expr::Bin(Bin::Minus, a, b) => format!("Value::Int(({}).as_int()? - ({}).as_int()?)", self.arg(a), self.arg(b)),
            Expr::Bin(op, a, b) => format!("binop(Bin::{op:?}, {}, {})?", self.val(a), self.val(b)),
            Expr::Un(op, a) => {
                let x = self.val(a);
                match op {
                    Un::Neg => format!("Value::Int(-({x}).as_int()?)"),
                    Un::Subset => format!("Value::Lazy(Arc::new(Lazy::Subset({x})))"),
                    Un::Domain => format!("({x}).domain()?"),
                    Un::IsFcn => format!("Value::Bool(({x}).domain().is_ok())"),
                    Un::Union => format!("union({x})?"),
                }
            }
            Expr::Builtin(bi, args) => {
                let parts: Vec<String> = args.iter().map(|x| self.val(x)).collect();
                format!("builtin_apply(Bi::{bi:?}, vec![{}])?", parts.join(", "))
            }
            _ => match self.r#ref(e) {
                Some(r) => format!("({r}).clone()"),
                None => unreachable!("no code for {:?}", std::mem::discriminant(e)),
            },
        }
    }

    /// Opens a block defining each LET body once, as a closure over the
    /// primed-variable array (a parameter, so it holds no borrow of it
    /// across later assignments), and the cell that memoizes it.
    /// References call it through `lazy`. Inlining the body at each
    /// reference instead is exponential in nested LETs: LeanSubtree's
    /// Next came to 135 MB of source.
    fn lazy_defs(&mut self, slots: &[(u32, u32)]) -> String {
        let mut s = String::from("{ ");
        for (slot, id) in slots.iter() {
            let (nx, nx_arg) = (self.nx, self.nx_arg);
            self.nx = "nx";
            self.nx_arg = "nx";
            let body = self.val(&self.p.lets[*id as usize]);
            self.nx = nx;
            self.nx_arg = nx_arg;
            let _ = write!(
                s,
                "let l{slot} = OnceCell::<Value>::new(); let f{slot} = |nx: &[Option<Value>]| -> R<Value> {{ Ok({body}) }}; "
            );
        }
        s
    }

    // ---- actions --------------------------------------------------------

    /// Wraps a continuation that a branch would duplicate into a closure.
    fn share(&mut self, k: &str, pre: &mut String) -> String {
        if k.len() < 40 && !k.contains('{') {
            return k.to_string();
        }
        let n = self.fresh();
        let _ = write!(pre, "let mut k{n} = |cx: &mut Cx<'a>| -> R<()> {{ {k} Ok(()) }}; ");
        format!("k{n}(cx)?;")
    }

    fn act(&mut self, a: &Act, k: &str) -> String {
        match a {
            Act::Guard(e) => format!("if {} {{ {k} }}", self.boolean(e)),
            Act::And(v) => {
                let mut code = k.to_string();
                for x in v.iter().rev() {
                    code = self.act(x, &code);
                }
                code
            }
            Act::Or(v) => {
                let mut pre = String::new();
                let k = if v.len() > 1 { self.share(k, &mut pre) } else { k.to_string() };
                let branches: Vec<String> = v.iter().map(|b| format!("{{ {} }}", self.act(b, &k))).collect();
                format!("{{ {pre}{} }}", branches.join(" "))
            }
            Act::Exists(bs, body) => {
                let body = self.act(body, k);
                self.loops(bs, &body)
            }
            Act::If(c, t, e) => {
                let mut pre = String::new();
                let k = self.share(k, &mut pre);
                let c = self.boolean(c);
                format!("{{ {pre}if {c} {{ {} }} else {{ {} }} }}", self.act(t, &k), self.act(e, &k))
            }
            Act::Case(arms, other) => {
                let mut pre = String::new();
                let k = self.share(k, &mut pre);
                let mut s = format!("{{ {pre}");
                for (p, b) in arms.iter() {
                    let _ = write!(s, "if {} {{ {} }} else ", self.boolean(p), self.act(b, &k));
                }
                match other {
                    Some(o) => {
                        let _ = write!(s, "{{ {} }} }}", self.act(o, &k));
                    }
                    None => s.push_str("{ return Err(\"no CASE arm matched\".into()); } }"),
                }
                s
            }
            Act::Let(defs, body) => {
                let mut s = String::from("{ ");
                for (slot, d) in defs.iter() {
                    let _ = write!(s, "let s{slot}_o: Value = {}; let s{slot}: &Value = &s{slot}_o; ", self.val(d));
                }
                s + &self.act(body, k) + " }"
            }
            Act::Lazy(slots, body) => {
                let s = self.lazy_defs(slots);
                s + &self.act(body, k) + " }"
            }
            Act::Assign(var, e) => {
                let v = format!("({}).normalized()?", self.val(e));
                self.assign(*var, &v, k)
            }
            Act::AssignIn(var, s) => {
                let n = self.fresh();
                let set = self.val(s);
                let inner = self.assign(*var, &format!("x{n}.clone()"), k);
                format!("{{ let set{n} = ({set}).elems()?; for x{n} in set{n}.iter() {{ {inner} }} }}")
            }
            Act::Unchanged(vars) => {
                let n = self.fresh();
                let list: Vec<String> = vars.iter().map(|v| format!("{v}usize")).collect();
                format!(
                    "{{ let vs{n}: &[usize] = &[{}]; let mut m{n}: u128 = 0; let mut ok{n} = true; \
                     for (j, &v) in vs{n}.iter().enumerate() {{ match &cx.next[v] {{ \
                       None => {{ cx.next[v] = Some(st[v].clone()); m{n} |= 1 << j; }} \
                       Some(o) => if *o != st[v] {{ ok{n} = false; break; }} }} }} \
                     if ok{n} {{ {k} }} \
                     for (j, &v) in vs{n}.iter().enumerate() {{ if m{n} & (1 << j) != 0 {{ cx.next[v] = None; }} }} }}",
                    list.join(", ")
                )
            }
        }
    }

    fn assign(&mut self, var: u32, v: &str, k: &str) -> String {
        let n = self.fresh();
        format!(
            "'b{n}: {{ let v{n}: Value = {v}; let fresh{n} = cx.next[{var}].is_none(); \
             if fresh{n} {{ cx.next[{var}] = Some(v{n}); }} else if cx.next[{var}].as_ref() != Some(&v{n}) {{ break 'b{n}; }} \
             {k} if fresh{n} {{ cx.next[{var}] = None; }} }}"
        )
    }
}

fn bound_name(b: &Bound) -> String {
    if b.tuple {
        // the tuple itself: rebuild it from its components
        let parts: Vec<String> = b.slots.iter().map(|s| format!("s{s}.clone()")).collect();
        format!("(&Value::Seq(vec![{}].into()))", parts.join(", "))
    } else {
        format!("s{}", b.slots[0])
    }
}

pub fn generate(l: &Loaded, dir: &Path) -> Result<(), String> {
    let p = &l.prog;
    let mut pool = HashMap::new();
    let mut i = 0;
    walk_program(p, &mut |v| {
        pool.insert(v as *const Value, i);
        i += 1;
    });
    let mut g = Gen { p, pool, n: 0, nx: "nx", nx_arg: "nx" };
    let mut code = String::new();
    code.push_str(
        "// GENERATED by tlc-rs -codegen. Do not edit; regenerate.\n\
         #![allow(unused, unreachable_code, unused_parens, unused_braces, unused_labels, clippy::all)]\n\
         use tlc_rs::codegen::rt::*;\n\n",
    );
    for (i, o) in p.ops.iter().enumerate() {
        let nparams = o.nparams;
        let mut sig = format!("fn op_{i}(g: &G, st: &[Value], nx: &[Option<Value>]");
        let mut binds = String::new();
        for j in 0..nparams {
            let _ = write!(sig, ", a{j}: &Value");
            let _ = write!(binds, "let s{j}: &Value = a{j}; ");
        }
        let body = g.val(&o.body);
        let _ = writeln!(code, "// {}\n{sig}) -> R<Value> {{ {binds}Ok({body}) }}\n", o.name);
    }
    for (kind, list) in [("inv", &p.invariants), ("con", &p.constraints)] {
        for (i, r) in list.iter().enumerate() {
            let body = g.boolean(&r.body);
            let _ = writeln!(
                code,
                "// {}\nfn {kind}_{i}(g: &G, st: &[Value]) -> R<bool> {{ let nx: &[Option<Value>] = &[]; Ok({body}) }}\n",
                r.name
            );
        }
    }
    // Step properties `[][A]_v` with no temporal-level parameters: compiled,
    // so a checker generated from a spec with them no longer evaluates them
    // in the interpreter on every transition. The others (none in our specs
    // today) fall back to it: `step_prop` returns None for their index.
    let (insts, _) = crate::liveness::instances(p)?;
    let mut compiled = Vec::new();
    for (i, inst) in insts.iter().enumerate() {
        let crate::eval::TProp::ActionBox(a, sub) = inst.leaf else { continue };
        if !inst.env.is_empty() {
            continue;
        }
        let (sv, ab) = (g.val(sub), g.boolean(a));
        let _ = writeln!(
            code,
            "// {} (step property)\nfn aprop_{i}(g: &G, s: &[Value], t: &[Value]) -> R<bool> {{ \
             let nx: &[Option<Value>] = &[]; \
             let v0 = {{ let st = s; {sv} }}; let v1 = {{ let st = t; {sv} }}; \
             if v0 == v1 {{ return Ok(true); }} \
             let nxv: Vec<Option<Value>> = t.iter().map(|v| Some(v.clone())).collect(); \
             let nx: &[Option<Value>] = &nxv; let st = s; Ok({ab}) }}\n",
            inst.name
        );
        compiled.push(i);
    }
    let view = match &p.view {
        Some(v) => g.val(&v.body),
        None => "return Err(\"no VIEW\".into())".into(),
    };
    let _ = writeln!(code, "fn view(g: &G, st: &[Value]) -> R<Value> {{ let nx: &[Option<Value>] = &[]; Ok({view}) }}\n");
    // The VIEW's parts as the seen-set key splits them (`symkey::view_parts`),
    // each compiled: under SYMMETRY/VIEW the key evaluates one part per new
    // state, and in the interpreter that was most of a generated checker's
    // time (ForgeSyncKeptSet, 2026-10-05: 63% of its samples).
    let parts = crate::symkey::view_parts(p).unwrap_or_default();
    for (c, (e, _)) in parts.iter().enumerate() {
        let body = g.val(e);
        let _ = writeln!(code, "fn vpart_{c}(g: &G, st: &[Value]) -> R<Value> {{ let nx: &[Option<Value>] = &[]; Ok({body}) }}\n");
    }
    let vparts: String = (0..parts.len())
        .map(|c| format!("{c} => match vpart_{c}(self.g.local(), st) {{ Err(e) if e == UNSUPPORTED => None, r => Some(r) }}, "))
        .collect();
    g.nx = "cx.next";
    g.nx_arg = "&cx.next[..]";
    for (name, a) in [("init", &p.init.body), ("next", &p.next.body)] {
        let body = g.act(a, "emit(cx)?;");
        let _ = writeln!(
            code,
            "fn {name}<'a>(g: &G, cx: &mut Cx<'a>, emit: K<'_, 'a>) -> R<()> {{ let st: &[Value] = cx.state; {body} Ok(()) }}\n"
        );
    }
    // A formula the generator could not compile returns UNSUPPORTED; that
    // one is answered by the interpreter (`self.p`), the rest stay compiled.
    let aprops: String = compiled
        .iter()
        .map(|i| format!("{i} => match aprop_{i}(self.g.local(), s, t) {{ Err(e) if e == UNSUPPORTED => None, r => Some(r) }}, "))
        .collect();
    let arms = |kind: &str, interp: &str, n: usize| -> String {
        (0..n)
            .map(|i| {
                format!(
                    "{i} => match {kind}_{i}(self.g.local(), cx.state) {{ Err(e) if e == UNSUPPORTED => Engine::{interp}(self.p, i, cx), r => r }}, "
                )
            })
            .collect::<String>()
    };
    let _ = write!(
        code,
        "struct E<'p> {{ g: G, p: &'p Program }}\n\
         impl Engine for E<'_> {{\n\
         fn init<'a>(&self, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {{ init(self.g.local(), cx, k) }}\n\
         fn next<'a>(&self, cx: &mut Cx<'a>, k: K<'_, 'a>) -> R<()> {{ next(self.g.local(), cx, k) }}\n\
         fn invariant(&self, i: usize, cx: &mut Cx) -> R<bool> {{ match i {{ {} _ => unreachable!() }} }}\n\
         fn constraint(&self, i: usize, cx: &mut Cx) -> R<bool> {{ match i {{ {} _ => unreachable!() }} }}\n\
         fn view(&self, cx: &mut Cx) -> R<Value> {{ view(self.g.local(), cx.state) }}\n\
         fn step_prop(&self, i: usize, s: &[Value], t: &[Value]) -> Option<R<bool>> {{ match i {{ {aprops}_ => None }} }}\n\
         fn view_part(&self, c: usize, st: &[Value]) -> Option<R<Value>> {{ match c {{ {vparts}_ => None }} }}\n\
         }}\n\n\
         fn make(p: &Program) -> Box<dyn Engine + '_> {{ Box::new(E {{ g: G::new(p), p }}) }}\n\n\
         fn main() -> std::process::ExitCode {{ tlc_rs::cli::main(Some(Generated {{ source_hash: {:#x}, make }})) }}\n",
        arms("inv", "invariant", p.invariants.len()),
        arms("con", "constraint", p.constraints.len()),
        l.source_hash
    );

    let src = dir.join("src");
    std::fs::create_dir_all(&src).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir.join(".cargo")).map_err(|e| e.to_string())?;
    std::fs::write(src.join("main.rs"), code).map_err(|e| e.to_string())?;
    let lib = env!("CARGO_MANIFEST_DIR");
    let pkg = l.name.to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "-");
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"tlcgen-{pkg}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n\n\
             [dependencies]\ntlc-rs = {{ path = \"{lib}\" }}\n\n\
             [profile.release]\nopt-level = 3\nlto = \"fat\"\ncodegen-units = 1\npanic = \"abort\"\n"
        ),
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(dir.join(".cargo/config.toml"), "[build]\nrustflags = [\"-C\", \"target-cpu=native\"]\n")
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::rt::G;
    use crate::value::Value;

    fn pool() -> G {
        let set = Value::Set(vec![Value::Int(1), Value::Int(2)].into());
        G { k: vec![set.clone(), Value::Seq(vec![set.clone()].into())], c: vec![Some(set), None] }
    }

    fn arc(v: &Value) -> usize {
        match v {
            Value::Set(xs) | Value::Seq(xs) => xs.as_ptr() as usize,
            _ => unreachable!(),
        }
    }

    #[test]
    fn local_is_one_private_copy_per_thread() {
        let g = pool();
        let a = g.local();
        assert!(std::ptr::eq(a, g.local()), "a thread reuses its copy");
        assert!(!std::ptr::eq(a, &g), "the copy is not the shared pool");
        assert!(a.k == g.k && a.c == g.c, "the copy holds the same constants");
        for (x, y) in a.k.iter().zip(&g.k) {
            assert_ne!(arc(x), arc(y), "a constant's count is shared with the pool");
        }
        assert_ne!(arc(a.c[0].as_ref().unwrap()), arc(g.c[0].as_ref().unwrap()));

        let (ga, other) = (a as *const G as usize, &g);
        let theirs = std::thread::scope(|s| s.spawn(|| other.local() as *const G as usize).join().unwrap());
        assert_ne!(theirs, ga, "another thread gets its own copy");

        // A second pool on this thread is not served the first one's copy.
        let h = pool();
        assert!(!std::ptr::eq(h.local(), a));
    }
}
