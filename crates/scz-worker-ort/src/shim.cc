// A narrow C interface over ONNX Runtime's C++ API, for scz-worker-ort (worker §7.1).
//
// The calls are the ones spike S5 measured against the same pinned release: a session made
// from the mapped model bytes, used in place; tensors created over caller memory; outputs
// pre-allocated and bound through IO binding, so the runtime writes the answer once, into
// its final location; the two memory profiles of worker §5. Nothing here throws across the
// C boundary.

#include <onnxruntime_cxx_api.h>
#include <onnxruntime_run_options_config_keys.h>
#include <onnxruntime_session_options_config_keys.h>

#include <malloc.h>

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

extern "C" {

struct scz_ort_opts {
  int32_t profile;     // 0: A, arena off; 1: B, capped arena
  uint64_t arena_max;  // profile B's cap, bytes
  int32_t prepack;     // 1: weight prepacking on (the runtime's default)
  int32_t ort_format;  // 1: the bytes are an ORT-format model, used in place
};

struct scz_ort_io {
  char name[128];
  int32_t elem;  // ONNXTensorElementDataType
  int32_t rank;
  int64_t dims[8];  // -1 for a symbolic dimension
};

struct scz_ort_tensor {
  void* data;
  uint64_t bytes;
  int32_t elem;
  int32_t rank;
  int64_t dims[8];
};

struct scz_ort;

int scz_ort_open(const void* bytes, uint64_t len, const scz_ort_opts* o, scz_ort** out, char* err, uint64_t errlen);
uint64_t scz_ort_count(const scz_ort* s, int32_t outputs);
int scz_ort_info(const scz_ort* s, int32_t outputs, uint64_t i, scz_ort_io* info);
int scz_ort_run(scz_ort* s, const scz_ort_tensor* in, uint64_t nin, const scz_ort_tensor* out, uint64_t nout, int32_t shrink,
                int32_t* in_place, char* err, uint64_t errlen);
void scz_ort_close(scz_ort* s);
void scz_ort_trim(void);
}

namespace {

void put_err(char* err, uint64_t errlen, const std::string& m) {
  if (err && errlen) {
    std::snprintf(err, errlen, "%s", m.c_str());
  }
}

// One environment per process, made with the first session. Profile B's capped arena is an
// allocator registered on the environment: the C API cannot give a session's own arena a cap.
std::mutex env_mu;
Ort::Env* env_ptr = nullptr;
bool env_arena = false;

Ort::Env& env() {
  // Errors only: under the sandbox the runtime's device discovery and telemetry warn on every
  // start about paths they may not read, which says nothing about the model.
  if (!env_ptr) env_ptr = new Ort::Env(ORT_LOGGING_LEVEL_ERROR, "sidecarzero-worker");
  return *env_ptr;
}

}  // namespace

struct scz_ort {
  std::unique_ptr<Ort::Session> session;
  std::vector<std::string> in_names, out_names;
  std::vector<scz_ort_io> ins, outs;
  Ort::MemoryInfo mem = Ort::MemoryInfo::CreateCpu(OrtDeviceAllocator, OrtMemTypeDefault);
};

static void describe(const Ort::TypeInfo& ti, const std::string& name, scz_ort_io* io) {
  std::memset(io, 0, sizeof *io);
  std::snprintf(io->name, sizeof io->name, "%s", name.c_str());
  if (ti.GetONNXType() != ONNX_TYPE_TENSOR) {
    io->elem = -1;
    return;
  }
  auto t = ti.GetTensorTypeAndShapeInfo();
  io->elem = static_cast<int32_t>(t.GetElementType());
  auto shape = t.GetShape();
  io->rank = static_cast<int32_t>(shape.size());
  for (size_t k = 0; k < shape.size() && k < 8; k++) io->dims[k] = shape[k];
}

