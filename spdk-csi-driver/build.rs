fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .include_file("csi.rs")
        .compile(
            &["proto/csi.proto"],
            &["proto/"],
        )?;
    Ok(())
}
