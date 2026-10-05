//! A generated checker keys states under SYMMETRY + VIEW with compiled
//! VIEW parts (`vpart_c`, through `Engine::view_part`), not the
//! interpreter, and reaches the interpreter's state count. The seen-set
//! key evaluates one VIEW part per new state; before 2026-10-05 it always
//! did so in the interpreter, and on ForgeSyncKeptSet (a VIEW that ranks
//! tokens with a set comprehension) that was 63% of a 192-worker run.
//!
//! One worker on both sides: under SYMMETRY with a VIEW the distinct
//! count can depend on the order states arrive in at more than one.
//!
//! Builds a generated crate (debug), so it takes a minute the first time.

use std::path::{Path, PathBuf};
use std::process::Command;

const SPEC: &str = r#"---- MODULE ViewToy ----
EXTENDS Naturals, FiniteSets, TLC
CONSTANTS Procs
VARIABLES held, ctr
Init == held = [p \in Procs |-> 0] /\ ctr = 1
Take(p) == ctr < 5 /\ held[p] = 0 /\ held' = [held EXCEPT ![p] = ctr] /\ ctr' = ctr + 1
Drop(p) == held[p] # 0 /\ held' = [held EXCEPT ![p] = 0] /\ UNCHANGED ctr
Next == \E p \in Procs : Take(p) \/ Drop(p)
Spec == Init /\ [][Next]_<<held, ctr>>
\* Tokens are compared only for order, so a view keeps their RANK.
Rank(n) == Cardinality({q \in Procs : held[q] # 0 /\ held[q] < n})
View == <<[p \in Procs |-> IF held[p] = 0 THEN 0 ELSE 1 + Rank(held[p])], ctr>>
Sym == Permutations(Procs)
====
"#;

const CFG: &str = "SPECIFICATION Spec\nCHECK_DEADLOCK FALSE\nCONSTANTS\n  Procs = {a, b, c}\nVIEW View\nSYMMETRY Sym\n";

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("spawn");
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

fn generated(dir: &Path) -> PathBuf {
    let crate_dir = dir.join("gen");
    let out = run(Command::new(env!("CARGO_BIN_EXE_tlc-rs"))
        .current_dir(dir)
        .args(["-codegen", crate_dir.to_str().unwrap(), "-config", "ViewToy.cfg", "ViewToy.tla"]));
    let main = std::fs::read_to_string(crate_dir.join("src/main.rs")).unwrap_or_else(|_| panic!("codegen failed: {out}"));
    // Two parts (the ranked map, then `ctr`), both compiled and dispatched.
    assert!(main.contains("fn vpart_0(") && main.contains("fn vpart_1("), "each VIEW part must be compiled");
    assert!(main.contains("fn view_part(") && main.contains("0 => match vpart_0("), "view_part must dispatch to them");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codegen-view-parts");
    let out = run(Command::new(env!("CARGO"))
        .current_dir(&crate_dir)
        .env("CARGO_TARGET_DIR", &target)
        .args(["build", "--offline", "--quiet"]));
    let bin = target.join("debug/tlcgen-viewtoy");
    assert!(bin.exists(), "the generated crate must build: {out}");
    bin
}

fn distinct(bin: &Path, dir: &Path, interp: bool) -> String {
    let mut cmd = Command::new(bin);
    cmd.current_dir(dir);
    if interp {
        cmd.args(["-engine", "interp"]);
    }
    let out = run(cmd.args(["-workers", "1", "-metadir", dir.join(format!("md-{interp}")).to_str().unwrap(),
                            "-config", "ViewToy.cfg", "ViewToy.tla"]));
    assert!(out.contains("No error has been found"), "{out}");
    out.split(" distinct states found").next().and_then(|s| s.rsplit(' ').next()).unwrap_or("").to_string()
}

#[test]
fn a_generated_checker_keys_symmetry_and_view_with_compiled_parts_and_the_interpreters_count() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("view-toy");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ViewToy.tla"), SPEC).unwrap();
    std::fs::write(dir.join("ViewToy.cfg"), CFG).unwrap();
    let bin = generated(&dir);
    let compiled = distinct(&bin, &dir, false);
    let interp = distinct(Path::new(env!("CARGO_BIN_EXE_tlc-rs")), &dir, true);
    assert!(!compiled.is_empty(), "no count from the generated checker");
    assert_eq!(compiled, interp, "the compiled VIEW parts must key states exactly as the interpreter does");
}
