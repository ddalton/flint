//! A Pratt parser for the TLA+ expression language, plus module structure.
//!
//! Layout: a `/\` or `\/` bullet at column c opens a junction list, and
//! every token at column <= c ends the current item. That is the rule SANY
//! applies, implemented as a stack of column floors that `peek` respects.

use crate::ast::*;
use crate::lexer::{Tok, Token};
use std::rc::Rc;

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    floors: Vec<i64>,
    /// `LET I == INSTANCE M IN ...`: brought to module level (its names are
    /// `I!D`, which nothing else defines)
    hoisted: Vec<crate::ast::Instance>,
}

type R<T> = Result<T, String>;

fn infix(op: &str) -> Option<(u8, bool)> {
    // (binding power, right associative)
    Some(match op {
        "=>" => (1, true),
        "<=>" | "~>" => (2, false),
        "/\\" | "\\/" => (3, false),
        "=" | "#" | "/=" | "<" | ">" | "<=" | "=<" | ">=" | "\\in" | "\\notin" | "\\subseteq"
        | "\\subset" | "\\supseteq" => (5, false),
        "@@" => (6, false),
        ":>" => (7, false),
        "\\cup" | "\\cap" | "\\" => (8, false),
        ".." => (9, false),
        "+" | "-" | "\\X" => (10, false),
        "*" | "\\div" | "%" | "\\o" => (13, false),
        "^" => (14, true),
        // user-definable (TLA+'s precedences)
        "\\prec" | "\\preceq" | "\\succ" | "\\succeq" | "\\ll" | "\\gg" | "\\sqsubset" | "\\sqsubseteq"
        | "\\sqsupset" | "\\sqsupseteq" | "\\sim" | "\\simeq" | "\\approx" | "\\cong" | "\\doteq" => (5, false),
        "##" | "$$" | "\\uplus" | "\\sqcap" | "\\sqcup" => (9, false),
        "++" | "\\oplus" | "%%" => (10, false),
        "\\ominus" => (11, false),
        "&" | "&&" | "**" | "//" | "\\otimes" | "\\odot" | "\\oslash" | "\\star" | "\\bullet" => (13, false),
        "^^" => (14, false),
        _ => return None,
    })
}

/// The set of an unbounded CHOOSE (not a name a spec can write).
pub const UNBOUNDED: &str = "$unbounded";

/// `f[x \in S, y \in T] == e`. Not recursive: `f == [x \in S, y \in T |-> e]`.
/// Recursive (e mentions f), TLC evaluates it lazily, one application at
/// a time, so it becomes an operator `f!app(arg)` (arg the argument, a
/// tuple for several bounds), applications `f[a]` in e become
/// `f!app(a)`, and `f` itself the function built from it.
fn function_def(name: String, bounds: Vec<Bound>, body: Ast) -> Vec<Rc<Def>> {
    fn mentions(a: &Ast, n: &str) -> bool {
        match a {
            Ast::Ident(x) | Ast::Apply(x, _) if x == n => true,
            _ => crate::compile::children(a).into_iter().any(|c| mentions(c, n)),
        }
    }
    let arg = |bs: &[Bound]| -> Ast {
        let parts: Vec<Ast> = bs
            .iter()
            .map(|b| if b.tuple { Ast::Tuple(b.names.iter().map(|n| Ast::Ident(n.clone())).collect()) } else { Ast::Ident(b.names[0].clone()) })
            .collect();
        if parts.len() == 1 { parts.into_iter().next().unwrap() } else { Ast::Tuple(parts) }
    };
    let recursive = mentions(&body, &name);
    let app = format!("{name}!app");
    fn rewrite(a: &Ast, f: &str, app: &str, whole: &Ast) -> Ast {
        match a {
            Ast::App(g, args) if matches!(&**g, Ast::Ident(x) if x == f) => {
                let args: Vec<Ast> = args.iter().map(|x| rewrite(x, f, app, whole)).collect();
                let arg = if args.len() == 1 { args.into_iter().next().unwrap() } else { Ast::Tuple(args) };
                Ast::Apply(app.to_string(), vec![arg])
            }
            Ast::Ident(x) if x == f => whole.clone(),
            _ => crate::compile::map_children(a, &mut |c| rewrite(c, f, app, whole)),
        }
    }
    let whole = Ast::FuncCons(bounds.clone(), Box::new(Ast::Apply(app.clone(), vec![arg(&bounds)])));
    // destructure the argument into the bound names
    let a = Ast::Ident("$arg".into());
    let nth = |v: &Ast, i: usize| Ast::App(Box::new(v.clone()), vec![Ast::Num(i as i64 + 1)]);
    let mut lets = Vec::new();
    for (j, b) in bounds.iter().enumerate() {
        let part = if bounds.len() == 1 { a.clone() } else { nth(&a, j) };
        if b.tuple {
            for (i, n) in b.names.iter().enumerate() {
                lets.push(Rc::new(Def { name: n.clone(), params: vec![], op_arity: vec![], body: nth(&part, i) }));
            }
        } else {
            lets.push(Rc::new(Def { name: b.names[0].clone(), params: vec![], op_arity: vec![], body: part }));
        }
    }
    let body = if recursive { rewrite(&body, &name, &app, &whole) } else { body };
    // an argument outside the domain is an error, as in TLC (a CASE with
    // no arm that matches)
    let in_domain = Ast::And(
        bounds
            .iter()
            .enumerate()
            .map(|(j, b)| Ast::Bin("\\in", Box::new(if bounds.len() == 1 { a.clone() } else { nth(&a, j) }), Box::new(b.set.clone())))
            .collect(),
    );
    let app_body = Ast::Case(vec![(in_domain, Ast::Let(lets, Box::new(body)))], None);
    vec![
        Rc::new(Def { name: app.clone(), params: vec!["$arg".into()], op_arity: vec![0], body: app_body }),
        Rc::new(Def { name, params: vec![], op_arity: vec![], body: whole }),
    ]
}

