use std::env;
use std::path::PathBuf;

fn main() {
    // Tell cargo to look for shared libraries in the specified directory
    println!("cargo:rustc-link-search=/usr/local/lib/spdk");
    println!("cargo:rustc-link-search=/usr/local/lib/dpdk");

    // Link against SPDK libraries
    println!("cargo:rustc-link-lib=spdk_nvme");
    println!("cargo:rustc-link-lib=spdk_env_dpdk");
    println!("cargo:rustc-link-lib=spdk_log");
    println!("cargo:rustc-link-lib=spdk_util");
    println!("cargo:rustc-link-lib=spdk_vmd");

    // Link against DPDK libraries
    println!("cargo:rustc-link-lib=rte_eal");
    println!("cargo:rustc-link-lib=rte_mempool");
    println!("cargo:rustc-link-lib=rte_ring");
    println!("cargo:rustc-link-lib=rte_mbuf");

    // System libraries
    println!("cargo:rustc-link-lib=numa");
    println!("cargo:rustc-link-lib=dl");
    println!("cargo:rustc-link-lib=pthread");

    // Tell cargo to invalidate the built crate whenever the wrapper changes
    println!("cargo:rerun-if-changed=wrapper.h");

    // Generate bindings
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg("-I/usr/local/include/spdk")
        .clang_arg("-I/usr/local/include/spdk/build")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        .allowlist_function("spdk_.*")
        .allowlist_type("spdk_.*")
        .allowlist_var("SPDK_.*")
        .generate()
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
