fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = tonic_prost_build::Config::new();
    config
        .protoc_arg("--experimental_allow_proto3_optional")
        .bytes(["."])
        .extern_path(".google.protobuf.Struct", "Parameters");

    let empty: &[&str] = &[];
    tonic_prost_build::configure().compile_with_config(
        config,
        &["proto/boardswarm.proto"],
        empty,
    )?;

    Ok(())
}
