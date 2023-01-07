fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = prost_build::Config::new();
    config
        .bytes(&["."])
        .extern_path(".google.protobuf.Struct", "Parameters");

    let empty: &[&str] = &[];
    tonic_build::configure().compile_with_config(config, &["proto/boardswarm.proto"], empty)?;

    Ok(())
}
