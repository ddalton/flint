fn main() {
    cc::Build::new().file("src/shim.c").compile("rshim");
    println!("cargo:rustc-link-lib=ibverbs");
    println!("cargo:rustc-link-lib=rdmacm");
    println!("cargo:rerun-if-changed=src/shim.c");
}