int scz_ort_open(const void* bytes, uint64_t len, const scz_ort_opts* o, scz_ort** out, char* err, uint64_t errlen) {
  try {
    std::lock_guard<std::mutex> g(env_mu);
    Ort::SessionOptions so;
    // Small, fixed pools, and no spinning when idle (worker §5).
    so.SetIntraOpNumThreads(1);
    so.SetInterOpNumThreads(1);
    so.AddConfigEntry(kOrtSessionOptionsConfigAllowIntraOpSpinning, "0");
    so.AddConfigEntry(kOrtSessionOptionsConfigAllowInterOpSpinning, "0");
    if (!o->prepack) so.AddConfigEntry(kOrtSessionOptionsConfigDisablePrepacking, "1");
    if (o->profile == 0) {
      so.DisableCpuMemArena();
      so.DisableMemPattern();
    } else {
      if (!env_arena) {
        Ort::ArenaCfg cfg(static_cast<size_t>(o->arena_max), /*kSameAsRequested*/ 1, -1, -1);
        env().CreateAndRegisterAllocator(Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault), cfg);
        env_arena = true;
      }
      so.EnableCpuMemArena();
      so.AddConfigEntry(kOrtSessionOptionsConfigUseEnvAllocators, "1");
    }
    if (o->ort_format) {
      // Use the mapped bytes in place; the mapping outlives the session.
      so.AddConfigEntry(kOrtSessionOptionsConfigUseORTModelBytesDirectly, "1");
      so.AddConfigEntry(kOrtSessionOptionsConfigUseORTModelBytesForInitializers, "1");
    }
    auto s = std::make_unique<scz_ort>();
    s->session = std::make_unique<Ort::Session>(env(), bytes, static_cast<size_t>(len), so);
    Ort::AllocatorWithDefaultOptions a;
    for (size_t i = 0; i < s->session->GetInputCount(); i++) {
      s->in_names.emplace_back(s->session->GetInputNameAllocated(i, a).get());
      scz_ort_io io;
      describe(s->session->GetInputTypeInfo(i), s->in_names.back(), &io);
      s->ins.push_back(io);
    }
    for (size_t i = 0; i < s->session->GetOutputCount(); i++) {
      s->out_names.emplace_back(s->session->GetOutputNameAllocated(i, a).get());
      scz_ort_io io;
      describe(s->session->GetOutputTypeInfo(i), s->out_names.back(), &io);
      s->outs.push_back(io);
    }
    *out = s.release();
    return 0;
  } catch (const std::exception& e) {
    put_err(err, errlen, e.what());
    return 1;
  }
}

uint64_t scz_ort_count(const scz_ort* s, int32_t outputs) { return outputs ? s->outs.size() : s->ins.size(); }

int scz_ort_info(const scz_ort* s, int32_t outputs, uint64_t i, scz_ort_io* info) {
  const auto& v = outputs ? s->outs : s->ins;
  if (i >= v.size()) return 1;
  *info = v[i];
  return 0;
}

static Ort::Value tensor(const Ort::MemoryInfo& mi, const scz_ort_tensor& t) {
  return Ort::Value::CreateTensor(mi, t.data, static_cast<size_t>(t.bytes), t.dims, static_cast<size_t>(t.rank),
                                  static_cast<ONNXTensorElementDataType>(t.elem));
}

// 0: OK. 1: the runtime refused or failed (MODEL_ERROR). 2: it failed to allocate
// (RESOURCE_EXHAUSTED; both of worker §5's caps surface as a general error naming the
// allocation).
int scz_ort_run(scz_ort* s, const scz_ort_tensor* in, uint64_t nin, const scz_ort_tensor* out, uint64_t nout, int32_t shrink,
                int32_t* in_place, char* err, uint64_t errlen) {
  try {
    Ort::IoBinding b(*s->session);
    std::vector<Ort::Value> keep;
    keep.reserve(nin + nout);
    for (uint64_t i = 0; i < nin && i < s->in_names.size(); i++) {
      keep.push_back(tensor(s->mem, in[i]));
      b.BindInput(s->in_names[i].c_str(), keep.back());
    }
    for (uint64_t i = 0; i < nout && i < s->out_names.size(); i++) {
      keep.push_back(tensor(s->mem, out[i]));
      b.BindOutput(s->out_names[i].c_str(), keep.back());
    }
    Ort::RunOptions ro;
    if (shrink) ro.AddConfigEntry(kOrtRunOptionsConfigEnableMemoryArenaShrinkage, "cpu:0");
    s->session->Run(ro, b);
    // The answer must be in the caller's memory: each bound output still points there.
    int32_t ok = 1;
    auto vals = b.GetOutputValues();
    for (uint64_t i = 0; i < vals.size() && i < nout; i++) {
      if (vals[i].GetTensorRawData() != out[i].data) ok = 0;
    }
    *in_place = ok;
    return 0;
  } catch (const std::exception& e) {
    std::string m = e.what();
    put_err(err, errlen, m);
    bool alloc = m.find("llocat") != std::string::npos || m.find("bad_alloc") != std::string::npos || m.find("memory") != std::string::npos;
    return alloc ? 2 : 1;
  }
}

void scz_ort_close(scz_ort* s) { delete s; }

// Return freed memory to the system (worker §5: glibc's allocator, purged on TRIM).
void scz_ort_trim(void) { malloc_trim(0); }
