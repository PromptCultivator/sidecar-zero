//! Triton's gRPC API with tensors in registered system shared memory: the benchmark's system B
//! (bench §3).
//!
//! One [`Triton`] per caller thread. Each owns a current-thread Tokio runtime, one HTTP/2
//! channel to Triton, and two POSIX shared-memory regions it has registered with Triton: the
//! input (one row of 256 `f32`, 1024 bytes) and the output (one `f32`). A call writes the row
//! into the input region, sends a `ModelInfer` whose tensors name the regions instead of
//! carrying bytes, and reads the score Triton wrote into the output region. Since the tensors
//! travel in shared memory, the request message is the same on every call: it is encoded once,
//! when the client connects, and each call sends those bytes again.
//!
//! The client process must see the same `/dev/shm` as Triton (in Docker, `--ipc=container:`
//! Triton's container; see `bench/baselines/triton/README.md`).
//!
//! Failure handling: every error, including the caller's timeout, comes back as `Err`. The
//! channel reconnects by itself on the call after a lost connection, and after any failed call
//! the next one registers both regions again before its inference, since a restarted Triton has
//! forgotten them. That registration runs as a task on the client's runtime, so when a call's
//! timeout is shorter than the registration it carries on during the following calls instead of
//! starting over.

use std::ffi::CString;
use std::future::{poll_fn, Future};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::pin;
use std::sync::atomic::{fence, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use prost::bytes::BufMut;
use prost::Message;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

/// The types and client generated from Triton's `grpc_service.proto` (package `inference`).
#[allow(clippy::all, clippy::pedantic, missing_docs, rustdoc::all)]
pub mod proto {
    tonic::include_proto!("inference");
}

use proto::grpc_inference_service_client::GrpcInferenceServiceClient;
use proto::infer_parameter::ParameterChoice;
use proto::model_infer_request::{InferInputTensor, InferRequestedOutputTensor};
use proto::{InferParameter, ModelInferRequest, ModelInferResponse, ModelReadyRequest, SystemSharedMemoryRegisterRequest, SystemSharedMemoryUnregisterRequest};

/// Features per row: the fraud model's input is `features: float32[rows, 256]`.
pub const FEATURES: usize = 256;
const INPUT_BYTES: usize = FEATURES * 4;
const OUTPUT_BYTES: usize = 4;
/// The bound on [`Triton::connect`]: reaching Triton, checking the model, registering.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The bound on the best-effort unregistration when a client is dropped.
const DROP_TIMEOUT: Duration = Duration::from_millis(500);
const MODEL_INFER: &str = "/inference.GRPCInferenceService/ModelInfer";

type Control = GrpcInferenceServiceClient<Channel>;

/// A blocking client of one model on one Triton server. See the crate documentation.
pub struct Triton {
    /// For the control calls: model readiness, registration.
    control: Control,
    /// For `ModelInfer`, sending the pre-encoded request. The same channel as `control`.
    infer: tonic::client::Grpc<Channel>,
    /// The `ModelInferRequest`, encoded once.
    request: Arc<[u8]>,
    input: Region,
    output: Region,
    registration: Registration,
    deadline: Deadline,
    /// Last, so the channel and the timer drop while the runtime that drives them still exists.
    rt: Runtime,
}

enum Registration {
    Done,
    /// The next call registers both regions before its inference.
    Needed,
    /// A registration that outlived the call that started it.
    Pending(JoinHandle<Result<(), String>>),
}

impl Triton {
    /// Connect to Triton's gRPC endpoint (e.g. `"http://127.0.0.1:8001"`), check that `model`
    /// is ready, create this client's two shared-memory regions, and register both with Triton.
    ///
    /// The regions are named `scz-<tag>-<pid>-in` and `-out`, as Triton registrations and as
    /// POSIX shared-memory objects (`/dev/shm/scz-<tag>-<pid>-in`). `tag` is up to 64 ASCII
    /// letters, digits, `-` or `_`, and must be unique among the live clients of one process.
    /// Fails after 10 seconds when Triton is not answering.
    pub fn connect(endpoint: &str, model: &str, tag: &str) -> Result<Triton, String> {
        if tag.is_empty() || tag.len() > 64 || !tag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(format!("tag {tag:?}: use 1 to 64 ASCII letters, digits, '-' or '_'"));
        }
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("starting the client's runtime: {e}"))?;
        let (channel, deadline) = {
            // The channel spawns its connection tasks on the runtime it is made in, and the timer
            // registers with its reactor.
            let _in_rt = rt.enter();
            let channel = Endpoint::from_shared(endpoint.to_owned())
                .map_err(|e| format!("endpoint {endpoint:?}: {e}"))?
                .connect_timeout(CONNECT_TIMEOUT)
                .tcp_nodelay(true)
                .connect_lazy();
            (channel, Deadline::new()?)
        };
        let base = format!("scz-{tag}-{}", std::process::id());
        let input = Region::create(format!("{base}-in"), INPUT_BYTES)?;
        let output = Region::create(format!("{base}-out"), OUTPUT_BYTES)?;
        let request = infer_request(model, &input, &output);
        let mut t = Triton {
            control: Control::new(channel.clone()),
            infer: tonic::client::Grpc::new(channel),
            request,
            input,
            output,
            registration: Registration::Needed,
            deadline,
            rt,
        };
        // From here a failure drops `t`, which unregisters whatever was registered.
        let mut control = t.control.clone();
        let regions = [t.input.registration(), t.output.registration()];
        let model = model.to_owned();
        t.rt.block_on(async move {
            let ready = control.model_ready(ModelReadyRequest { name: model.clone(), version: String::new() });
            let ready = tokio::time::timeout(CONNECT_TIMEOUT, ready).await.map_err(|_| format!("Triton did not answer within {CONNECT_TIMEOUT:?}"))?;
            if !ready.map_err(|s| failed("ModelReady", &s))?.into_inner().ready {
                return Err(format!("model {model:?} is not ready"));
            }
            tokio::time::timeout(CONNECT_TIMEOUT, register(control, regions)).await.map_err(|_| "registering the regions timed out".to_owned())?
        })?;
        t.registration = Registration::Done;
        Ok(t)
    }

    /// One inference: write `features` into the input region, send `ModelInfer` naming both
    /// regions, and read the score back from the output region.
    ///
    /// Returns `Err` on any failure, and when `timeout` passes first. The timeout is a timerfd,
    /// so it fires when it is due, give or take the kernel's wake-up latency, rather than on
    /// Tokio's 1 ms tick. A call after a failure first re-registers the regions (see the crate
    /// documentation).
    pub fn infer(&mut self, features: &[f32; FEATURES], timeout: Duration) -> Result<f32, String> {
        // SAFETY: the input region is INPUT_BYTES long, mapped read-write, and written only by
        // this client; Triton reads it only during a call, and no call is running.
        unsafe { std::ptr::copy_nonoverlapping(features.as_ptr().cast::<u8>(), self.input.ptr, INPUT_BYTES) };
        fence(Ordering::Release);
        let Triton { control, infer, request, input, output, registration, deadline, rt } = self;
        let call = async {
            if let Registration::Needed = registration {
                let regions = [input.registration(), output.registration()];
                *registration = Registration::Pending(tokio::spawn(register(control.clone(), regions)));
            }
            if let Registration::Pending(task) = registration {
                let done = task.await.unwrap_or_else(|e| Err(format!("registering the regions: {e}")));
                *registration = if done.is_ok() { Registration::Done } else { Registration::Needed };
                done?;
            }
            infer.ready().await.map_err(|e| format!("ModelInfer: {e}"))?;
            let response = infer.unary(Request::new(request.clone()), PathAndQuery::from_static(MODEL_INFER), InferCodec).await;
            check(&response.map_err(|s| failed("ModelInfer", &s))?.into_inner())
        };
        deadline.arm(timeout);
        let result = rt.block_on(async {
            let (mut call, mut expired) = (pin!(call), pin!(deadline.expired()));
            poll_fn(|cx| match call.as_mut().poll(cx) {
                Poll::Ready(r) => Poll::Ready(r),
                Poll::Pending => expired.as_mut().poll(cx).map(|()| Err(format!("timed out after {timeout:?}"))),
            })
            .await
        });
        deadline.disarm();
        if let Err(e) = result {
            // Re-register on the next call, unless a registration is already under way.
            if let Registration::Done = registration {
                *registration = Registration::Needed;
            }
            return Err(e);
        }
        fence(Ordering::Acquire);
        // SAFETY: the output region is OUTPUT_BYTES long and page-aligned; Triton wrote it
        // before answering, and writes it only during a call.
        Ok(unsafe { std::ptr::read_volatile(output.ptr.cast::<f32>()) })
    }
}

