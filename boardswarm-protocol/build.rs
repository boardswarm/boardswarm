fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = tonic_prost_build::Config::new();
    config
        .protoc_arg("--experimental_allow_proto3_optional")
        .bytes(["."])
        .extern_path(".google.protobuf.Struct", "Parameters");

    let empty: &[&str] = &[];
    let mut builder = tonic_prost_build::configure();

    // Don't generate transport-dependent code when transport feature is off
    if std::env::var("CARGO_FEATURE_TRANSPORT").is_err() {
        builder = builder.build_transport(false);
    }

    builder.compile_with_config(config, &["proto/boardswarm.proto"], empty)?;

    Ok(())
}
