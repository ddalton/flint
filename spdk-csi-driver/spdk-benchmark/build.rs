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
    println!("cargo:rustc-link-lib=rte_pci");
    println!("cargo:rustc-link-lib=rte_bus_pci");
    println!("cargo:rustc-link-lib=rte_kvargs");
    println!("cargo:rustc-link-lib=rte_telemetry");

    // System libraries
    println!("cargo:rustc-link-lib=numa");
    println!("cargo:rustc-link-lib=dl");
    println!("cargo:rustc-link-lib=pthread");

    // Force link OpenSSL (SPDK has undefined references to these)
    // Use whole-archive to ensure they're actually linked
    println!("cargo:rustc-link-arg=-Wl,--no-as-needed");
    println!("cargo:rustc-link-lib=ssl");
    println!("cargo:rustc-link-lib=crypto");
    println!("cargo:rustc-link-arg=-Wl,--as-needed");

    // Tell cargo to invalidate the built crate whenever the wrapper changes
    println!("cargo:rerun-if-changed=wrapper.h");

    // Generate bindings
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg("-I/usr/local/include/spdk")
        .clang_arg("-I/usr/local/include/spdk/build")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_function("spdk_.*")
        .allowlist_type("spdk_.*")
        .allowlist_var("SPDK_.*")
        // Disable layout tests to avoid packed struct alignment issues
        .layout_tests(false)
        // Disable all automatic derives to avoid packed struct issues
        .derive_copy(false)
        .derive_debug(false)
        .derive_default(false)
        .derive_hash(false)
        .derive_eq(false)
        .derive_partialeq(false)
        .derive_ord(false)
        .derive_partialord(false)
        // Make problematic types opaque to avoid alignment errors
        .opaque_type("spdk_nvme_ctrlr_data")
        .opaque_type("spdk_nvmf_fabric_connect_rsp")
        .opaque_type("spdk_nvmf_fabric_prop_get_rsp")
        // Disable bitfield alignment that causes packed struct issues
        .default_enum_style(bindgen::EnumVariation::Rust { non_exhaustive: false })
        .generate()
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