impl Drop for Triton {
    /// Unregister both regions, best effort and for at most 500 ms; the regions are then
    /// unmapped and unlinked as the fields drop.
    fn drop(&mut self) {
        if let Registration::Pending(task) = &self.registration {
            task.abort();
        }
        let mut control = self.control.clone();
        let names = [self.input.name.clone(), self.output.name.clone()];
        let unregister = async move {
            for name in names {
                let _ = control.system_shared_memory_unregister(SystemSharedMemoryUnregisterRequest { name }).await;
            }
        };
        // The timer is made inside the runtime: a Sleep needs one to exist.
        self.rt.block_on(async { _ = tokio::time::timeout(DROP_TIMEOUT, unregister).await });
    }
}

/// Register both regions. Each is unregistered first: a registration left over from an attempt
/// that timed out, or from a crashed process with the same name, would otherwise make the new
/// one fail. Unregistering a name Triton does not know succeeds.
async fn register(mut control: Control, regions: [SystemSharedMemoryRegisterRequest; 2]) -> Result<(), String> {
    for region in regions {
        let name = region.name.clone();
        control.system_shared_memory_unregister(SystemSharedMemoryUnregisterRequest { name }).await.map_err(|s| failed("SystemSharedMemoryUnregister", &s))?;
        control.system_shared_memory_register(region).await.map_err(|s| failed("SystemSharedMemoryRegister", &s))?;
    }
    Ok(())
}