const KEYWORDS: &[&str] = &[
    "THEN", "ELSE", "IN", "OTHER", "EXCEPT", "LET", "IF", "CASE", "CHOOSE", "MODULE", "EXTENDS",
    "CONSTANT", "CONSTANTS", "VARIABLE", "VARIABLES", "ASSUME", "THEOREM", "RECURSIVE", "LOCAL",
    "INSTANCE", "WITH", "LAMBDA",
];

impl Parser {
    pub fn new(toks: Vec<Token>) -> Self {
        Parser { toks, pos: 0, floors: vec![-1], hoisted: Vec::new() }
    }

    fn raw(&self) -> &Token {
        &self.toks[self.pos]
    }

    /// The next token, or Eof if layout ends the current junction item.
    fn peek(&self) -> &Tok {
        let t = &self.toks[self.pos];
        if (t.col as i64) <= *self.floors.last().unwrap() {
            &Tok::Eof
        } else {
            &t.tok
        }
    }

    fn peek_at(&self, k: usize) -> &Tok {
        &self.toks[(self.pos + k).min(self.toks.len() - 1)].tok
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if *o == op)
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == kw)
    }

    fn err<T>(&self, msg: &str) -> R<T> {
        let t = self.raw();
        Err(format!("line {} col {}: {msg} (at {:?})", t.line, t.col + 1, t.tok))
    }

    fn expect_op(&mut self, op: &str) -> R<()> {
        if self.is_op(op) {
            self.bump();
            Ok(())
        } else {
            self.err(&format!("expected `{op}`"))
        }
    }

    fn expect_kw(&mut self, kw: &str) -> R<()> {
        if self.is_kw(kw) {
            self.bump();
            Ok(())
        } else {
            self.err(&format!("expected `{kw}`"))
        }
    }

    fn ident(&mut self) -> R<String> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.bump();
                Ok(s)
            }
            _ => self.err("expected identifier"),
        }
    }

    // ---- module structure --------------------------------------------------

    pub fn module(&mut self) -> R<Module> {
        // Skip anything before `---- MODULE Name ----`.
        while !(matches!(self.raw().tok, Tok::Sep) && matches!(self.peek_at(1), Tok::Ident(s) if s == "MODULE"))
        {
            if matches!(self.raw().tok, Tok::Eof) {
                return self.err("no MODULE header");
            }
            self.bump();
        }
        self.bump();
        self.bump();
        let mut m = Module { name: self.ident()?, ..Default::default() };
        if matches!(self.raw().tok, Tok::Sep) {
            self.bump();
        }
        // LOCAL applies to the next declaration
        let mut local = false;
        loop {
            let t = self.raw().tok.clone();
            let was_local = std::mem::take(&mut local);
            let sym = |name: &str, var: bool| crate::ast::Decl::Sym { name: name.to_string(), local: was_local, var };
            match t {
                Tok::End | Tok::Eof => break,
                Tok::Sep => {
                    self.bump();
                }
                Tok::Ident(ref s) => match s.as_str() {
                    "EXTENDS" => {
                        self.bump();
                        m.extends = self.ident_list()?;
                    }
                    "CONSTANT" | "CONSTANTS" => {
                        self.bump();
                        for c in self.decl_list()? {
                            m.decls.push(sym(&c, false));
                            m.constants.push(c);
                        }
                    }
                    "VARIABLE" | "VARIABLES" => {
                        self.bump();
                        let vs = self.ident_list()?;
                        m.decls.extend(vs.iter().map(|v| sym(v, true)));
                        m.variables.extend(vs);
                    }
                    "ASSUME" | "ASSUMPTION" | "AXIOM" => {
                        self.bump();
                        // `ASSUME Name == e`: an assumption that is also a
                        // definition of Name (SANY's theorem node)
                        let mut name = None;
                        if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Op("==")) {
                            name = Some(self.ident()?);
                            self.bump();
                        }
                        let e = self.expr(0)?;
                        if let Some(n) = name {
                            m.decls.push(sym(&n, false));
                            m.defs.push(Rc::new(Def { name: n, params: vec![], op_arity: vec![], body: e.clone() }));
                        }
                        m.assumes.push(e);
                    }
                    // Theorems and their proofs, and proof commands, are
                    // for TLAPS; TLC ignores them, and so does tlc-rs.
                    "THEOREM" | "LEMMA" | "PROPOSITION" | "COROLLARY" | "USE" | "HIDE" | "PROOF" | "BY" | "OBVIOUS"
                    | "OMITTED" | "QED" => {
                        // `THEOREM Name == e` with e an expression: `Name!:`
                        // is e (TLC checks `ASSUME Name!:`); the proof is skipped
                        let (save, floors) = (self.pos, self.floors.clone());
                        let is_thm = matches!(s.as_str(), "THEOREM" | "LEMMA" | "PROPOSITION" | "COROLLARY");
                        if is_thm && matches!(self.peek_at(1), Tok::Ident(_)) && self.peek_at(2) == &Tok::Op("==") {
                            self.bump();
                            let name = self.ident()?;
                            self.bump();
                            if !self.is_kw("ASSUME") {
                                if let Ok(e) = self.expr(0) {
                                    m.defs.push(Rc::new(Def { name: format!("{name}!:"), params: vec![], op_arity: vec![], body: e }));
                                }
                            }
                        }
                        self.pos = save;
                        self.floors = floors;
                        self.skip_unit()
                    }
                    "RECURSIVE" => {
                        self.bump();
                        for r in self.decl_list()? {
                            m.decls.push(sym(&r, false));
                        }
                    }
                    "LOCAL" => {
                        self.bump();
                        local = true;
                    }
                    "INSTANCE" => {
                        self.bump();
                        let inst = self.instance(String::new())?;
                        m.decls.push(crate::ast::Decl::Instance { name: String::new(), module: inst.module.clone(), local: was_local });
                        m.instances.push(inst);
                    }
                    _ if matches!(self.peek_at(1), Tok::Op("==")) && matches!(self.peek_at(2), Tok::Ident(k) if k == "INSTANCE") => {
                        let name = self.ident()?;
                        self.bump();
                        self.bump();
                        let inst = self.instance(name.clone())?;
                        m.decls.push(crate::ast::Decl::Instance { name, module: inst.module.clone(), local: was_local });
                        m.instances.push(inst);
                    }
                    _ => {
                        let ds = self.defs()?;
                        m.decls.push(sym(&ds.last().unwrap().name, false));
                        m.defs.extend(ds);
                    }
                },
                // a proof step (`<1>2. ...`) left of any theorem's indent
                Tok::Op("<") if self.line_start() => self.skip_unit(),
                _ => return self.err("unexpected token at module level"),
            }
        }
        m.instances.extend(std::mem::take(&mut self.hoisted));
        Ok(m)
    }

    fn line_start(&self) -> bool {
        self.pos == 0 || self.toks[self.pos - 1].line != self.toks[self.pos].line
    }

    /// Does a module-level unit start here: a declaration keyword, or a
    /// definition `X ==`, `F(..) ==`, `f[..] ==`, `a op b ==`?
    fn unit_start(&self) -> bool {
        let at = |k: usize| &self.toks[(self.pos + k).min(self.toks.len() - 1)].tok;
        match at(0) {
            Tok::Sep | Tok::End | Tok::Eof => true,
            Tok::Ident(s) => {
                if matches!(
                    s.as_str(),
                    "VARIABLE" | "VARIABLES" | "CONSTANT" | "CONSTANTS" | "ASSUME" | "ASSUMPTION" | "AXIOM" | "THEOREM"
                        | "LEMMA" | "PROPOSITION" | "COROLLARY" | "INSTANCE" | "LOCAL" | "RECURSIVE" | "EXTENDS"
                ) {
                    return true;
                }
                match at(1) {
                    Tok::Op("==") => true,
                    Tok::Op(o @ ("(" | "[")) => {
                        let close = if *o == "(" { ")" } else { "]" };
                        let mut depth = 0;
                        for k in 1..200 {
                            match at(k) {
                                Tok::Op(x) if *x == *o => depth += 1,
                                Tok::Op(x) if *x == close => {
                                    depth -= 1;
                                    if depth == 0 {
                                        return matches!(at(k + 1), Tok::Op("=="));
                                    }
                                }
                                Tok::Eof | Tok::End => return false,
                                _ => {}
                            }
                        }
                        false
                    }
                    Tok::Op(_) => matches!((at(2), at(3)), (Tok::Ident(_), Tok::Op("=="))),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// Skips a theorem (statement and proof) or a proof command: up to the
    /// next line that starts a module-level unit no further right than the
    /// unit being skipped (a proof's `ASSUME NEW`, or a definition inside a
    /// proof, is indented under its theorem).
    fn skip_unit(&mut self) {
        let col = self.raw().col;
        self.bump();
        while !(self.line_start() && self.raw().col <= col && self.unit_start()) {
            if matches!(self.raw().tok, Tok::Eof) {
                return;
            }
            self.bump();
        }
    }

    /// After `INSTANCE`: `M [WITH x <- e, ...]`.
    fn instance(&mut self, name: String) -> R<crate::ast::Instance> {
        let module = self.ident()?;
        let mut subs = Vec::new();
        if self.is_kw("WITH") {
            self.bump();
            loop {
                let x = self.ident()?;
                self.expect_op("<-")?;
                subs.push((x, self.expr(0)?));
                if !self.is_op(",") {
                    break;
                }
                self.bump();
            }
        }
        Ok(crate::ast::Instance { name, module, subs })
    }

    fn ident_list(&mut self) -> R<Vec<String>> {
        let mut v = vec![self.ident()?];
        while self.is_op(",") {
            self.bump();
            v.push(self.ident()?);
        }
        Ok(v)
    }

    /// `A, B(_, _), C` — arities are dropped.
    fn decl_list(&mut self) -> R<Vec<String>> {
        let mut v = Vec::new();
        loop {
            v.push(self.ident()?);
            if self.is_op("(") {
                while !self.is_op(")") {
                    self.bump();
                }
                self.bump();
            }
            if !self.is_op(",") {
                return Ok(v);
            }
            self.bump();
        }
    }

    /// A definition: `F == e`, `F(x, op(_, _)) == e`, `a \prec b == e`,
    /// or a function `f[x \in S, ...] == e`, which is two when recursive
    /// (see `function_def`).
    fn defs(&mut self) -> R<Vec<Rc<Def>>> {
        // infix: `a op b == e`
        if let (Tok::Ident(a), Tok::Op(op), Tok::Ident(b), Tok::Op("==")) =
            (self.peek().clone(), self.peek_at(1).clone(), self.peek_at(2).clone(), self.peek_at(3).clone())
        {
            if infix(op).is_some() {
                for _ in 0..4 {
                    self.bump();
                }
                let body = self.expr(0)?;
                return Ok(vec![Rc::new(Def { name: op.to_string(), params: vec![a, b], op_arity: vec![0, 0], body })]);
            }
        }
        let name = self.ident()?;
        if self.is_op("[") {
            self.bump();
            let bounds = self.bounds()?;
            self.expect_op("]")?;
            self.expect_op("==")?;
            let body = self.expr(0)?;
            return Ok(function_def(name, bounds, body));
        }
        let (mut params, mut op_arity) = (Vec::new(), Vec::new());
        if self.is_op("(") {
            self.bump();
            loop {
                params.push(self.ident()?);
                // an operator parameter: `op(_, _)`
                let mut arity = 0;
                if self.is_op("(") {
                    self.bump();
                    loop {
                        match self.peek() {
                            Tok::Ident(u) if u == "_" => {
                                self.bump();
                                arity += 1;
                            }
                            _ => return self.err("expected `_` in an operator parameter"),
                        }
                        if self.is_op(",") {
                            self.bump();
                        } else {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                }
                op_arity.push(arity);
                if self.is_op(",") {
                    self.bump();
                } else {
                    break;
                }
            }
            self.expect_op(")")?;
        }
        self.expect_op("==")?;
        let body = self.expr(0)?;
        Ok(vec![Rc::new(Def { name, params, op_arity, body })])
    }

    // ---- expressions -------------------------------------------------------

    pub fn expr(&mut self, min_bp: u8) -> R<Ast> {
        let mut lhs = self.prefix()?;
        loop {
            let op = match self.peek() {
                Tok::Op(o) => *o,
                _ => break,
            };
            let Some((bp, right)) = infix(op) else { break };
            if bp < min_bp {
                break;
            }
            self.bump();
            let rhs = self.expr(if right { bp } else { bp + 1 })?;
            lhs = match op {
                "/\\" => match lhs {
                    Ast::And(mut v) => {
                        v.push(rhs);
                        Ast::And(v)
                    }
                    l => Ast::And(vec![l, rhs]),
                },
                "\\/" => match lhs {
                    Ast::Or(mut v) => {
                        v.push(rhs);
                        Ast::Or(v)
                    }
                    l => Ast::Or(vec![l, rhs]),
                },
                "\\X" => match lhs {
                    Ast::Product(mut v) => {
                        v.push(rhs);
                        Ast::Product(v)
                    }
                    l => Ast::Product(vec![l, rhs]),
                },
                "~>" => Ast::Temporal("~>", vec![lhs, rhs]),
                "/=" => Ast::Bin("#", Box::new(lhs), Box::new(rhs)),
                "=<" => Ast::Bin("<=", Box::new(lhs), Box::new(rhs)),
                _ => Ast::Bin(op, Box::new(lhs), Box::new(rhs)),
            };
        }
        Ok(lhs)
    }

    fn junction(&mut self, op: &'static str) -> R<Ast> {
        let col = self.raw().col as i64;
        let mut items = Vec::new();
        while matches!(self.raw().tok, Tok::Op(o) if o == op) && self.raw().col as i64 == col {
            self.bump();
            self.floors.push(col);
            let item = self.expr(0);
            self.floors.pop();
            items.push(item?);
        }
        Ok(if op == "/\\" { Ast::And(items) } else { Ast::Or(items) })
    }

    fn prefix(&mut self) -> R<Ast> {
        // a label (`Name::` or `Name(a, b)::`) names the expression after it
        if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Op("::")) {
            self.bump();
            self.bump();
        }
        let t = self.peek().clone();
        let e = match t {
            Tok::Op("/\\") => return self.junction("/\\"),
            Tok::Op("\\/") => return self.junction("\\/"),
            Tok::Op("~") => {
                self.bump();
                Ast::Not(Box::new(self.expr(4)?))
            }
            Tok::Op("-") => {
                self.bump();
                Ast::Neg(Box::new(self.expr(12)?))
            }
            Tok::Op(op @ ("[]" | "<>")) => {
                self.bump();
                Ast::Temporal(op, vec![self.expr(4)?])
            }
            Tok::Op("\\A") | Tok::Op("\\E") => {
                self.bump();
                let bounds = self.bounds()?;
                self.expect_op(":")?;
                let body = self.expr(0)?;
                Ast::Quant(t == Tok::Op("\\A"), bounds, Box::new(body))
            }
            Tok::Ident(ref s) => match s.as_str() {
                "IF" => {
                    self.bump();
                    let c = self.expr(0)?;
                    self.expect_kw("THEN")?;
                    let a = self.expr(0)?;
                    self.expect_kw("ELSE")?;
                    let b = self.expr(0)?;
                    return Ok(Ast::If(Box::new(c), Box::new(a), Box::new(b)));
                }
                "CASE" => {
                    self.bump();
                    let mut arms = Vec::new();
                    let mut other = None;
                    loop {
                        if self.is_kw("OTHER") {
                            self.bump();
                            self.expect_op("->")?;
                            other = Some(Box::new(self.expr(0)?));
                            break;
                        }
                        let p = self.expr(0)?;
                        self.expect_op("->")?;
                        let e = self.expr(0)?;
                        arms.push((p, e));
                        if !self.is_op("[]") {
                            break;
                        }
                        self.bump();
                    }
                    return Ok(Ast::Case(arms, other));
                }
                "LET" => {
                    self.bump();
                    let mut defs = Vec::new();
                    while !self.is_kw("IN") {
                        if self.is_kw("RECURSIVE") {
                            self.bump();
                            self.decl_list()?;
                            continue;
                        }
                        if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Op("==") && matches!(self.peek_at(2), Tok::Ident(w) if w == "INSTANCE") {
                            let name = self.ident()?;
                            self.bump();
                            self.bump();
                            let inst = self.instance(name)?;
                            // a WITH could name the LET's own parameters,
                            // which module level cannot see
                            if !inst.subs.is_empty() {
                                return self.err("INSTANCE ... WITH inside a LET is not supported");
                            }
                            self.hoisted.push(inst);
                            continue;
                        }
                        defs.extend(self.defs()?);
                    }
                    self.bump();
                    let body = self.expr(0)?;
                    return Ok(Ast::Let(defs, Box::new(body)));
                }
                "CHOOSE" => {
                    self.bump();
                    // unbounded: `CHOOSE x : P`. TLC cannot evaluate it either;
                    // it is fine as long as nothing evaluates it (a cfg
                    // usually overrides such a definition)
                    if matches!(self.peek(), Tok::Ident(_)) && matches!(self.peek_at(1), Tok::Op(":")) {
                        let x = self.ident()?;
                        self.bump();
                        let body = self.expr(0)?;
                        let b = Bound { names: vec![x], tuple: false, set: Ast::Ident(UNBOUNDED.into()) };
                        return Ok(Ast::Choose(Box::new(b), Box::new(body)));
                    }
                    let mut b = self.bounds()?;
                    self.expect_op(":")?;
                    let body = self.expr(0)?;
                    if b.len() != 1 {
                        return self.err("CHOOSE takes one bound");
                    }
                    return Ok(Ast::Choose(Box::new(b.pop().unwrap()), Box::new(body)));
                }
                "UNCHANGED" => {
                    self.bump();
                    Ast::Unchanged(Box::new(self.expr(15)?))
                }
                "SUBSET" | "UNION" => {
                    self.bump();
                    let op = if s == "SUBSET" { "SUBSET" } else { "UNION" };
                    Ast::Prefix(op, Box::new(self.expr(9)?))
                }
                "DOMAIN" => {
                    self.bump();
                    Ast::Prefix("DOMAIN", Box::new(self.expr(10)?))
                }
                "ENABLED" => {
                    self.bump();
                    Ast::Temporal("ENABLED", vec![self.expr(4)?])
                }
                "TRUE" => {
                    self.bump();
                    Ast::Bool(true)
                }
                "LAMBDA" => {
                    self.bump();
                    let ps = self.ident_list()?;
                    self.expect_op(":")?;
                    return Ok(Ast::Lambda(ps, Box::new(self.expr(0)?)));
                }
                "FALSE" => {
                    self.bump();
                    Ast::Bool(false)
                }
                _ if (s.starts_with("WF_") || s.starts_with("SF_")) => {
                    self.bump();
                    let kind = if s.starts_with("WF_") { "WF" } else { "SF" };
                    let subscript = if s.len() == 3 { self.primary_postfix()? } else { Ast::Ident(s[3..].to_string()) };
                    self.expect_op("(")?;
                    let a = self.expr(0)?;
                    self.expect_op(")")?;
                    Ast::Temporal(kind, vec![subscript, a])
                }
                _ if KEYWORDS.contains(&s.as_str()) => return self.err("unexpected keyword"),
                _ => self.primary_postfix()?,
            },
            _ => self.primary_postfix()?,
        };
        Ok(e)
    }

    fn primary_postfix(&mut self) -> R<Ast> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Tok::Op("'") => {
                    self.bump();
                    e = Ast::Prime(Box::new(e));
                }
                Tok::Op("[") => {
                    self.bump();
                    let args = self.expr_list("]")?;
                    e = Ast::App(Box::new(e), args);
                }
                Tok::Op(".") if matches!(self.peek_at(1), Tok::Ident(_)) => {
                    self.bump();
                    e = Ast::Field(Box::new(e), self.ident()?);
                }
                _ => return Ok(e),
            }
        }
    }

    /// Comma-separated expressions up to and including `close`.
    /// A subscript's name after `_` (`[Next]_EWD998!vars`): `I!Op` parts too.
    fn subscript_name(&mut self, first: &str) -> R<Ast> {
        let mut s = first.to_string();
        while self.is_op("!") && matches!(self.peek_at(1), Tok::Ident(_)) {
            self.bump();
            let t = self.ident()?;
            s = format!("{s}!{t}");
        }
        Ok(Ast::Ident(s))
    }

    fn expr_list(&mut self, close: &str) -> R<Vec<Ast>> {
        let mut v = Vec::new();
        if self.is_op(close) {
            self.bump();
            return Ok(v);
        }
        loop {
            // an infix operator as an argument (`FoldFunctionOnSet(+, 0, f, S)`):
            // the operator `LAMBDA a, b : a + b`
            if let Tok::Op(o) = self.peek().clone() {
                if close == ")" && infix(o).is_some() && matches!(self.peek_at(1), Tok::Op("," | ")")) {
                    self.bump();
                    let (a, b) = ("$l".to_string(), "$r".to_string());
                    let body = Ast::Bin(o, Box::new(Ast::Ident(a.clone())), Box::new(Ast::Ident(b.clone())));
                    v.push(Ast::Lambda(vec![a, b], Box::new(body)));
                    if self.is_op(",") {
                        self.bump();
                        continue;
                    }
                    self.expect_op(close)?;
                    return Ok(v);
                }
            }
            v.push(self.expr(0)?);
            if self.is_op(",") {
                self.bump();
            } else {
                self.expect_op(close)?;
                return Ok(v);
            }
        }
    }

    fn bounds(&mut self) -> R<Vec<Bound>> {
        let mut out = Vec::new();
        loop {
            if self.is_op("<<") {
                self.bump();
                let names = self.ident_list()?;
                self.expect_op(">>")?;
                self.expect_op("\\in")?;
                out.push(Bound { names, tuple: true, set: self.expr(0)? });
            } else {
                let names = self.ident_list()?;
                self.expect_op("\\in")?;
                let set = self.expr(0)?;
                for n in names {
                    out.push(Bound { names: vec![n], tuple: false, set: set.clone() });
                }
            }
            if !self.is_op(",") {
                return Ok(out);
            }
            self.bump();
        }
    }

    fn primary(&mut self) -> R<Ast> {
        let t = self.peek().clone();
        match t {
            Tok::Num(n) => {
                self.bump();
                Ok(Ast::Num(n))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Ast::Str(s))
            }
            Tok::Op("@") => {
                self.bump();
                Ok(Ast::At)
            }
            Tok::Op("(") => {
                self.bump();
                // Parentheses reset layout: a bullet list inside may sit left
                // of an enclosing one.
                self.floors.push(-1);
                let e = self.expr(0);
                self.floors.pop();
                self.expect_op(")")?;
                e
            }
            Tok::Op("<<") => {
                self.bump();
                let v = self.expr_list(">>")?;
                // `<<A>>_v`: the subscript lexes as an identifier starting
                // with `_`, as in `[A]_v`
                if v.len() == 1 {
                    if let Tok::Ident(s) = self.raw().tok.clone() {
                        if let Some(rest) = s.strip_prefix('_') {
                            self.bump();
                            let sub = if rest.is_empty() { self.primary_postfix()? } else { self.subscript_name(rest)? };
                            return Ok(Ast::Temporal("<<>>_", vec![v.into_iter().next().unwrap(), sub]));
                        }
                    }
                }
                Ok(Ast::Tuple(v))
            }
            Tok::Op("{") => {
                self.bump();
                self.set_expr()
            }
            Tok::Op("[") => {
                self.bump();
                self.bracket_expr()
            }
            Tok::Ident(s) => {
                self.bump();
                // `I!Op`: an operator of the instance I; `D!2`: the second
                // conjunct (or disjunct) of D's definition
                let mut s = s;
                // `Thm!:`: a named theorem's statement
                if self.is_op("!") && self.peek_at(1) == &Tok::Op(":") {
                    self.bump();
                    self.bump();
                    return Ok(Ast::Ident(format!("{s}!:")));
                }
                while self.is_op("!") && matches!(self.peek_at(1), Tok::Ident(_) | Tok::Num(_)) {
                    self.bump();
                    let t = match self.peek().clone() {
                        Tok::Num(n) => {
                            self.bump();
                            n.to_string()
                        }
                        _ => self.ident()?,
                    };
                    s = format!("{s}!{t}");
                }
                if self.is_op("(") {
                    self.bump();
                    let args = self.expr_list(")")?;
                    Ok(Ast::Apply(s, args))
                } else {
                    Ok(Ast::Ident(s))
                }
            }
            _ => self.err("expected an expression"),
        }
    }

    fn set_expr(&mut self) -> R<Ast> {
        if self.is_op("}") {
            self.bump();
            return Ok(Ast::SetEnum(vec![]));
        }
        // `{x \in S : P}` — a filter.
        if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Op("\\in") {
            let save = self.pos;
            let name = self.ident()?;
            self.bump();
            let set = self.expr(0)?;
            if self.is_op(":") {
                self.bump();
                let p = self.expr(0)?;
                self.expect_op("}")?;
                return Ok(Ast::SetFilter(Box::new(Bound { names: vec![name], tuple: false, set }), Box::new(p)));
            }
            self.pos = save;
        }
        // `{<<a, b>> \in S : P}` — a filter over tuples.
        if self.is_op("<<") {
            let save = self.pos;
            self.bump();
            let mut names = Vec::new();
            while let Tok::Ident(n) = self.peek().clone() {
                self.bump();
                names.push(n);
                if !self.is_op(",") {
                    break;
                }
                self.bump();
            }
            if !names.is_empty() && self.is_op(">>") && self.peek_at(1) == &Tok::Op("\\in") {
                self.bump();
                self.bump();
                let set = self.expr(0)?;
                if self.is_op(":") {
                    self.bump();
                    let p = self.expr(0)?;
                    self.expect_op("}")?;
                    return Ok(Ast::SetFilter(Box::new(Bound { names, tuple: true, set }), Box::new(p)));
                }
            }
            self.pos = save;
        }
        let first = self.expr(0)?;
        if self.is_op(":") {
            self.bump();
            let b = self.bounds()?;
            self.expect_op("}")?;
            return Ok(Ast::SetMap(Box::new(first), b));
        }
        let mut v = vec![first];
        while self.is_op(",") {
            self.bump();
            v.push(self.expr(0)?);
        }
        self.expect_op("}")?;
        Ok(Ast::SetEnum(v))
    }

    fn bracket_expr(&mut self) -> R<Ast> {
        if let Tok::Ident(_) = self.peek() {
            match self.peek_at(1) {
                Tok::Op("|->") => {
                    let mut fields = Vec::new();
                    loop {
                        let f = self.ident()?;
                        self.expect_op("|->")?;
                        fields.push((f, self.expr(0)?));
                        if !self.is_op(",") {
                            break;
                        }
                        self.bump();
                    }
                    self.expect_op("]")?;
                    return Ok(Ast::Record(fields));
                }
                Tok::Op(":") => {
                    let mut fields = Vec::new();
                    loop {
                        let f = self.ident()?;
                        self.expect_op(":")?;
                        fields.push((f, self.expr(0)?));
                        if !self.is_op(",") {
                            break;
                        }
                        self.bump();
                    }
                    self.expect_op("]")?;
                    return Ok(Ast::RecordSet(fields));
                }
                Tok::Op("\\in") | Tok::Op(",") => {
                    let save = self.pos;
                    if let Ok(b) = self.bounds() {
                        if self.is_op("|->") {
                            self.bump();
                            let body = self.expr(0)?;
                            self.expect_op("]")?;
                            return Ok(Ast::FuncCons(b, Box::new(body)));
                        }
                    }
                    self.pos = save;
                }
                _ => {}
            }
        }
        if self.is_op("<<") {
            // `[<<a, b>> \in S |-> e]`
            let save = self.pos;
            if let Ok(b) = self.bounds() {
                if self.is_op("|->") {
                    self.bump();
                    let body = self.expr(0)?;
                    self.expect_op("]")?;
                    return Ok(Ast::FuncCons(b, Box::new(body)));
                }
            }
            self.pos = save;
        }
        let e = self.expr(0)?;
        if self.is_kw("EXCEPT") {
            self.bump();
            let mut ups = Vec::new();
            loop {
                self.expect_op("!")?;
                let mut path = Vec::new();
                loop {
                    if self.is_op(".") {
                        self.bump();
                        path.push(PathEl::Field(self.ident()?));
                    } else if self.is_op("[") {
                        self.bump();
                        path.push(PathEl::Idx(self.expr_list("]")?));
                    } else {
                        break;
                    }
                }
                self.expect_op("=")?;
                ups.push((path, self.expr(0)?));
                if !self.is_op(",") {
                    break;
                }
                self.bump();
            }
            self.expect_op("]")?;
            return Ok(Ast::Except(Box::new(e), ups));
        }
        if self.is_op("->") {
            self.bump();
            let r = self.expr(0)?;
            self.expect_op("]")?;
            return Ok(Ast::FuncSet(Box::new(e), Box::new(r)));
        }
        self.expect_op("]")?;
        // `[A]_v`: the subscript lexes as an identifier starting with `_`.
        if let Tok::Ident(s) = self.raw().tok.clone() {
            if let Some(rest) = s.strip_prefix('_') {
                self.bump();
                let sub = if rest.is_empty() { self.primary_postfix()? } else { self.subscript_name(rest)? };
                return Ok(Ast::BoxAction(Box::new(e), Box::new(sub)));
            }
        }
        self.err("unsupported bracket expression")
    }
}

