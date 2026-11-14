//! sidecarzero-worker-gguf: the worker for GGUF embedding models through llama.cpp (worker
//! §7.2). Started by the supervisor with the arena, the model, and a status pipe; its own
//! arguments, which the supervisor puts first (`--worker-arg`), are in scz_worker_gguf.
fn main() {
    let (args, own) = scz_worker_core::parse_known(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("sidecarzero-worker-gguf[{}]: {e}", std::process::id());
        std::process::exit(2)
    });
    let opts = scz_worker_gguf::Options::parse(&own).unwrap_or_else(|e| {
        eprintln!("sidecarzero-worker-gguf[{}]: {e}", std::process::id());
        std::process::exit(2)
    });
    scz_worker_core::run_with(args, scz_worker_gguf::GgufBackend::new(opts))
}