/// The one `ModelInferRequest` this client sends: one row in, one score out, both by region.
fn infer_request(model: &str, input: &Region, output: &Region) -> Arc<[u8]> {
    ModelInferRequest {
        model_name: model.to_owned(),
        inputs: vec![InferInputTensor {
            name: "features".to_owned(),
            datatype: "FP32".to_owned(),
            shape: vec![1, FEATURES as i64],
            parameters: input.parameters(),
            contents: None,
        }],
        outputs: vec![InferRequestedOutputTensor { name: "score".to_owned(), parameters: output.parameters() }],
        ..ModelInferRequest::default()
    }
    .encode_to_vec()
    .into()
}

/// A response is accepted when it has the one output asked for, of one row's shape. Its data
/// is in the output region, not in the message.
fn check(response: &ModelInferResponse) -> Result<(), String> {
    match response.outputs.as_slice() {
        [o] if o.shape == [1, 1] => Ok(()),
        outputs => Err(format!("ModelInfer: unexpected outputs {outputs:?}")),
    }
}

fn failed(call: &str, status: &Status) -> String {
    format!("{call}: {:?}: {}", status.code(), status.message())
}

/// Sends the pre-encoded request bytes as they are, and decodes the response with prost.
#[derive(Clone, Copy)]
struct InferCodec;

impl Codec for InferCodec {
    type Encode = Arc<[u8]>;
    type Decode = ModelInferResponse;
    type Encoder = InferCodec;
    type Decoder = InferCodec;
    fn encoder(&mut self) -> InferCodec {
        *self
    }
    fn decoder(&mut self) -> InferCodec {
        *self
    }
}

