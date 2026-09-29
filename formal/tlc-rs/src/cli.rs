//! The command line, shared by `tlc-rs` and every generated checker.
//!
//!   tlc-rs [-workers N] [-engine interp|closure] [-codegen DIR] [-config X.cfg]
//!          [-metadir DIR] [-checkpoint MIN] [-recover DIR] [-queue-mem MB] [-fpmem MB] X.tla
//!
//! Disk: a BFS level (kept serialized) beyond `-queue-mem` (default 256 MB)
//! spills to the metadir (default `states/<spec>-<time>` beside the spec),
//! and every `-checkpoint` minutes (default 30; 0 = never) the search is
//! checkpointed there at a level boundary. `-recover DIR` resumes from
//! that checkpoint. Past `-fpmem` (default 1024 MB) the seen set's shards
//! spill to sorted files there too. A finished run removes what it wrote.
//!
//! Invariants, state constraints, deadlock, SYMMETRY, VIEW, and temporal
//! PROPERTYs in the shapes the gates use (`[][A]_v`, `P ~> Q`,
//! `[](P => <>Q)`, `<>[]P`, `[]<>P`) under WF/SF fairness. Anything else
//! is refused rather than ignored.

use crate::eval::{Engine, Program};
use crate::{ast, check, closure, codegen, compile, lexer, parser};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

/// Built in. TLAPS and the proof libraries that extend it define only
/// proof-checker pseudo-operators, which a model never evaluates.
const STANDARD: &[&str] = &[
    "Naturals", "Integers", "Sequences", "FiniteSets", "TLC", "TLAPS", "NaturalsInduction", "WellFoundedInduction",
    "FiniteSetTheorems", "SequenceTheorems", "SequencesExtTheorems", "FunctionTheorems",
];

/// What a generated checker brings: the hash of the sources it was
/// generated from, and the constructor for its engine.
pub struct Generated {
    pub source_hash: u64,
    pub make: fn(&Program) -> Box<dyn Engine + '_>,
}

