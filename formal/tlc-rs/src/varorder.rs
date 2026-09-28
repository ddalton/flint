//! TLC's variable order, derived instead of passed with `-var-order`.
//!
//! Under SYMMETRY, TLC picks the least permuted image of a state comparing
//! variables in the order of its `vars` array, which is
//! `ModuleNode.getVariableDecls()`: SANY's module `Context` is a
//! `java.util.Hashtable` from names to symbols, and the variables come out
//! in that table's enumeration order. So this rebuilds the table:
//!  - the insertion sequence, as SANY's `Generator` makes it: SANY's 72
//!    built-in operators (every context starts as a copy of them); each
//!    EXTENDS module's non-local symbols in the order they entered its
//!    own context (`mergeExtendContext`), skipping names already present;
//!    then the module's own declarations in source order. A named
//!    `I == INSTANCE M` adds `I!D` for each non-local definition D of M,
//!    in M's table's enumeration order (`getByClass`), then `I`; a bare
//!    INSTANCE adds D itself;
//!  - `java.util.Hashtable` exactly: capacity 11, load factor 0.75,
//!    rehash to 2n+1 at `count >= threshold`, a new entry at the head of
//!    its bucket, `elements()` from the last bucket down;
//!  - keys hash as `String.hashCode()` of the name;
//!  - `ModuleNode.getVariableDecls()` reverses the enumeration.
//! The standard modules' symbols are those of the tla2tools.jar the gates
//! use (v1.7.4); a module this does not know makes the order unknown.

use crate::ast::Decl;
use std::collections::HashMap;

/// Java's `String.hashCode()` (over UTF-16 units).
fn java_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

#[derive(Clone)]
struct Sym {
    name: String,
    local: bool,
    var: bool,
    /// a user definition (or instance): what INSTANCE copies; not a
    /// built-in
    def: bool,
}

/// `java.util.Hashtable`, keeping insertion order besides.
#[derive(Clone)]
struct Table {
    /// buckets, each chain head first
    tab: Vec<Vec<usize>>,
    count: usize,
    threshold: usize,
    syms: Vec<Sym>,
    index: HashMap<String, usize>,
}

impl Table {
    fn new() -> Table {
        Table { tab: vec![Vec::new(); 11], count: 0, threshold: 8, syms: Vec::new(), index: HashMap::new() }
    }

    fn bucket(name: &str, cap: usize) -> usize {
        ((java_hash(name) & 0x7FFF_FFFF) as usize) % cap
    }

    /// Adds `s` unless its name is already present (SANY never replaces).
    fn put(&mut self, s: Sym) {
        if self.index.contains_key(&s.name) {
            return;
        }
        if self.count >= self.threshold {
            self.rehash();
        }
        let i = self.syms.len();
        let b = Self::bucket(&s.name, self.tab.len());
        self.index.insert(s.name.clone(), i);
        self.syms.push(s);
        self.tab[b].insert(0, i);
        self.count += 1;
    }

    fn rehash(&mut self) {
        let cap = self.tab.len() * 2 + 1;
        self.threshold = (cap as f64 * 0.75) as usize;
        let mut new: Vec<Vec<usize>> = vec![Vec::new(); cap];
        for old in self.tab.iter().rev() {
            for &e in old {
                let b = Self::bucket(&self.syms[e].name, cap);
                new[b].insert(0, e);
            }
        }
        self.tab = new;
    }

    /// `elements()`: the last bucket first, each chain from its head.
    fn elements(&self) -> impl Iterator<Item = &Sym> {
        self.tab.iter().rev().flat_map(|c| c.iter().map(|&e| &self.syms[e]))
    }
}

/// A module as SANY sees it: what it extends, and its declarations.
pub struct ModuleDecls {
    pub extends: Vec<String>,
    pub decls: Vec<Decl>,
}

/// SANY's built-in operators, in the order they entered its initial
/// context (read from SANY v1.7.4's own context of a root module). Every
/// module's context starts as a copy of it (`Context.duplicate`, which
/// puts them newest first).
const BUILTINS: [&str; 72] = ["STRING", "FALSE", "TRUE", "BOOLEAN", "=", "/=", ".", "'", "\\lnot", "\\neg", "\\land", "\\lor", "\\equiv", "=>", "SUBSET", "UNION", "DOMAIN", "\\subseteq", "\\in", "\\notin", "\\", "\\intersect", "\\union", "\\times", "~>", "[]", "<>", "ENABLED", "UNCHANGED", "\\cdot", "-+->", "$AngleAct", "$BoundedChoose", "$BoundedExists", "$BoundedForall", "$CartesianProd", "$Case", "$ConjList", "$DisjList", "$Except", "$FcnApply", "$FcnConstructor", "$IfThenElse", "$NonRecursiveFcnSpec", "$Pair", "$RcdConstructor", "$RcdSelect", "$RecursiveFcnSpec", "$Seq", "$SetEnumerate", "$SetOfAll", "$SetOfFcns", "$SetOfRcds", "$SF", "$SquareAct", "$SubsetOf", "$TemporalExists", "$TemporalForall", "$TemporalWhile", "$Tuple", "$UnboundedChoose", "$UnboundedExists", "$UnboundedForall", "$WF", "$Nop", "$Qed", "$Pfcase", "$Have", "$Take", "$Pick", "$Witness", "$Suffices"];

