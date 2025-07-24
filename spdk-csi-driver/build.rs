use std::env;
use std::path::PathBuf;

fn main() {
    // Build protobuf for CSI interface
    build_protobuf();
}

fn build_protobuf() {
    println!("cargo:rerun-if-changed=proto/");
    
    // Use tonic-build since CSI is a gRPC service
    match tonic_build::configure()
        .out_dir(env::var("OUT_DIR").unwrap())
        .compile(&["proto/csi.proto"], &["proto"]) {
        Ok(_) => {
            println!("cargo:warning=CSI protobuf bindings generated successfully");
            
            // List generated files for debugging
            let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
            if let Ok(entries) = std::fs::read_dir(&out_dir) {
                for entry in entries.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        if name.ends_with(".rs") {
                            println!("cargo:warning=Generated file: {}", name);
                        }
                    }
                }
            }
        },
        Err(e) => {
            panic!("Failed to compile CSI protobuf files: {}", e);
        }
    }
}