/// FNV-1a: stable across builds and platforms, unlike std's hasher.
fn fnv(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= *b as u64;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

/// The order TLC interns names in (checked against TLC 1.7.4's own
/// `UniqueString` tokens): the cfg's values as it reads the cfg; then
/// identifiers as SANY lexes each module, a module before those it
/// extends; then string literals, which are interned only when semantic
/// analysis builds their values — text order, a module after those it
/// extends.
#[derive(Default)]
struct InternOrder {
    idents: Vec<String>,
    strings: Vec<String>,
}

/// Standard modules TLC defines in TLA+ (tla2tools' StandardModules),
/// used when no file of that name is found.
const EMBEDDED: &[(&str, &str)] = &[("Bags", include_str!("../modules/Bags.tla"))];

/// Directories searched for a module not beside the spec.
static LIB_DIRS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();

fn load(
    dir: &Path,
    name: &str,
    out: &mut Vec<ast::Module>,
    hash: &mut u64,
    order: &mut InternOrder,
    reg: &mut HashMap<String, crate::varorder::ModuleDecls>,
) -> Result<(), String> {
    if out.iter().any(|m| m.name == name) || STANDARD.contains(&name) {
        return Ok(());
    }
    // the spec's directory, then the library directories (`-lib`,
    // TLCRS_LIB): where TLC's classpath finds CommunityModules
    let file = format!("{name}.tla");
    let found = std::iter::once(dir.to_path_buf())
        .chain(LIB_DIRS.get().into_iter().flatten().cloned())
        .map(|d| d.join(&file))
        .find(|p| p.is_file());
    let path = found.clone().unwrap_or_else(|| dir.join(&file));
    // a standard module TLC defines in TLA+ itself, built in as its source
    let src = match (found, EMBEDDED.iter().find(|(n, _)| *n == name)) {
        (None, Some((_, src))) => src.to_string(),
        _ => std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?,
    };
    fnv(hash, src.as_bytes());
    let toks = lexer::lex_module(&src).map_err(|e| format!("{}: {e}", path.display()))?;
    order.idents.extend(toks.iter().filter_map(|t| match &t.tok {
        lexer::Tok::Ident(s) => Some(s.clone()),
        _ => None,
    }));
    let strings: Vec<String> = toks
        .iter()
        .filter_map(|t| match &t.tok {
            lexer::Tok::Str(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let mut m = parser::Parser::new(toks).module().map_err(|e| format!("{}: {e}", path.display()))?;
    reg.insert(m.name.clone(), crate::varorder::ModuleDecls { extends: m.extends.clone(), decls: m.decls.clone() });
    for e in m.extends.clone() {
        load(dir, &e, out, hash, order, reg)?;
    }
    // An instantiated module is loaded apart (its names are not this
    // module's) and brought in as renamed, substituted definitions.
    for inst in m.instances.clone() {
        let mut group = Vec::new();
        load(dir, &inst.module, &mut group, hash, order, reg)?;
        let x = crate::instance::expand(&inst, &group).map_err(|e| format!("{}: {e}", path.display()))?;
        m.defs.extend(x.defs);
        m.assumes.extend(x.assumes);
        m.origins.extend(x.origins);
    }
    order.strings.extend(strings);
    out.push(m);
    Ok(())
}

pub struct Loaded {
    pub name: String,
    /// every module read, as SANY would see it: for TLC's variable order
    pub decls: HashMap<String, crate::varorder::ModuleDecls>,
    pub prog: Program,
    pub source_hash: u64,
    pub cfg_path: PathBuf,
    pub spec_path: PathBuf,
}

pub fn load_spec(spec_path: &Path, cfg_path: &Path) -> Result<Loaded, String> {
    let dir = spec_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = spec_path.file_stem().unwrap().to_string_lossy().to_string();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    // TLC reads the cfg before it parses the spec, and the cfg's string and
    // model values are interned as it reads them.
    let cfg_src = std::fs::read_to_string(cfg_path).map_err(|e| format!("{}: {e}", cfg_path.display()))?;
    let mut cfg = parser::parse_cfg(lexer::lex(&cfg_src)?)?;
    let mut order = Vec::new();
    fn cfg_names(v: &parser::CfgVal, order: &mut Vec<String>) {
        match v {
            parser::CfgVal::Str(s) | parser::CfgVal::Model(s) => order.push(s.clone()),
            parser::CfgVal::Set(xs) | parser::CfgVal::Tuple(xs) => xs.iter().for_each(|x| cfg_names(x, order)),
            _ => {}
        }
    }
    cfg.constants.iter().for_each(|(_, v)| cfg_names(v, &mut order));
    let mut modules = Vec::new();
    let mut spec_order = InternOrder::default();
    let mut reg = HashMap::new();
    load(&dir, &name, &mut modules, &mut hash, &mut spec_order, &mut reg)?;
    order.extend(spec_order.idents);
    order.extend(spec_order.strings);
    // `x <-[M] y`: M's definition x, under every name it was brought in as
    for (m, x, y) in cfg.scoped_substitutions.clone() {
        let mut names: Vec<String> = Vec::new();
        for md in &modules {
            if md.name == m && md.defs.iter().any(|d| d.name == x && !md.origins.contains_key(&x)) {
                names.push(x.clone());
            }
            names.extend(md.origins.iter().filter(|(_, o)| o.0 == m && o.1 == x).map(|(n, _)| n.clone()));
        }
        names.sort();
        names.dedup();
        // x not M's own definition (`Nat <-[ZSequences] ZSeqNat`: a built-in
        // M sees through EXTENDS): modules are flattened, so everywhere
        if names.is_empty() {
            names.push(x.clone());
        }
        cfg.substitutions.extend(names.into_iter().map(|n| (n, y.clone())));
    }
    fnv(&mut hash, cfg_src.as_bytes());

    if !cfg.action_constraints.is_empty() {
        return Err("ACTION_CONSTRAINT is not supported".into());
    }
    let mut c = compile::Compiler::new(&modules, &cfg, &order)?;
    let (init_ast, next_ast, fair_asts) = match (&cfg.spec, &cfg.init, &cfg.next) {
        (Some(s), _, _) => c.spec_parts_fair(s)?,
        (None, Some(i), Some(n)) => (ast::Ast::Ident(i.clone()), ast::Ast::Ident(n.clone()), vec![]),
        // no behavior: TLC checks the assumptions and explores nothing
        (None, None, None) => (ast::Ast::Bool(false), ast::Ast::Bool(false), vec![]),
        _ => return Err("the cfg names neither SPECIFICATION nor INIT/NEXT".into()),
    };
    let init = c.rooted_act("Init", &init_ast, true)?;
    let next = c.rooted_act("Next", &next_ast, false)?;
    let invariants = cfg.invariants.iter().map(|n| c.rooted_expr(n)).collect::<Result<_, _>>()?;
    let constraints = cfg.constraints.iter().map(|n| c.rooted_expr(n)).collect::<Result<_, _>>()?;
    let assumes: Vec<ast::Ast> = modules.iter().flat_map(|m| m.assumes.iter().cloned()).collect();
    let symmetry = cfg.symmetry.as_ref().map(|n| c.rooted_expr(n)).transpose()?;
    let view = cfg.view.as_ref().map(|n| c.rooted_expr(n)).transpose()?;
    let properties = cfg.properties.iter().map(|n| c.rooted_property(n)).collect::<Result<Vec<_>, _>>()?;
    let fairness = if cfg.properties.is_empty() { None } else { c.rooted_fairness(&fair_asts)? };
    let mut prog = c.finish(init, next, invariants, constraints, &assumes, cfg.check_deadlock, symmetry, view)?;
    prog.properties = properties;
    prog.fairness = fairness;
    Ok(Loaded { name, decls: reg, prog, source_hash: hash, cfg_path: cfg_path.into(), spec_path: spec_path.into() })
}

fn run(generated: Option<Generated>) -> Result<bool, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let (mut cfg_path, mut spec_path, mut engine, mut codegen_dir) = (None, None, "interp".to_string(), None);
    let mut var_order: Option<String> = None;
    let mut print_var_order = false;
    let (mut metadir, mut recover, mut checkpoint_min, mut queue_mb): (Option<PathBuf>, bool, f64, u64) = (None, false, 30.0, 256);
    // TLCRS_FPMEM_MB: the default -fpmem, for forcing spills in tests
    let mut fp_mb: u64 = std::env::var("TLCRS_FPMEM_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(1024);
    let mut lib_dirs: Vec<PathBuf> = std::env::var_os("TLCRS_LIB").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-lib" => {
                i += 1;
                lib_dirs.push(PathBuf::from(args.get(i).ok_or("-lib needs a directory")?));
            }
            "-workers" => {
                i += 1;
                let w = &args[i];
                if w != "auto" {
                    workers = w.parse().map_err(|_| "bad -workers")?;
                }
            }
            "-config" => {
                i += 1;
                cfg_path = Some(PathBuf::from(&args[i]));
            }
            "-engine" => {
                i += 1;
                engine = args[i].clone();
            }
            "-print-var-order" => print_var_order = true,
            "-var-order" => {
                i += 1;
                var_order = Some(args[i].clone());
            }
            "-metadir" => {
                i += 1;
                metadir = Some(PathBuf::from(&args[i]));
            }
            "-recover" => {
                i += 1;
                metadir = Some(PathBuf::from(&args[i]));
                recover = true;
            }
            "-checkpoint" => {
                i += 1;
                checkpoint_min = args[i].parse().map_err(|_| "bad -checkpoint (minutes)")?;
            }
            "-fpmem" => {
                i += 1;
                fp_mb = args[i].parse().map_err(|_| "bad -fpmem (MB)")?;
            }
            "-queue-mem" => {
                i += 1;
                queue_mb = args[i].parse().map_err(|_| "bad -queue-mem (MB)")?;
            }
            "-codegen" => {
                i += 1;
                codegen_dir = Some(PathBuf::from(&args[i]));
            }
            a if a.ends_with(".tla") => spec_path = Some(PathBuf::from(a)),
            a => return Err(format!("unknown argument {a}")),
        }
        i += 1;
    }
    let spec_path = spec_path.ok_or("usage: tlc-rs [-workers N] [-engine interp|closure] [-codegen DIR] [-lib DIR]... [-config X.cfg] X.tla")?;
    let cfg_path = cfg_path.unwrap_or_else(|| spec_path.with_extension("cfg"));

    let t0 = Instant::now();
    let _ = LIB_DIRS.set(lib_dirs);
    let mut l = load_spec(&spec_path, &cfg_path)?;
    if let Some(order) = var_order {
        let names: Vec<&str> = order.split(',').map(str::trim).collect();
        let idx: Vec<usize> = names
            .iter()
            .map(|n| l.prog.vars.iter().position(|v| v == n).ok_or(format!("-var-order: unknown variable {n}")))
            .collect::<Result<_, _>>()?;
        if idx.len() != l.prog.vars.len() {
            return Err("-var-order must list every variable once".into());
        }
        l.prog.cmp_order = idx;
    } else if !l.prog.symmetry.is_empty() || print_var_order {
        // TLC's order, derived: see varorder.rs
        match crate::varorder::tlc_order(&l.name, &l.decls) {
            Ok(names) if names.len() == l.prog.vars.len() => {
                l.prog.cmp_order = names.iter().map(|n| l.prog.vars.iter().position(|v| v == n).unwrap()).collect();
            }
            Ok(names) => return Err(format!("variable-order model found {} variables, the spec has {}", names.len(), l.prog.vars.len())),
            Err(e) => {
                eprintln!("tlc-rs: TLC's variable order is unknown ({e}); declaration order is used, so SYMMETRY counts may differ from TLC's. Pass -var-order.");
            }
        }
    }
    if print_var_order {
        let names: Vec<&str> = l.prog.cmp_order.iter().map(|&i| l.prog.vars[i].as_str()).collect();
        println!("{}", names.join(","));
        return Ok(true);
    }
    let prog = &l.prog;

    if std::env::var_os("TLCRS_DUMP_NAMES").is_some() {
        // name, then the rank tlc-rs gives it: for checking TLC's token order
        for (id, n) in crate::value::NAMES.get().unwrap().iter().enumerate() {
            println!("{n}\t{id}");
        }
        return Ok(true);
    }
    if let Some(dir) = codegen_dir {
        codegen::generate(&l, &dir)?;
        println!("tlc-rs: generated a checker for {} in {} ({:.3}s)", l.name, dir.display(), t0.elapsed().as_secs_f64());
        return Ok(true);
    }

    let closures;
    let gen_engine;
    let (e, engine_name): (&dyn Engine, &str) = match &generated {
        Some(g) => {
            if g.source_hash != l.source_hash {
                return Err(format!(
                    "this checker was generated from different sources than {} + {}; regenerate it",
                    spec_path.display(),
                    cfg_path.display()
                ));
            }
            gen_engine = (g.make)(prog);
            (&*gen_engine, "generated")
        }
        None => match engine.as_str() {
            "interp" => (prog, "interp"),
            "closure" => {
                closures = closure::compile(prog)?;
                (&closures, "closure")
            }
            e => return Err(format!("unknown engine {e}")),
        },
    };
    println!(
        "tlc-rs: {} ({} variables, {} symmetry permutations), {} workers, {} engine; loaded in {:.3}s",
        l.name,
        prog.vars.len(),
        prog.symmetry.len(),
        workers,
        engine_name,
        t0.elapsed().as_secs_f64()
    );

    let t1 = Instant::now();
    let checker = check::Checker::new(prog, e, workers)?;
    if let Some(dump) = std::env::var_os("TLCRS_KEYS") {
        return keys_of_dump(&checker, Path::new(&dump), None);
    }
    let mut checker = checker;
    if let Some(dump) = std::env::var_os("TLCRS_REFERENCE") {
        let mut keys = std::collections::HashSet::new();
        keys_of_dump(&checker, Path::new(&dump), Some(&mut keys))?;
        checker.reference = Some(keys);
    }
    if recover {
        crate::store::check_recoverable(metadir.as_ref().unwrap(), l.source_hash)?;
    }
    checker.disk = check::Disk {
        metadir: metadir.unwrap_or_else(|| {
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            spec_path.parent().unwrap_or(Path::new(".")).join("states").join(format!("{}-tlcrs-{secs}-{}", l.name, std::process::id()))
        }),
        checkpoint_secs: (checkpoint_min * 60.0) as u64,
        queue_mem: queue_mb << 20,
        fp_mem: fp_mb << 20,
        recover,
        source_hash: l.source_hash,
    };
    let checker = checker;
    let out = checker.run();
    let secs = t1.elapsed().as_secs_f64();
    let ok = match &out.failure {
        None => {
            println!("Model checking completed. No error has been found.");
            true
        }
        Some(check::Failure::Invariant(inv, _)) => {
            println!("Error: Invariant {inv} is violated.");
            false
        }
        Some(check::Failure::Deadlock(_)) => {
            println!("Error: Deadlock reached.");
            false
        }
        Some(check::Failure::Eval(e, _)) => {
            println!("Error: {e}");
            false
        }
        Some(check::Failure::Step(name, _, _)) => {
            println!("Error: Action property {name} is violated by the last step above.");
            false
        }
        Some(check::Failure::Liveness(_)) => false,
        Some(check::Failure::InitProperty(name, _)) => {
            println!("Error: Property {name} is violated by the initial state above.");
            false
        }
    };
    println!(
        "{} states generated, {} distinct states found. Depth {}. Checked in {:.3}s ({:.0} distinct/s).",
        out.generated,
        out.distinct,
        // TLC's depth of an empty search is 0
        if out.distinct == 0 { 0 } else { out.depth },
        secs,
        out.distinct as f64 / secs
    );
    Ok(ok)
}

pub fn main(generated: Option<Generated>) -> ExitCode {
    match run(generated) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(12),
        Err(e) if e == crate::compile::ASSUME_FALSE => {
            println!("Error: {e}.");
            ExitCode::from(10)
        }
        Err(e) => {
            eprintln!("tlc-rs: {e}");
            ExitCode::from(2)
        }
    }
}

