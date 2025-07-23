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
    
    // Debug: List available SPDK libraries
    if let Ok(entries) = std::fs::read_dir(&spdk_lib) {
        println!("cargo:warning=Available libraries in {}:", spdk_lib);
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".a") || name.ends_with(".so") {
                    println!("cargo:warning=  {}", name);
                }
            }
        }
    }
    
    // Link SPDK libraries
    println!("cargo:rustc-link-search=native={}", spdk_lib);
    
    // Also add DPDK library path (DPDK is built as part of SPDK)
    let dpdk_lib = format!("{}/lib", spdk_root);
    println!("cargo:rustc-link-search=native={}", dpdk_lib);
    
    // Core SPDK libraries (verified against SPDK v24.01.x) - use dynamic linking
    let spdk_libs = [
        "spdk_env_dpdk", "spdk_util", "spdk_log", "spdk_thread",
        "spdk_bdev", "spdk_blob", "spdk_blob_bdev", "spdk_lvol", "spdk_bdev_aio",
        "spdk_json", "spdk_accel", "spdk_trace", "spdk_notify", "spdk_conf",
        "spdk_init", "spdk_dma",
    ];
    
    for lib in &spdk_libs {
        println!("cargo:rustc-link-lib=dylib={}", lib);
    }
    
    // DPDK libraries (required by SPDK when built with --with-shared)
    let dpdk_libs = [
        "rte_eal", "rte_mempool", "rte_ring", "rte_kvargs", "rte_hash",
        "rte_timer", "rte_mbuf", "rte_ethdev", "rte_cryptodev", "rte_bus_pci",
        "rte_pci", "rte_telemetry", "rte_rcu",
    ];
    
    for lib in &dpdk_libs {
        println!("cargo:rustc-link-lib=dylib={}", lib);
    }
    
    // System dependencies
    let sys_libs = ["uring", "uuid", "dl", "rt", "numa"];
    for lib in &sys_libs {
        println!("cargo:rustc-link-lib={}", lib);
    }
    
    // ISA-L library (Intel Storage Acceleration Library) - required by SPDK
    println!("cargo:rustc-link-lib=dylib=isal");
    
    // Generate bindings
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", spdk_include))
        .clang_arg("-I/usr/include")
        
        // Core SPDK environment and initialization
        .allowlist_function("spdk_env_.*")
        .allowlist_function("spdk_log_.*")
        .allowlist_function("spdk_util_.*")
        .allowlist_function("spdk_get_ticks_hz")
        .allowlist_function("spdk_uuid_.*")
        
        // Bdev iteration and property functions (real SPDK v24.01.x APIs)
        .allowlist_function("spdk_bdev_first")
        .allowlist_function("spdk_bdev_next")
        .allowlist_function("spdk_bdev_get_by_name")
        .allowlist_function("spdk_bdev_get_name")
        .allowlist_function("spdk_bdev_get_block_size")
        .allowlist_function("spdk_bdev_get_num_blocks")
        .allowlist_function("spdk_bdev_get_uuid")
        .allowlist_function("spdk_bdev_get_product_name")
        .allowlist_function("spdk_bdev_get_module_name")
        .allowlist_function("spdk_bdev_io_type_supported")
        .allowlist_function("spdk_bdev_open_ext")
        .allowlist_function("spdk_bdev_close")
        
        // Blob/Blobstore functions (real SPDK v24.01.x APIs)
        .allowlist_function("spdk_bs_.*")
        .allowlist_function("spdk_blob_.*")
        .allowlist_function("spdk_lvol_.*")
        
        // Essential bdev types and constants
        .allowlist_type("spdk_bdev")
        .allowlist_type("spdk_bdev_desc")
        .allowlist_type("spdk_bdev_io_type")
        .allowlist_type("spdk_io_channel")
        .allowlist_type("spdk_uuid")
        .allowlist_type("spdk_env_opts")
        .allowlist_type("spdk_log_level")
        
        // Bdev I/O type constants (from spdk/bdev.h enum)
        .allowlist_var("SPDK_BDEV_IO_TYPE_.*")
        .allowlist_var("SPDK_LOG_.*")
        .allowlist_var("SPDK_BDEV_.*")
        .allowlist_var("SPDK_ENV_.*")
        
        // Generate enum constants
        .rustified_enum("spdk_bdev_io_type")
        
        // Blob/LVS related types
        .allowlist_type("spdk_blob_store")
        .allowlist_type("spdk_blob")
        .allowlist_type("spdk_lvol_store")
        .allowlist_type("spdk_lvol")
        .allowlist_type("spdk_bs_dev")
        .allowlist_type("spdk_bs_opts")
        .allowlist_type("spdk_blob_opts")
        
        // Allow some basic NVMe types that might be referenced but block complex ones
        .allowlist_type("spdk_nvme_cmd")
        .allowlist_type("spdk_nvme_status")
        
        // Block only specific problematic NVMe functions that might cause issues
        .blocklist_function("spdk_nvme_ctrlr_.*_detailed.*")
        .blocklist_function("spdk_nvme_qpair_.*_advanced.*")
        
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
