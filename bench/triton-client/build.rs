// Generate the client side of Triton's GRPCInferenceService from the vendored protos (see
// README.md). Needs `protoc` on PATH, or PROTOC naming it; the dev image has it.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    tonic_prost_build::configure().build_server(false).compile_protos(&["proto/grpc_service.proto"], &["proto"])?;
    Ok(())
}
