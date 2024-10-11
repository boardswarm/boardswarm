fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = prost_build::Config::new();
    config
        .protoc_arg("--experimental_allow_proto3_optional")
        .bytes(["."])
        .extern_path(".google.protobuf.Struct", "Parameters");

    let empty: &[&str] = &[];
    tonic_build::configure().compile_protos_with_config(
        config,
        &["proto/boardswarm.proto"],
        empty,
    )?;

    Ok(())
}
