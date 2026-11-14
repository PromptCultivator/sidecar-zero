//! sidecarzero-worker: the worker for ONNX Runtime models (worker §7.1). Started by the
//! supervisor with the arena, the model, and a status pipe; see scz_worker_ort for options.
fn main() {
    scz_worker_core::run(scz_worker_ort::OrtBackend::from_env())
}
