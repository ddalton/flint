//! A generated checker evaluates step properties `[][A]_v` itself
//! (`aprop_i`, through `Engine::step_prop`) and reaches the interpreter's
//! verdict: a violated one is caught on the step that breaks it, one that
//! holds passes with the same state count. Each property reads a constant
//! set (from the cfg), so the pool must carry the properties' constants
//! too (`walk_tprop`): scalars and set literals are not pooled.
//!
//! Builds a generated crate (debug), so it takes a minute the first time.

use std::path::{Path, PathBuf};
use std::process::Command;

const SPEC: &str = r#"---- MODULE StepToy ----
EXTENDS Naturals
CONSTANTS N, Up, All
VARIABLE x
Init == x = 0
Next == x' = (x + 1) % N
Spec == Init /\ [][Next]_x
Rises == [][x' > x /\ x' \in Up]_x
Wraps == [][x' = (x + 1) % N /\ x' \in All]_x
====
"#;

fn cfg(prop: &str) -> String {
    format!("SPECIFICATION Spec\nCHECK_DEADLOCK FALSE\nCONSTANTS\n  N = 4\n  Up = {{1, 2, 3}}\n  All = {{0, 1, 2, 3}}\nPROPERTY {prop}\n")
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("spawn");
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

fn generated(dir: &Path, cfg_name: &str) -> PathBuf {
    let crate_dir = dir.join(format!("gen-{cfg_name}"));
    let out = run(Command::new(env!("CARGO_BIN_EXE_tlc-rs"))
        .current_dir(dir)
        .args(["-codegen", crate_dir.to_str().unwrap(), "-config", &format!("{cfg_name}.cfg"), "StepToy.tla"]));
    let main = std::fs::read_to_string(crate_dir.join("src/main.rs")).unwrap_or_else(|_| panic!("codegen failed: {out}"));
    assert!(main.contains("fn aprop_0("), "the step property must be compiled, not left to the interpreter");
    assert!(main.contains("0 => match aprop_0("), "step_prop must dispatch to it");
    assert!(main.contains("0 => match aprop_nx_0("), "step_prop_nx must dispatch to the body on the unbuilt successor");
    let aprop = main.lines().find(|l| l.starts_with("fn aprop_nx_0(")).unwrap();
    assert!(aprop.contains("g.k["), "the property must read its constant from the pool: {aprop}");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codegen-step-props");
    let out = run(Command::new(env!("CARGO"))
        .current_dir(&crate_dir)
        .env("CARGO_TARGET_DIR", &target)
        .args(["build", "--offline", "--quiet"]));
    let bin = target.join("debug/tlcgen-steptoy");
    assert!(bin.exists(), "the generated crate must build: {out}");
    bin
}

fn check(bin: &Path, dir: &Path, cfg_name: &str, interp: bool) -> String {
    let mut cmd = Command::new(bin);
    cmd.current_dir(dir);
    if interp {
        cmd.args(["-engine", "interp"]);
    }
    run(cmd.args(["-workers", "2", "-metadir", dir.join(format!("md-{cfg_name}-{interp}")).to_str().unwrap(),
                  "-config", &format!("{cfg_name}.cfg"), "StepToy.tla"]))
}

#[test]
fn a_generated_checker_decides_step_properties_as_the_interpreter_does() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("step-toy");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("StepToy.tla"), SPEC).unwrap();
    std::fs::write(dir.join("Rises.cfg"), cfg("Rises")).unwrap();
    std::fs::write(dir.join("Wraps.cfg"), cfg("Wraps")).unwrap();
    let interp = Path::new(env!("CARGO_BIN_EXE_tlc-rs"));

    // Violated: the wrap 3 -> 0 does not rise.
    let bin = generated(&dir, "Rises");
    for (who, out) in [("generated", check(&bin, &dir, "Rises", false)), ("interpreter", check(interp, &dir, "Rises", true))] {
        assert!(out.contains("Action property Rises is violated"), "{who} must catch Rises: {out}");
    }

    // Holds: every step is the wrap-around increment. Same count both ways.
    let bin = generated(&dir, "Wraps");
    for (who, out) in [("generated", check(&bin, &dir, "Wraps", false)), ("interpreter", check(interp, &dir, "Wraps", true))] {
        assert!(out.contains("No error has been found"), "{who} must find Wraps holds: {out}");
        assert!(out.contains(" 4 distinct states found"), "{who} must see the 4 states: {out}");
    }
}
