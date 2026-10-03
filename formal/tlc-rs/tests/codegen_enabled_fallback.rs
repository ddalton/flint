//! An invariant with ENABLED (which the generator does not compile) still
//! builds and is decided by a generated checker: the formula returns
//! `rt::UNSUPPORTED` and the engine asks the interpreter for that one
//! invariant. LeanSubtree's convergence invariants (`~ENABLED SyncerProgress
//! => ...`) are the case: before this, their generated crate did not compile.
//!
//! Builds a generated crate (debug), so it takes a minute the first time.

use std::path::{Path, PathBuf};
use std::process::Command;

const SPEC: &str = r#"---- MODULE EnabledToy ----
EXTENDS Naturals
VARIABLE x
Init == x = 0
Next == x < 3 /\ x' = x + 1
Spec == Init /\ [][Next]_x
Settled == ~ENABLED Next => x = 3
Early == ~ENABLED Next => x = 2
====
"#;

fn cfg(inv: &str) -> String {
    format!("SPECIFICATION Spec\nCHECK_DEADLOCK FALSE\nINVARIANT {inv}\n")
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("spawn");
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

fn generated(dir: &Path, cfg_name: &str) -> PathBuf {
    let crate_dir = dir.join(format!("gen-{cfg_name}"));
    let out = run(Command::new(env!("CARGO_BIN_EXE_tlc-rs"))
        .current_dir(dir)
        .args(["-codegen", crate_dir.to_str().unwrap(), "-config", &format!("{cfg_name}.cfg"), "EnabledToy.tla"]));
    let main = std::fs::read_to_string(crate_dir.join("src/main.rs")).unwrap_or_else(|_| panic!("codegen failed: {out}"));
    assert!(main.contains("Err(e) if e == UNSUPPORTED => Engine::invariant(self.p, i, cx)"), "the invariant must fall back to the interpreter");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codegen-enabled");
    let out = run(Command::new(env!("CARGO"))
        .current_dir(&crate_dir)
        .env("CARGO_TARGET_DIR", &target)
        .args(["build", "--offline", "--quiet"]));
    let bin = target.join("debug/tlcgen-enabledtoy");
    assert!(out.trim().is_empty() || !out.contains("error"), "the generated crate must build: {out}");
    assert!(bin.exists(), "the generated crate must build: {out}");
    bin
}

fn check(bin: &Path, dir: &Path, cfg_name: &str, interp: bool) -> String {
    let mut cmd = Command::new(bin);
    cmd.current_dir(dir);
    if interp {
        cmd.args(["-engine", "interp"]);
    }
    run(cmd.args(["-workers", "1", "-metadir", dir.join(format!("md-{cfg_name}-{interp}")).to_str().unwrap(),
                  "-config", &format!("{cfg_name}.cfg"), "EnabledToy.tla"]))
}

#[test]
fn a_generated_checker_decides_an_enabled_invariant_through_the_interpreter() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("enabled-toy");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("EnabledToy.tla"), SPEC).unwrap();
    std::fs::write(dir.join("Settled.cfg"), cfg("Settled")).unwrap();
    std::fs::write(dir.join("Early.cfg"), cfg("Early")).unwrap();
    let interp = Path::new(env!("CARGO_BIN_EXE_tlc-rs"));

    // Holds: Next is disabled only at x = 3.
    let bin = generated(&dir, "Settled");
    for (who, out) in [("generated", check(&bin, &dir, "Settled", false)), ("interpreter", check(interp, &dir, "Settled", true))] {
        assert!(out.contains("No error has been found"), "{who} must find Settled holds: {out}");
        assert!(out.contains(" 4 distinct states found"), "{who} must see the 4 states: {out}");
    }

    // Violated: at x = 3 nothing is enabled and x is not 2.
    let bin = generated(&dir, "Early");
    for (who, out) in [("generated", check(&bin, &dir, "Early", false)), ("interpreter", check(interp, &dir, "Early", true))] {
        assert!(out.contains("Invariant Early is violated"), "{who} must catch Early: {out}");
    }
}
