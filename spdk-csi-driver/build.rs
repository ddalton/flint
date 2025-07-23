use std::env;
use std::path::PathBuf;

fn main() {
    // Only generate SPDK bindings on Linux
    if cfg!(target_os = "linux") {
        println!("cargo:rerun-if-changed=wrapper.h");
        
        // Look for SPDK installation
        let spdk_root = env::var("SPDK_ROOT_DIR")
            .or_else(|_| env::var("SPDK_ROOT"))
            .unwrap_or_else(|_| "/usr/local".to_string());
        
        let spdk_include = format!("{}/include", spdk_root);
        let spdk_lib = format!("{}/lib", spdk_root);
        
        println!("cargo:rustc-link-search=native={}", spdk_lib);
        println!("cargo:rustc-link-lib=static=spdk_bdev");
        println!("cargo:rustc-link-lib=static=spdk_blob");
        println!("cargo:rustc-link-lib=static=spdk_blob_bdev");
        println!("cargo:rustc-link-lib=static=spdk_lvol");
        println!("cargo:rustc-link-lib=static=spdk_util");
        println!("cargo:rustc-link-lib=static=spdk_env_dpdk");
        println!("cargo:rustc-link-lib=static=spdk_log");
        
        // Generate bindings
        let bindings = bindgen::Builder::default()
            .header("wrapper.h")
            .clang_arg(format!("-I{}", spdk_include))
            .clang_arg("-I/usr/include")
            // Only include what we need for Flint
            .allowlist_function("spdk_.*")
            .allowlist_type("spdk_.*")
            .allowlist_var("SPDK_.*")
            // Focus on our core functionality
            .allowlist_function("spdk_bdev_.*")
            .allowlist_function("spdk_lvol_.*")
            .allowlist_function("spdk_app_.*")
            .allowlist_function("spdk_env_.*")
            // Derive traits for ease of use
            .derive_debug(true)
            .derive_default(true)
            .derive_copy(true)
            .derive_eq(true)
            .derive_partialeq(true)
            .derive_hash(true)
            // Avoid layout tests that can be problematic
            .layout_tests(false)
            .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
            .generate()
            .expect("Unable to generate SPDK bindings");

        let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
        bindings
            .write_to_file(out_path.join("spdk_bindings.rs"))
            .expect("Couldn't write SPDK bindings!");
    } else {
        println!("cargo:warning=SPDK bindings only available on Linux");
    }

    // Always build protobuf (CSI spec)
    println!("cargo:rerun-if-changed=proto/csi.proto");
    
    let out_dir = env::var("OUT_DIR").unwrap();
    println!("Generating protobuf files to: {}", out_dir);
    
    // Ensure proto file exists
    if !std::path::Path::new("proto/csi.proto").exists() {
        panic!("proto/csi.proto file not found!");
    }
    
    let result = tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(&out_dir)
        .compile(&["proto/csi.proto"], &["proto/"]);
    
    match result {
        Ok(_) => {
            println!("Protobuf compilation succeeded");
            
            // List files in out_dir to see what was generated
            if let Ok(entries) = std::fs::read_dir(&out_dir) {
                println!("Files in {}:", out_dir);
                for entry in entries {
                    if let Ok(entry) = entry {
                        println!("  {}", entry.file_name().to_string_lossy());
                    }
                }
            }
            
                         // Check if the expected file exists (tonic generates based on package name)
             let generated_file_v1 = format!("{}/csi.v1.rs", out_dir);
             let generated_file = format!("{}/csi.rs", out_dir);
             
             if std::path::Path::new(&generated_file_v1).exists() {
                 // Rename csi.v1.rs to csi.rs for easier importing
                 std::fs::rename(&generated_file_v1, &generated_file)
                     .expect("Failed to rename csi.v1.rs to csi.rs");
                 println!("Renamed {} to {}", generated_file_v1, generated_file);
             } else if !std::path::Path::new(&generated_file).exists() {
                 panic!("Neither {} nor {} was generated", generated_file_v1, generated_file);
             }
        }
        Err(e) => {
            panic!("Failed to compile protos: {}", e);
        }
    }
    
    println!("Successfully generated protobuf files");
}