fn std_module(name: &str) -> Option<ModuleDecls> {
    fn defs(ns: &[&str]) -> Vec<Decl> {
        ns.iter().map(|n| Decl::Sym { name: n.to_string(), local: false, var: false }).collect()
    }
    let local_inst = |m: &str| Decl::Instance { name: String::new(), module: m.into(), local: true };
    Some(match name {
        "Naturals" => ModuleDecls {
            extends: vec![],
            decls: defs(&["Nat", "+", "-", "*", "^", "<", ">", "\\leq", "\\geq", "%", "\\div", ".."]),
        },
        "Integers" => ModuleDecls { extends: vec!["Naturals".into()], decls: defs(&["Int", "-."]) },
        "Sequences" => ModuleDecls {
            extends: vec![],
            decls: std::iter::once(local_inst("Naturals"))
                .chain(defs(&["Seq", "Len", "\\o", "Append", "Head", "Tail", "SubSeq", "SelectSeq"]))
                .collect(),
        },
        "FiniteSets" => ModuleDecls {
            extends: vec![],
            decls: [local_inst("Naturals"), local_inst("Sequences")].into_iter().chain(defs(&["IsFiniteSet", "Cardinality"])).collect(),
        },
        "TLC" => ModuleDecls {
            extends: vec![],
            decls: [local_inst("Naturals"), local_inst("Sequences"), local_inst("FiniteSets")]
                .into_iter()
                .chain(defs(&[
                    "Print", "PrintT", "Assert", "JavaTime", "TLCGet", "TLCSet", ":>", "@@", "Permutations", "SortSeq",
                    "RandomElement", "Any", "ToString", "TLCEval",
                ]))
                .collect(),
        },
        _ => return None,
    })
}

struct Builder<'a> {
    user: &'a HashMap<String, ModuleDecls>,
    done: HashMap<String, Table>,
}

impl Builder<'_> {
    fn context(&mut self, name: &str) -> Result<Table, String> {
        if let Some(t) = self.done.get(name) {
            return Ok(t.clone());
        }
        let std;
        let m = match self.user.get(name) {
            Some(m) => m,
            None => {
                std = std_module(name).ok_or(format!("module {name} is not known to the variable-order model"))?;
                &std
            }
        };
        let mut t = Table::new();
        for b in BUILTINS.iter().rev() {
            t.put(Sym { name: b.to_string(), local: false, var: false, def: false });
        }
        for e in &m.extends {
            let et = self.context(e)?;
            // mergeExtendContext: in the order they entered e's context
            for s in et.syms.iter().filter(|s| !s.local) {
                t.put(s.clone());
            }
        }
        for d in &m.decls {
            match d {
                Decl::Sym { name, local, var } => t.put(Sym { name: name.clone(), local: *local, var: *var, def: !var }),
                Decl::Instance { name: iname, module, local } => {
                    let it = self.context(module)?;
                    let copied: Vec<String> = it.elements().filter(|s| s.def && !s.local).map(|s| s.name.clone()).collect();
                    for n in copied {
                        let n = if iname.is_empty() { n } else { format!("{iname}!{n}") };
                        t.put(Sym { name: n, local: *local, var: false, def: true });
                    }
                    if !iname.is_empty() {
                        t.put(Sym { name: iname.clone(), local: *local, var: false, def: true });
                    }
                }
            }
        }
        self.done.insert(name.to_string(), t.clone());
        Ok(t)
    }
}

/// The root module's variables in TLC's order.
pub fn tlc_order(root: &str, modules: &HashMap<String, ModuleDecls>) -> Result<Vec<String>, String> {
    let mut b = Builder { user: modules, done: HashMap::new() };
    let t = b.context(root)?;
    if std::env::var_os("TLCRS_VARORDER_DEBUG").is_some() {
        eprintln!("varorder: {root}: {} symbols, capacity {}", t.count, t.tab.len());
        for s in &t.syms {
            eprintln!("  {}{}", s.name, if s.local { " (local)" } else { "" });
        }
    }
    // ModuleNode.getVariableDecls reverses the context's enumeration
    let mut v: Vec<String> = t.elements().filter(|s| s.var).map(|s| s.name.clone()).collect();
    v.reverse();
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_string_hash() {
        // values from Java's "…".hashCode()
        assert_eq!(java_hash(""), 0);
        assert_eq!(java_hash("a"), 97);
        assert_eq!(java_hash("hello"), 99162322);
        assert_eq!(java_hash("polygenelubricants"), i32::MIN);
    }
}