impl Encoder for InferCodec {
    type Item = Arc<[u8]>;
    type Error = Status;
    fn encode(&mut self, item: Arc<[u8]>, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for InferCodec {
    type Item = ModelInferResponse;
    type Error = Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<ModelInferResponse>, Status> {
        ModelInferResponse::decode(src).map(Some).map_err(|e| Status::internal(format!("decoding ModelInferResponse: {e}")))
    }
}

/// A call's deadline: a timerfd registered with the client's runtime, which wakes it when the
/// deadline is due. Tokio's own timer rounds a deadline up to its 1 ms tick and then its sleep up
/// to a whole millisecond again, so a 2 ms budget could become nearly 4.
struct Deadline(AsyncFd<OwnedFd>);

impl Deadline {
    /// Within the runtime, which it registers with.
    fn new() -> Result<Deadline, String> {
        // SAFETY: timerfd_create has no preconditions; the result is checked.
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
        if fd < 0 {
            return Err(format!("timerfd_create: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: fd is a new descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: an OwnedFd keeps its descriptor open, and the same, until it is dropped, which
        // is when the AsyncFd that owns it is.
        let fd = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) };
        fd.map(Deadline).map_err(|e| format!("registering the timer: {e}"))
    }

    /// Expire `after` from now. Setting the timer also clears an expiry not yet read.
    fn arm(&self, after: Duration) {
        // A zero it_value would disarm it instead.
        self.set(after.max(Duration::from_nanos(1)));
    }

    fn disarm(&self) {
        self.set(Duration::ZERO);
    }

    fn set(&self, after: Duration) {
        let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let value = libc::timespec { tv_sec: after.as_secs().min(i32::MAX as u64) as libc::time_t, tv_nsec: after.subsec_nanos() as libc::c_long };
        let spec = libc::itimerspec { it_interval: zero, it_value: value };
        // SAFETY: a timerfd this value owns, and a valid itimerspec; it cannot fail with these.
        unsafe { libc::timerfd_settime(self.0.as_raw_fd(), 0, &spec, std::ptr::null_mut()) };
    }

    /// Resolves when the armed timer expires. Readiness left over from an earlier arming reads
    /// nothing and is cleared, so only this arming's expiry counts.
    async fn expired(&self) {
        loop {
            // An error from the reactor counts as expiry: the call fails rather than hangs.
            let Ok(mut ready) = self.0.readable().await else { return };
            let mut n = 0u64;
            // SAFETY: reads at most 8 bytes into a u64.
            if unsafe { libc::read(self.0.as_raw_fd(), (&raw mut n).cast(), 8) } == 8 {
                return;
            }
            ready.clear_ready();
        }
    }
}

/// A POSIX shared-memory object, mapped read-write, unlinked when dropped.
struct Region {
    /// The registration name; the object is `/<name>`.
    name: String,
    key: CString,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: a Region exclusively owns its mapping; nothing about it is tied to a thread.
unsafe impl Send for Region {}

impl Region {
    fn create(name: String, len: usize) -> Result<Region, String> {
        let key = CString::new(format!("/{name}")).map_err(|e| e.to_string())?;
        let os = |what: &str| format!("{what} {key:?}: {}", std::io::Error::last_os_error());
        // SAFETY: plain libc calls on a NUL-terminated name and a descriptor this function
        // owns; every result is checked before it is used. Without O_EXCL an object left by a
        // crashed process of the same pid is reused, and resized.
        unsafe {
            let fd = libc::shm_open(key.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
            if fd < 0 {
                return Err(os("shm_open"));
            }
            let mut ptr = libc::MAP_FAILED;
            let err = if libc::ftruncate(fd, len as libc::off_t) != 0 {
                Some(os("ftruncate"))
            } else {
                ptr = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
                (ptr == libc::MAP_FAILED).then(|| os("mmap"))
            };
            libc::close(fd);
            if let Some(err) = err {
                libc::shm_unlink(key.as_ptr());
                return Err(err);
            }
            Ok(Region { name, key, ptr: ptr.cast(), len })
        }
    }

    fn registration(&self) -> SystemSharedMemoryRegisterRequest {
        let key = self.key.to_str().expect("built from a String").to_owned();
        SystemSharedMemoryRegisterRequest { name: self.name.clone(), key, offset: 0, byte_size: self.len as u64 }
    }

    /// The tensor parameters that place a tensor in this region.
    fn parameters(&self) -> std::collections::HashMap<String, InferParameter> {
        let p = |c| InferParameter { parameter_choice: Some(c) };
        [
            ("shared_memory_region", p(ParameterChoice::StringParam(self.name.clone()))),
            ("shared_memory_byte_size", p(ParameterChoice::Int64Param(self.len as i64))),
            ("shared_memory_offset", p(ParameterChoice::Int64Param(0))),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: ptr and len are the mapping made in `create`, unmapped only here.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
            libc::shm_unlink(self.key.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_can_move_to_its_caller_thread() {
        fn send<T: Send>() {}
        send::<Triton>();
    }

    #[test]
    fn the_request_names_both_regions_and_carries_no_tensor_bytes() {
        let input = Region::create(format!("scz-test-{}-in", std::process::id()), INPUT_BYTES).unwrap();
        let output = Region::create(format!("scz-test-{}-out", std::process::id()), OUTPUT_BYTES).unwrap();
        let r = ModelInferRequest::decode(&*infer_request("fraud", &input, &output)).unwrap();
        assert_eq!(r.model_name, "fraud");
        assert!(r.raw_input_contents.is_empty());
        let [i] = r.inputs.as_slice() else { panic!("{:?}", r.inputs) };
        assert_eq!((i.name.as_str(), i.datatype.as_str(), i.shape.as_slice()), ("features", "FP32", &[1, 256][..]));
        let region = |p: &std::collections::HashMap<String, InferParameter>| p["shared_memory_region"].parameter_choice.clone();
        assert_eq!(region(&i.parameters), Some(ParameterChoice::StringParam(input.name.clone())));
        assert_eq!(i.parameters["shared_memory_byte_size"].parameter_choice, Some(ParameterChoice::Int64Param(1024)));
        let [o] = r.outputs.as_slice() else { panic!("{:?}", r.outputs) };
        assert_eq!(o.name, "score");
        assert_eq!(region(&o.parameters), Some(ParameterChoice::StringParam(output.name.clone())));
        assert_eq!(o.parameters["shared_memory_byte_size"].parameter_choice, Some(ParameterChoice::Int64Param(4)));
    }

    #[test]
    fn a_region_is_unlinked_when_dropped() {
        let name = format!("scz-test-{}-unlink", std::process::id());
        let path = format!("/dev/shm/{name}");
        let r = Region::create(name, INPUT_BYTES).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), INPUT_BYTES as u64);
        drop(r);
        assert!(std::fs::metadata(&path).is_err());
    }

    #[test]
    fn a_deadline_fires_when_due_and_not_on_an_earlier_expiry() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let d = {
            let _in_rt = rt.enter();
            Deadline::new().unwrap()
        };
        let wait = |after: Duration| {
            d.arm(after);
            let t = std::time::Instant::now();
            rt.block_on(d.expired());
            d.disarm();
            t.elapsed()
        };
        let took = wait(Duration::from_millis(2));
        assert!(took >= Duration::from_millis(2) && took < Duration::from_millis(50), "{took:?}");
        // An expiry nobody waited for, which the reactor sees and caches as readiness while it
        // runs something else; then a longer deadline, which the stale readiness must not end.
        d.arm(Duration::from_millis(1));
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
        let took = wait(Duration::from_millis(20));
        assert!(took >= Duration::from_millis(20), "{took:?}");
    }

    #[test]
    fn a_bad_tag_or_endpoint_fails_without_panicking() {
        assert!(Triton::connect("http://127.0.0.1:1", "fraud", "no/slash").err().unwrap().contains("tag"));
        assert!(Triton::connect("not a uri", "fraud", "t").is_err());
        // Nothing listens on port 1: the connection is refused.
        assert!(Triton::connect("http://127.0.0.1:1", "fraud", "t").is_err());
    }
}
