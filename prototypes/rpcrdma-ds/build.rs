fn main() {
    // The verbs shim exists only where verbs do. Elsewhere the crate
    // still builds so the protocol tests (src/proto/tests.rs) run.
    println!("cargo:rerun-if-changed=src/shim.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    cc::Build::new().file("src/shim.c").compile("rshim");
    println!("cargo:rustc-link-lib=ibverbs");
    println!("cargo:rustc-link-lib=rdmacm");
}