/// Diagnostic: read a TLC `-dump` file, compute tlc-rs's key for every
/// state in it, and report states TLC kept apart that tlc-rs would merge.
fn keys_of_dump(checker: &check::Checker, path: &Path, mut out: Option<&mut std::collections::HashSet<u64>>) -> Result<bool, String> {
    use crate::value::Value;
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let names = crate::value::NAMES.get().unwrap();
    let id = |n: &str| names.iter().position(|x| x == n).map(|i| i as u32);
    fn lit(a: &ast::Ast, id: &dyn Fn(&str) -> Option<u32>) -> Result<Value, String> {
        use ast::Ast::*;
        Ok(match a {
            Num(n) => Value::Int(*n),
            Neg(x) => Value::Int(-lit(x, id)?.as_int()?),
            Bool(b) => Value::Bool(*b),
            Str(s) => Value::Str(id(s).ok_or(format!("unknown string {s}"))?),
            Ident(n) => Value::Model(id(n).ok_or(format!("unknown model value {n}"))?),
            SetEnum(v) => Value::set(v.iter().map(|x| lit(x, id)).collect::<Result<_, _>>()?)?,
            Tuple(v) => Value::Seq(v.iter().map(|x| lit(x, id)).collect::<Result<Vec<_>, _>>()?.into()),
            Record(fs) => Value::func(
                fs.iter().map(|(f, e)| Ok((Value::Str(id(f).ok_or(format!("unknown field {f}"))?), lit(e, id)?))).collect::<Result<_, String>>()?,
            )?,
            Bin(":>", k, v) => Value::func(vec![(lit(k, id)?, lit(v, id)?)])?,
            Bin("@@", f, g) => {
                let mut p = lit(f, id)?.pairs()?;
                p.extend(lit(g, id)?.pairs()?);
                Value::func(p)?
            }
            other => return Err(format!("not a literal: {other:?}")),
        })
    }
    let vars = &checker.p.vars;
    let mut seen: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
    let mut states: Vec<&str> = Vec::new();
    let mut collisions = 0;
    for block in text.split("\nState ").skip(0) {
        let Some(body) = block.split_once(":\n").map(|(_, b)| b) else { continue };
        let src = format!("---- MODULE D ----\nS == {body}\n====");
        let m = parser::Parser::new(lexer::lex(&src)?).module()?;
        let ast::Ast::And(conj) = &m.defs[0].body else { return Err("state is not a conjunction".into()) };
        let mut st = vec![Value::Bool(false); vars.len()];
        for c in conj {
            let ast::Ast::Bin("=", v, e) = c else { return Err("expected var = value".into()) };
            let ast::Ast::Ident(v) = &**v else { return Err("expected a variable".into()) };
            let i = vars.iter().position(|x| x == v).ok_or(format!("unknown variable {v}"))?;
            st[i] = lit(e, &id)?;
        }
        let k = checker.fp(&st);
        if let Some(o) = out.as_deref_mut() {
            o.insert(k);
        }
        let n = states.len();
        states.push(body);
        if let Some(&j) = seen.get(&k) {
            collisions += 1;
            if collisions <= 1 {
                println!("=== TLC kept these apart; tlc-rs gives them one key ===");
                // only the lines that differ
                for (x, y) in states[j].lines().zip(states[n].lines()) {
                    if x != y {
                        println!("- {x}\n+ {y}");
                    }
                }
            }
        } else {
            seen.insert(k, n);
        }
    }
    println!("{} states read, {} distinct tlc-rs keys, {} collisions", states.len(), seen.len(), collisions);
    Ok(collisions == 0)
}