// ---- the .cfg file ---------------------------------------------------------

#[derive(Debug, Clone)]
pub enum CfgVal {
    Int(i64),
    Str(String),
    Bool(bool),
    Model(String),
    Set(Vec<CfgVal>),
    Tuple(Vec<CfgVal>),
}

#[derive(Default, Debug)]
pub struct Cfg {
    pub constants: Vec<(String, CfgVal)>,
    pub substitutions: Vec<(String, String)>,
    /// `x <-[M] y`: (M, x, y), overriding M's definition x wherever M is
    /// instantiated
    pub scoped_substitutions: Vec<(String, String, String)>,
    pub init: Option<String>,
    pub next: Option<String>,
    pub spec: Option<String>,
    pub invariants: Vec<String>,
    pub constraints: Vec<String>,
    pub action_constraints: Vec<String>,
    pub properties: Vec<String>,
    pub symmetry: Option<String>,
    pub view: Option<String>,
    pub check_deadlock: bool,
}

pub fn parse_cfg(toks: Vec<Token>) -> R<Cfg> {
    let mut p = Parser::new(toks);
    let mut c = Cfg { check_deadlock: true, ..Default::default() };
    let section_words = [
        "CONSTANT", "CONSTANTS", "INIT", "NEXT", "SPECIFICATION", "INVARIANT", "INVARIANTS",
        "PROPERTY", "PROPERTIES", "CONSTRAINT", "CONSTRAINTS", "ACTION_CONSTRAINT",
        "ACTION_CONSTRAINTS", "SYMMETRY", "VIEW", "CHECK_DEADLOCK", "ALIAS", "POSTCONDITION",
    ];
    let mut section = String::new();
    loop {
        let t = p.raw().tok.clone();
        match t {
            Tok::Eof => break,
            Tok::Ident(s) if section_words.contains(&s.as_str()) => {
                p.bump();
                section = s;
                if section == "CHECK_DEADLOCK" {
                    c.check_deadlock = p.ident()? == "TRUE";
                }
            }
            Tok::Ident(name) => {
                p.bump();
                match section.as_str() {
                    "CONSTANT" | "CONSTANTS" => {
                        // `<-[M]` / `= [M]v`: scoped to module M
                        let scope = |p: &mut Parser| -> Option<String> {
                            if p.is_op("[") && matches!(p.peek_at(2), Tok::Op("]")) {
                                if let Tok::Ident(m) = p.peek_at(1).clone() {
                                    p.bump();
                                    p.bump();
                                    p.bump();
                                    return Some(m);
                                }
                            }
                            None
                        };
                        if p.is_op("<-") {
                            p.bump();
                            match scope(&mut p) {
                                Some(m) => c.scoped_substitutions.push((m, name, p.ident()?)),
                                None => c.substitutions.push((name, p.ident()?)),
                            }
                        } else {
                            p.expect_op("=")?;
                            // a constant is one name however it is reached:
                            // the scope changes nothing tlc-rs resolves
                            scope(&mut p);
                            c.constants.push((name, cfg_val(&mut p)?));
                        }
                    }
                    "INIT" => c.init = Some(name),
                    "NEXT" => c.next = Some(name),
                    "SPECIFICATION" => c.spec = Some(name),
                    "INVARIANT" | "INVARIANTS" => c.invariants.push(name),
                    "PROPERTY" | "PROPERTIES" => c.properties.push(name),
                    "CONSTRAINT" | "CONSTRAINTS" => c.constraints.push(name),
                    "ACTION_CONSTRAINT" | "ACTION_CONSTRAINTS" => c.action_constraints.push(name),
                    "SYMMETRY" => c.symmetry = Some(name),
                    "VIEW" => c.view = Some(name),
                    _ => {}
                }
            }
            _ => return p.err("unexpected token in cfg"),
        }
    }
    Ok(c)
}

fn cfg_val(p: &mut Parser) -> R<CfgVal> {
    match p.raw().tok.clone() {
        Tok::Num(n) => {
            p.bump();
            Ok(CfgVal::Int(n))
        }
        Tok::Op("-") => {
            p.bump();
            match p.bump() {
                Tok::Num(n) => Ok(CfgVal::Int(-n)),
                _ => p.err("expected number"),
            }
        }
        Tok::Str(s) => {
            p.bump();
            Ok(CfgVal::Str(s))
        }
        Tok::Ident(s) => {
            p.bump();
            Ok(match s.as_str() {
                "TRUE" => CfgVal::Bool(true),
                "FALSE" => CfgVal::Bool(false),
                _ => CfgVal::Model(s),
            })
        }
        Tok::Op(o @ ("{" | "<<")) => {
            p.bump();
            let close = if o == "{" { "}" } else { ">>" };
            let mut v = Vec::new();
            while !p.is_op(close) {
                v.push(cfg_val(p)?);
                if p.is_op(",") {
                    p.bump();
                }
            }
            p.bump();
            Ok(if o == "{" { CfgVal::Set(v) } else { CfgVal::Tuple(v) })
        }
        _ => p.err("bad cfg value"),
    }
}
