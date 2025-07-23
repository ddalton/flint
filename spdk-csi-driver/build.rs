use std::env;
use std::path::PathBuf;

fn main() {
    // Check if we should skip SPDK bindings (useful for controller-only builds)
    let skip_spdk = env::var("SKIP_SPDK_BINDINGS").unwrap_or_default().to_lowercase() == "true";
    
    // Build SPDK bindings on Linux when not skipped
    if cfg!(target_os = "linux") && !skip_spdk {
        build_spdk_bindings();
    } else {
        create_empty_bindings();
        if skip_spdk {
            println!("cargo:warning=SPDK bindings skipped (SKIP_SPDK_BINDINGS=true)");
        } else {
            println!("cargo:warning=SPDK bindings only available on Linux");
        }
    }

    // Always build protobuf for CSI
    build_protobuf();
}

fn build_spdk_bindings() {
    println!("cargo:rerun-if-changed=wrapper.h");
    
    // Find SPDK installation
    let spdk_root = env::var("SPDK_ROOT_DIR")
        .or_else(|_| env::var("SPDK_ROOT"))
        .unwrap_or_else(|_| "/usr/local".to_string());
    
    let spdk_include = format!("{}/include", spdk_root);
    let spdk_lib = format!("{}/lib", spdk_root);
    
    // Verify SPDK installation exists
    let spdk_header = format!("{}/spdk/stdinc.h", spdk_include);
    if !std::path::Path::new(&spdk_header).exists() {
        panic!("SPDK headers not found at {}. Install SPDK or set SPDK_ROOT_DIR", spdk_include);
    }
    
    println!("cargo:warning=Using SPDK from: {}", spdk_root);
    
    // Link SPDK libraries
    println!("cargo:rustc-link-search=native={}", spdk_lib);
    
    // Core SPDK libraries
    let spdk_libs = [
        "spdk_env_dpdk", "spdk_env", "spdk_util", "spdk_log", "spdk_thread",
        "spdk_bdev", "spdk_blob", "spdk_blob_bdev", "spdk_lvol", "spdk_bdev_aio",
    ];
    
    for lib in &spdk_libs {
        println!("cargo:rustc-link-lib=static={}", lib);
    }
    
    // System dependencies
    let sys_libs = ["uring", "uuid", "dl", "rt", "numa"];
    for lib in &sys_libs {
        println!("cargo:rustc-link-lib={}", lib);
    }
    
    // Generate bindings
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", spdk_include))
        .clang_arg("-I/usr/include")
        
        // Core SPDK functions we need
        .allowlist_function("spdk_env_.*")
        .allowlist_function("spdk_log_.*")
        .allowlist_function("spdk_util_.*")
        .allowlist_function("spdk_get_ticks_hz")
        .allowlist_function("spdk_uuid_.*")
        .allowlist_function("spdk_bdev_.*")
        .allowlist_function("spdk_blob_.*")
        .allowlist_function("spdk_lvol_.*")
        .allowlist_function("spdk_bs_.*")
        
        // Essential types
        .allowlist_type("spdk_env_opts")
        .allowlist_type("spdk_log_level")
        .allowlist_type("spdk_bdev.*")
        .allowlist_type("spdk_lvol.*")
        .allowlist_type("spdk_bdev_io_.*")
        .allowlist_type("spdk_blob.*")
        .allowlist_type("spdk_bs_.*")
        .allowlist_type("spdk_uuid")
        .allowlist_type("spdk_io_channel")
        
        // Constants
        .allowlist_var("SPDK_BDEV_.*")
        .allowlist_var("SPDK_LVOL_.*")
        .allowlist_var("SPDK_LOG_.*")
        .allowlist_var("SPDK_BS_.*")
        .allowlist_var("SPDK_BLOB_.*")
        
        // Allow basic NVMe types that might be referenced
        .allowlist_type("spdk_nvme_cmd")
        .allowlist_type("spdk_nvme_status")
        
        // Block only specific problematic NVMe functions
        .blocklist_function("spdk_nvme_ctrlr_.*")
        .blocklist_function("spdk_nvme_qpair_.*")
        .blocklist_var("SPDK_NVME_CTRLR_.*")
        
        // Conservative derives
        .derive_debug(true)
        .derive_default(true)
        .derive_eq(false)
        .derive_copy(false)
        .layout_tests(false)
        .size_t_is_usize(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Failed to generate SPDK bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("spdk_bindings.rs"))
        .expect("Failed to write SPDK bindings");
    
    println!("cargo:warning=SPDK bindings generated successfully");
}

fn create_empty_bindings() {
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    std::fs::write(
        out_path.join("spdk_bindings.rs"),
        "// SPDK bindings skipped\n"
    ).expect("Failed to write empty bindings file");
}

fn build_protobuf() {
    println!("cargo:rerun-if-changed=proto/csi.proto");
    
    if !std::path::Path::new("proto/csi.proto").exists() {
        panic!("proto/csi.proto file not found");
    }
    
    let out_dir = env::var("OUT_DIR").unwrap();
    
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(&out_dir)
        .compile(&["proto/csi.proto"], &["proto/"])
        .expect("Failed to compile protobuf");
    
    // Handle file naming
    let generated_v1 = format!("{}/csi.v1.rs", out_dir);
    let generated = format!("{}/csi.rs", out_dir);
    
    if std::path::Path::new(&generated_v1).exists() {
        std::fs::rename(&generated_v1, &generated)
            .expect("Failed to rename generated protobuf file");
    }
}
