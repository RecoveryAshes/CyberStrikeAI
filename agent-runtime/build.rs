fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    std::env::set_var("PROTOC", protoc);
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_protos(
            &["../proto/agent_runtime/v1/agent_runtime.proto"],
            &["../proto"],
        )?;
    println!("cargo:rerun-if-changed=../proto/agent_runtime/v1/agent_runtime.proto");
    Ok(())
}
