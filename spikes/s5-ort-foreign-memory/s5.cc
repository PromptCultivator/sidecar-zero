// Spike S5 - ONNX Runtime over foreign memory. Throwaway code: clarity over polish.
//
//   s5 options                                   config keys: accepted? (sub-question e)
//   s5 zerocopy <model> <rows> <profile>         run over a memfd-backed "slot", prove no copy (a)
//   s5 latency  <model> <variant> <calls> <rows> <profile>   fixed cost per Run (b)
//   s5 mem      <model> <profile> <csv>          RSS over a long loop of varying batch sizes (c)
//   s5 load     <model> <variant> [--hold]       model from a mapped descriptor, smaps accounting (d)
//   s5 rlimit   <model>                          RLIMIT_DATA backstop under profile A (extra)
//
// Profiles are the two in docs/components/05-worker.md section 5:
//   A          DisableCpuMemArena + DisableMemPattern
//   B          arena on, arena_extend_strategy = kSameAsRequested, max_mem set (OrtArenaCfg)
//   B-default  arena on with ONNX Runtime's defaults (no cap), for comparison
#include <onnxruntime_cxx_api.h>
#include <onnxruntime_run_options_config_keys.h>
#include <onnxruntime_session_options_config_keys.h>

#include <dlfcn.h>
#include <fcntl.h>
#include <malloc.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

#include <algorithm>
#include <cinttypes>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iostream>
#include <iterator>
#include <memory>
#include <random>
#include <string>
#include <vector>

// Present only when libmhook.so is preloaded (zero-copy test).
extern "C" {
void mhook_begin(void) __attribute__((weak));
size_t mhook_end(size_t* bytes, const size_t** sizes) __attribute__((weak));
// Present only when jemalloc is preloaded.
int mallctl(const char* name, void* oldp, size_t* oldlenp, void* newp, size_t newlen) __attribute__((weak));
}

#define DIE(...)                      \
  do {                                \
    fprintf(stderr, "FATAL: ");       \
    fprintf(stderr, __VA_ARGS__);     \
    fputc('\n', stderr);              \
    exit(2);                          \
  } while (0)

static const int64_t N_FEAT = 256;
static size_t g_page = 4096;

static uint64_t now_ns() {
  timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return uint64_t(ts.tv_sec) * 1000000000ull + uint64_t(ts.tv_nsec);
}
static size_t round_up(size_t v, size_t a) { return (v + a - 1) / a * a; }
static bool ends_with(const std::string& s, const std::string& suf) {
  return s.size() >= suf.size() && s.compare(s.size() - suf.size(), suf.size(), suf) == 0;
}

// ---------------------------------------------------------------------------------------------
// /proc readers
// ---------------------------------------------------------------------------------------------
struct Statm {
  long rss_kb, shared_kb;  // resident; resident file + shmem pages
};
static Statm read_statm() {
  FILE* f = fopen("/proc/self/statm", "r");
  long size = 0, res = 0, shared = 0;
  if (!f || fscanf(f, "%ld %ld %ld", &size, &res, &shared) != 3) DIE("statm");
  fclose(f);
  return {long(res * g_page / 1024), long(shared * g_page / 1024)};
}
static long kv_kb(const char* file, const char* key) {  // "Key:   123 kB" from /proc/self/<file>
  std::ifstream f(std::string("/proc/self/") + file);
  std::string line, want = std::string(key) + ":";
  while (std::getline(f, line))
    if (line.compare(0, want.size(), want) == 0) return atol(line.c_str() + want.size());
  return -1;
}
static void reset_hwm() {  // reset VmHWM (peak RSS) so each phase reports its own peak
  FILE* f = fopen("/proc/self/clear_refs", "w");
  if (f) { fputs("5", f); fclose(f); }
}

struct SmapsSum {
  long size = 0, rss = 0, pss = 0, sh_clean = 0, sh_dirty = 0, pr_clean = 0, pr_dirty = 0, anon = 0;
  int vmas = 0;
  std::string perms;
};
// Sum every VMA in /proc/self/smaps whose backing path is `path` (whoever created the mapping).
static SmapsSum smaps_for_path(const std::string& path) {
  SmapsSum s;
  std::ifstream f("/proc/self/smaps");
  std::string line;
  bool in = false;
  while (std::getline(f, line)) {
    unsigned long a, b, off;
    char perms[8];
    int n = 0;
    if (sscanf(line.c_str(), "%lx-%lx %7s %lx %*s %*s%n", &a, &b, perms, &off, &n) == 4) {
      std::string name = line.substr(n);
      size_t i = name.find_first_not_of(' ');
      name = i == std::string::npos ? "" : name.substr(i);
      in = (name == path);
      if (in) { s.vmas++; s.perms = perms; }
    } else if (in) {
      char key[64];
      long v;
      if (sscanf(line.c_str(), "%63[^:]: %ld kB", key, &v) == 2) {
        std::string k = key;
        if (k == "Size") s.size += v;
        else if (k == "Rss") s.rss += v;
        else if (k == "Pss") s.pss += v;
        else if (k == "Shared_Clean") s.sh_clean += v;
        else if (k == "Shared_Dirty") s.sh_dirty += v;
        else if (k == "Private_Clean") s.pr_clean += v;
        else if (k == "Private_Dirty") s.pr_dirty += v;
        else if (k == "Anonymous") s.anon += v;
      }
    }
  }
  return s;
}

// ---------------------------------------------------------------------------------------------
// A stand-in for one slot: a memfd, mapped shared. Request values and response values sit at
// page-aligned (hence 64-byte-aligned) offsets, the way a capacity template would place them.
// ---------------------------------------------------------------------------------------------
struct Slot {
  int fd = -1;
  uint8_t* base = nullptr;
  size_t len = 0, req_off = 0, req_cap = 0, resp_off = 0, resp_cap = 0;
  float* in() const { return reinterpret_cast<float*>(base + req_off); }
  float* out() const { return reinterpret_cast<float*>(base + resp_off); }
};
static Slot make_slot(size_t req_bytes, size_t resp_bytes) {
  Slot s;
  s.req_off = g_page;  // page 0 stands in for the frame prefix
  s.req_cap = round_up(req_bytes, g_page);
  s.resp_off = s.req_off + s.req_cap + g_page;  // one spare page between the regions
  s.resp_cap = round_up(resp_bytes, g_page);
  s.len = s.resp_off + s.resp_cap + g_page;
  s.fd = memfd_create("scz-spike-s5-slot", MFD_CLOEXEC);
  if (s.fd < 0 || ftruncate(s.fd, off_t(s.len)) != 0) DIE("memfd: %s", strerror(errno));
  void* p = mmap(nullptr, s.len, PROT_READ | PROT_WRITE, MAP_SHARED, s.fd, 0);
  if (p == MAP_FAILED) DIE("mmap slot: %s", strerror(errno));
  s.base = static_cast<uint8_t*>(p);
  return s;
}
static void fill_random(float* p, size_t n, unsigned seed) {
  std::mt19937 g(seed);
  std::normal_distribution<float> d(0.f, 1.f);
  for (size_t i = 0; i < n; i++) p[i] = d(g);
}

// ---------------------------------------------------------------------------------------------
// Session construction
// ---------------------------------------------------------------------------------------------
static size_t env_size(const char* name, size_t dflt) {
  const char* v = getenv(name);
  return v ? size_t(strtoull(v, nullptr, 10)) : dflt;
}

// Options common to both profiles in the design: one intra-op thread, one inter-op thread, no spinning.
static Ort::SessionOptions base_options() {
  Ort::SessionOptions so;
  so.SetIntraOpNumThreads(1);
  so.SetInterOpNumThreads(1);
  so.SetExecutionMode(ORT_SEQUENTIAL);
  so.AddConfigEntry(kOrtSessionOptionsConfigAllowIntraOpSpinning, "0");
  so.AddConfigEntry(kOrtSessionOptionsConfigAllowInterOpSpinning, "0");
  // Not in the design. S5_DISABLE_PREPACK=1 turns off kernels' PrePack() of constant weights, which
  // otherwise re-lays the weights out in memory the runtime allocates at session creation.
  if (env_size("S5_DISABLE_PREPACK", 0) == 1) so.AddConfigEntry(kOrtSessionOptionsConfigDisablePrepacking, "1");
  return so;
}
static const char* prepack_note() {
  return env_size("S5_DISABLE_PREPACK", 0) == 1 ? "session.disable_prepacking=1" : "prepacking on (default)";
}

static void apply_profile(Ort::Env& env, Ort::SessionOptions& so, const std::string& profile, bool verbose) {
  if (profile == "A") {
    so.DisableCpuMemArena();
    so.DisableMemPattern();
    if (verbose) printf("profile A: DisableCpuMemArena() ok, DisableMemPattern() ok\n");
  } else if (profile == "B-default") {
    so.EnableCpuMemArena();
    if (verbose) printf("profile B-default: EnableCpuMemArena() ok; ONNX Runtime's default arena (no cap)\n");
  } else if (profile == "B") {
    // The C API has no way to hand an OrtArenaCfg to a session's own CPU arena. The only route is an
    // allocator registered on the environment, which the session is then told to use.
    size_t cap = env_size("S5_ARENA_MAX_MEM", size_t(128) << 20);
    Ort::ArenaCfg cfg(cap, /*arena_extend_strategy: 1 = kSameAsRequested*/ 1, -1, -1);
    Ort::MemoryInfo mi = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);
    env.CreateAndRegisterAllocator(mi, cfg);
    so.EnableCpuMemArena();
    so.AddConfigEntry(kOrtSessionOptionsConfigUseEnvAllocators, "1");
    if (verbose)
      printf("profile B: CreateArenaCfg(max_mem=%zu, arena_extend_strategy=1 kSameAsRequested) ok, "
             "CreateAndRegisterAllocator(env, Cpu/OrtArenaAllocator) ok, EnableCpuMemArena() ok, "
             "AddSessionConfigEntry(\"%s\",\"1\") ok\n",
             cap, kOrtSessionOptionsConfigUseEnvAllocators);
  } else {
    DIE("unknown profile %s", profile.c_str());
  }
}

struct Mapped {
  void* addr = nullptr;
  size_t len = 0;
};
static Mapped map_file_ro(const std::string& path) {
  int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
  if (fd < 0) DIE("open %s: %s", path.c_str(), strerror(errno));
  struct stat st;
  fstat(fd, &st);
  void* p = mmap(nullptr, size_t(st.st_size), PROT_READ, MAP_SHARED, fd, 0);
  if (p == MAP_FAILED) DIE("mmap %s: %s", path.c_str(), strerror(errno));
  close(fd);  // the mapping outlives the descriptor
  return {p, size_t(st.st_size)};
}

// How every test except `load` gets its session: the way the design says the worker will. The file is
// mapped read-only and the session is created from that buffer; for an ORT-format model the two
// "use the bytes in place" options are set. The mapping is leaked on purpose (it must outlive the session).
static std::unique_ptr<Ort::Session> load_as_designed(Ort::Env& env, Ort::SessionOptions& so, const std::string& path) {
  Mapped m = map_file_ro(path);
  if (ends_with(path, ".ort")) {
    so.AddConfigEntry(kOrtSessionOptionsConfigUseORTModelBytesDirectly, "1");
    so.AddConfigEntry(kOrtSessionOptionsConfigUseORTModelBytesForInitializers, "1");
  }
  return std::make_unique<Ort::Session>(env, m.addr, m.len, so);
}

static const char* IN_NAMES[] = {"features"};
static const char* OUT_NAMES[] = {"score"};

// ---------------------------------------------------------------------------------------------
// options: are the keys accepted?
// ---------------------------------------------------------------------------------------------
static int cmd_options() {
  printf("ONNX Runtime version string (OrtGetApiBase()->GetVersionString()): %s\n", Ort::GetVersionString().c_str());
  printf("ORT_API_VERSION compiled against: %d\n", ORT_API_VERSION);
  Ort::SessionOptions so;
  const char* keys[] = {kOrtSessionOptionsConfigUseORTModelBytesDirectly,
                        kOrtSessionOptionsConfigUseORTModelBytesForInitializers,
                        kOrtSessionOptionsConfigUseEnvAllocators,
                        kOrtSessionOptionsConfigAllowIntraOpSpinning,
                        "session.this_key_does_not_exist"};
  for (const char* k : keys) {
    try {
      so.AddConfigEntry(k, "1");
      printf("AddSessionConfigEntry(\"%s\", \"1\") -> ok\n", k);
    } catch (const Ort::Exception& e) {
      printf("AddSessionConfigEntry(\"%s\", \"1\") -> ERROR %s\n", k, e.what());
    }
  }
  Ort::RunOptions ro;
  for (const char* k : {kOrtRunOptionsConfigEnableMemoryArenaShrinkage, "memory.this_key_does_not_exist"}) {
    try {
      ro.AddConfigEntry(k, "cpu:0");
      printf("AddRunConfigEntry(\"%s\", \"cpu:0\") -> ok\n", k);
    } catch (const Ort::Exception& e) {
      printf("AddRunConfigEntry(\"%s\", \"cpu:0\") -> ERROR %s\n", k, e.what());
    }
  }
  printf("note: a config entry is a string in a map. A made-up key is accepted too, so acceptance alone\n"
         "      proves nothing. Whether a key has an effect is shown by behaviour, in the other tests.\n");
  return 0;
}

// ---------------------------------------------------------------------------------------------
// zerocopy
// ---------------------------------------------------------------------------------------------
struct FaultRec {
  void* addr;
  void* pc;
  int region;  // 0 request, 1 response
  size_t page;
  char kind;  // 'r' first touch of a request page, 'W' second fault on a request page (a write), 'w' write to response
};
static FaultRec g_faults[512];
static volatile int g_nfaults;
static uint8_t *g_in_lo, *g_in_hi, *g_out_lo, *g_out_hi;
static uint8_t g_in_state[4096];

static void on_segv(int, siginfo_t* si, void* uctx) {
  uint8_t* a = static_cast<uint8_t*>(si->si_addr);
  ucontext_t* uc = static_cast<ucontext_t*>(uctx);
#if defined(__aarch64__)
  void* pc = reinterpret_cast<void*>(uc->uc_mcontext.pc);
#elif defined(__x86_64__)
  void* pc = reinterpret_cast<void*>(uc->uc_mcontext.gregs[REG_RIP]);
#else
#error "unsupported architecture"
#endif
  uint8_t* pg = reinterpret_cast<uint8_t*>(reinterpret_cast<uintptr_t>(a) & ~(uintptr_t(g_page) - 1));
  int n = g_nfaults;
  if (a >= g_in_lo && a < g_in_hi) {
    size_t idx = size_t(pg - g_in_lo) / g_page;
    int st = g_in_state[idx];
    // First fault on a request page: grant read. A second fault on the same page can only be a write.
    mprotect(pg, g_page, st == 0 ? PROT_READ : PROT_READ | PROT_WRITE);
    g_in_state[idx] = uint8_t(st + 1);
    if (n < 512) g_faults[n] = {a, pc, 0, idx, st == 0 ? 'r' : 'W'};
  } else if (a >= g_out_lo && a < g_out_hi) {
    mprotect(pg, g_page, PROT_READ | PROT_WRITE);  // response pages were readable: this fault is a write
    if (n < 512) g_faults[n] = {a, pc, 1, size_t(pg - g_out_lo) / g_page, 'w'};
  } else {
    signal(SIGSEGV, SIG_DFL);  // a real crash
    return;
  }
  g_nfaults = n + 1;
}

static void print_alloc_trace(const char* what, size_t n, size_t bytes, const size_t* sizes) {
  printf("  %-52s allocations=%zu bytes=%zu sizes=[", what, n, bytes);
  for (size_t i = 0; i < n && i < 40; i++) printf("%s%zu", i ? "," : "", sizes[i]);
  printf("%s]\n", n > 40 ? ",..." : "");
}

static int cmd_zerocopy(const std::string& model, int64_t rows, const std::string& profile) {
  printf("== zerocopy model=%s rows=%" PRId64 " profile=%s\n", model.c_str(), rows, profile.c_str());
  Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "s5");
  Ort::SessionOptions so = base_options();
  apply_profile(env, so, profile, false);
  auto sess = load_as_designed(env, so, model);
  Ort::Session& s = *sess;

  const size_t in_bytes = size_t(rows) * N_FEAT * 4, out_bytes = size_t(rows) * 4;
  Slot slot = make_slot(in_bytes, out_bytes);
  fill_random(slot.in(), size_t(rows) * N_FEAT, 1234);
  memset(slot.base + slot.resp_off, 0xA5, slot.resp_cap);
  printf("slot mapping        [%p, %p)  memfd, MAP_SHARED, %zu bytes\n", (void*)slot.base, (void*)(slot.base + slot.len), slot.len);
  printf("request values at   %p = mapping+0x%zx, %zu bytes (address %% 64 = %zu)\n", (void*)slot.in(), slot.req_off, in_bytes,
         size_t(reinterpret_cast<uintptr_t>(slot.in()) % 64));
  printf("response values at  %p = mapping+0x%zx, %zu bytes (address %% 64 = %zu)\n", (void*)slot.out(), slot.resp_off, out_bytes,
         size_t(reinterpret_cast<uintptr_t>(slot.out()) % 64));

  Ort::MemoryInfo mi = Ort::MemoryInfo::CreateCpu(OrtDeviceAllocator, OrtMemTypeDefault);
  int64_t in_shape[2] = {rows, N_FEAT}, out_shape[2] = {rows, 1};
  bool ok = true;

  // Reference: the runtime's normal API. Private copy of the input, output allocated by the runtime.
  std::vector<float> priv_in(slot.in(), slot.in() + rows * N_FEAT);
  std::vector<float> ref(static_cast<size_t>(rows));
  {
    Ort::Value rin = Ort::Value::CreateTensor<float>(mi, priv_in.data(), priv_in.size(), in_shape, 2);
    Ort::Value rout{nullptr};
    s.Run(Ort::RunOptions{nullptr}, IN_NAMES, &rin, 1, OUT_NAMES, &rout, 1);
    const float* p = rout.GetTensorData<float>();
    bool outside = (const uint8_t*)p < slot.base || (const uint8_t*)p >= slot.base + slot.len;
    printf("reference run (normal API): output tensor allocated by the runtime at %p, %s the slot mapping\n", (const void*)p,
           outside ? "outside" : "INSIDE");
    memcpy(ref.data(), p, out_bytes);
  }

  // The fast path: both tensors are created over slot memory, the output is bound.
  Ort::Value in = Ort::Value::CreateTensor<float>(mi, slot.in(), size_t(rows) * N_FEAT, in_shape, 2);
  Ort::Value out = Ort::Value::CreateTensor<float>(mi, slot.out(), size_t(rows), out_shape, 2);
  Ort::IoBinding binding(s);
  binding.BindInput("features", in);
  binding.BindOutput("score", out);
  Ort::RunOptions ro;
  s.Run(ro, binding);  // warm-up: per-thread scratch is created now, not in the measured run

  // 1. Pointers.
  printf("[1] pointers\n");
  bool p_in = in.GetTensorMutableData<float>() == slot.in();
  printf("  input  OrtValue data pointer  %p  %s request values address\n", (void*)in.GetTensorMutableData<float>(), p_in ? "==" : "!=");
  std::vector<Ort::Value> bound = binding.GetOutputValues();
  bool p_out = bound.size() == 1 && bound[0].GetTensorMutableData<float>() == slot.out();
  printf("  bound output (GetBoundOutputValues) data pointer  %p  %s response values address\n",
         (void*)bound[0].GetTensorMutableData<float>(), p_out ? "==" : "!=");
  ok = ok && p_in && p_out;

  // 2. Bytes.
  printf("[2] bytes\n");
  memset(slot.base + slot.resp_off, 0xA5, slot.resp_cap);
  s.Run(ro, binding);
  bool same = memcmp(slot.out(), ref.data(), out_bytes) == 0;
  bool tail = true;
  for (size_t i = out_bytes; i < slot.resp_cap; i++) tail = tail && slot.base[slot.resp_off + i] == 0xA5;
  std::vector<uint8_t> via_fd(out_bytes);
  bool fd_same = pread(slot.fd, via_fd.data(), out_bytes, off_t(slot.resp_off)) == ssize_t(out_bytes) &&
                 memcmp(via_fd.data(), ref.data(), out_bytes) == 0;
  printf("  response region equals reference output bit for bit (%zu bytes): %s\n", out_bytes, same ? "yes" : "NO");
  printf("  bytes after the %zu response bytes still hold the 0xA5 sentinel (%zu bytes checked): %s\n", out_bytes,
         slot.resp_cap - out_bytes, tail ? "yes" : "NO");
  printf("  pread() of the memfd at the response offset returns the same bytes (seen through the descriptor, not our mapping): %s\n",
         fd_same ? "yes" : "NO");
  printf("  first scores: %.7g %.7g ...\n", slot.out()[0], rows > 1 ? slot.out()[1] : slot.out()[0]);
  ok = ok && same && tail && fd_same;

  // 3. Allocations during Run. With profile A every tensor the runtime allocates is one malloc, so a
  //    copy of the request (in_bytes) or of the response (out_bytes) would be visible by its size.
  printf("[3] allocations made inside Run (recorded by libmhook.so)\n");
  if (!mhook_begin) {
    printf("  libmhook.so not preloaded: skipped\n");
  } else {
    size_t bytes = 0;
    const size_t* sizes = nullptr;
    mhook_begin();
    void* a = malloc(12345);
    free(a);
    {
      Ort::AllocatorWithDefaultOptions alloc;
      void* b = alloc.Alloc(54321);
      alloc.Free(b);
    }
    size_t n = mhook_end(&bytes, &sizes);
    print_alloc_trace("self-check: malloc(12345) + OrtAllocator.Alloc(54321)", n, bytes, sizes);

    std::vector<size_t> control, fast;
    {
      Ort::Value rout{nullptr};
      mhook_begin();
      s.Run(ro, IN_NAMES, &in, 1, OUT_NAMES, &rout, 1);
      n = mhook_end(&bytes, &sizes);
      char label[96];
      snprintf(label, sizeof label, "control: output NOT bound (runtime allocates %zu B)", out_bytes);
      print_alloc_trace(label, n, bytes, sizes);
      control.assign(sizes, sizes + std::min<size_t>(n, 4096));
    }
    mhook_begin();
    s.Run(ro, binding);
    n = mhook_end(&bytes, &sizes);
    print_alloc_trace("fast path: input over slot, output bound to slot", n, bytes, sizes);
    fast.assign(sizes, sizes + std::min<size_t>(n, 4096));

    // What does the control allocate that the fast path does not, and the other way round?
    std::vector<size_t> c = control, f = fast, only_control, only_fast;
    std::sort(c.begin(), c.end());
    std::sort(f.begin(), f.end());
    std::set_difference(c.begin(), c.end(), f.begin(), f.end(), std::back_inserter(only_control));
    std::set_difference(f.begin(), f.end(), c.begin(), c.end(), std::back_inserter(only_fast));
    printf("  allocated only by the control: [");
    for (size_t i = 0; i < only_control.size(); i++) printf("%s%zu", i ? "," : "", only_control[i]);
    printf("]   only by the fast path: [");
    for (size_t i = 0; i < only_fast.size(); i++) printf("%s%zu", i ? "," : "", only_fast[i]);
    printf("]\n");
    printf("  fast path allocations big enough to hold the request (>= %zu B): [", in_bytes);
    bool first = true;
    for (size_t v : fast)
      if (v >= in_bytes) { printf("%s%zu", first ? "" : ",", v); first = false; }
    printf("]\n");
  }

  // 4. Who touches slot memory? Make request pages inaccessible and response pages read-only, run,
  //    and record the program counter of each first touch.
  printf("[4] first touch of each slot page during Run (mprotect + SIGSEGV)\n");
  memset(slot.base + slot.resp_off, 0xA5, slot.resp_cap);
  g_in_lo = slot.base + slot.req_off;
  g_in_hi = g_in_lo + slot.req_cap;
  g_out_lo = slot.base + slot.resp_off;
  g_out_hi = g_out_lo + slot.resp_cap;
  struct sigaction sa = {};
  sa.sa_sigaction = on_segv;
  sa.sa_flags = SA_SIGINFO;
  sigaction(SIGSEGV, &sa, nullptr);
  mprotect(g_in_lo, slot.req_cap, PROT_NONE);
  mprotect(g_out_lo, slot.resp_cap, PROT_READ);
  s.Run(ro, binding);
  mprotect(g_in_lo, slot.req_cap, PROT_READ | PROT_WRITE);
  mprotect(g_out_lo, slot.resp_cap, PROT_READ | PROT_WRITE);
  signal(SIGSEGV, SIG_DFL);
  int nf = g_nfaults;
  int reads = 0, in_writes = 0, out_writes = 0, in_ort = 0;
  for (int i = 0; i < nf && i < 512; i++) {
    const FaultRec& f = g_faults[i];
    Dl_info di = {};
    dladdr(f.pc, &di);
    const char* mod = di.dli_fname ? di.dli_fname : "?";
    uintptr_t off = di.dli_fbase ? uintptr_t(f.pc) - uintptr_t(di.dli_fbase) : 0;
    bool is_ort = strstr(mod, "libonnxruntime") != nullptr;
    in_ort += is_ort;
    reads += f.kind == 'r';
    in_writes += f.kind == 'W';
    out_writes += f.kind == 'w';
    printf("  FAULT %s page=%zu %s addr=%p pc=%p module=%s offset=0x%zx\n", f.region == 0 ? "request " : "response", f.page,
           f.kind == 'r' ? "read " : "WRITE", f.addr, f.pc, mod, size_t(off));
  }
  size_t in_pages = round_up(in_bytes, g_page) / g_page;
  printf("  request pages: %zu, each first read once: %s; writes to request pages: %d\n", in_pages,
         size_t(reads) == in_pages ? "yes" : "NO", in_writes);
  printf("  response pages written: %d; all %d faulting instructions are inside libonnxruntime: %s\n", out_writes, nf,
         in_ort == nf ? "yes" : "NO");
  bool same2 = memcmp(slot.out(), ref.data(), out_bytes) == 0;
  printf("  response still equals reference after the probe run: %s\n", same2 ? "yes" : "NO");
  ok = ok && same2 && in_writes == 0 && in_ort == nf && nf > 0;

  printf("RESULT zerocopy model=%s rows=%" PRId64 " profile=%s: %s\n\n", model.c_str(), rows, profile.c_str(), ok ? "PASS" : "FAIL");
  return ok ? 0 : 1;
}

// ---------------------------------------------------------------------------------------------
// latency
// ---------------------------------------------------------------------------------------------
static int cmd_latency(const std::string& model, const std::string& variant, size_t calls, int64_t rows, const std::string& profile) {
  int cpu = int(env_size("S5_PIN_CPU", 2));
  cpu_set_t set;
  CPU_ZERO(&set);
  CPU_SET(cpu, &set);
  bool pinned = sched_setaffinity(0, sizeof set, &set) == 0;

  Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "s5");
  Ort::SessionOptions so = base_options();
  apply_profile(env, so, profile, false);
  auto sess = load_as_designed(env, so, model);
  Ort::Session& s = *sess;

  // NS request areas and NS response areas in one memfd mapping, each 64-byte aligned, each holding a
  // different random request. Rotating through them stops the branch predictor from learning one path
  // through the trees, and is what a worker sees: a different slot on every call.
  const size_t NS = 256;
  const size_t req_sz = size_t(rows) * N_FEAT * 4, resp_sz = round_up(size_t(rows) * 4, 64);
  Slot slot = make_slot(NS * req_sz, NS * resp_sz);
  fill_random(slot.in(), NS * size_t(rows) * N_FEAT, 99);
  memset(slot.base + slot.resp_off, 0, slot.resp_cap);

  Ort::MemoryInfo mi = Ort::MemoryInfo::CreateCpu(OrtDeviceAllocator, OrtMemTypeDefault);
  int64_t in_shape[2] = {rows, N_FEAT}, out_shape[2] = {rows, 1};
  Ort::RunOptions ro;
  Ort::IoBinding binding(s);
  // "bound-once" binds scratch area 0 once and refreshes its contents (untimed) before each call.
  std::vector<float> spare(slot.in(), slot.in() + rows * N_FEAT);
  Ort::Value in0 = Ort::Value::CreateTensor<float>(mi, slot.in(), size_t(rows) * N_FEAT, in_shape, 2);
  Ort::Value out0 = Ort::Value::CreateTensor<float>(mi, slot.out(), size_t(rows), out_shape, 2);
  int mode = variant == "bound-once" ? 0 : variant == "bind-per-call" ? 1 : variant == "run-per-call" ? 2 : -1;
  if (mode < 0) DIE("unknown variant %s", variant.c_str());
  if (mode == 0) {
    binding.BindInput("features", in0);
    binding.BindOutput("score", out0);
  }

  // Resolution of the clock: the smallest non-zero step seen between back-to-back readings.
  uint64_t clock_step = UINT64_MAX;
  for (int i = 0; i < 200000; i++) {
    uint64_t a = now_ns(), d = now_ns() - a;
    if (d != 0 && d < clock_step) clock_step = d;
  }

  size_t warm = std::max<size_t>(std::min<size_t>(calls / 10, 10000), 200);
  std::vector<uint64_t> lat(calls);
  double checksum = 0;
  uint64_t t_begin = now_ns();
  for (size_t i = 0; i < warm + calls; i++) {
    size_t k = i % NS;
    float* x = slot.in() + k * size_t(rows) * N_FEAT;
    float* y = reinterpret_cast<float*>(slot.base + slot.resp_off + k * resp_sz);
    uint64_t t0, t1;
    if (mode == 0) {
      if (k != 0) memcpy(slot.in(), x, req_sz); else memcpy(slot.in(), spare.data(), req_sz);
      y = slot.out();
      t0 = now_ns();
      s.Run(ro, binding);
      t1 = now_ns();
    } else if (mode == 1) {
      // What §7.1 describes: two OrtValues over this request's slot, bound, run, released.
      t0 = now_ns();
      {
        Ort::Value in = Ort::Value::CreateTensor<float>(mi, x, size_t(rows) * N_FEAT, in_shape, 2);
        Ort::Value out = Ort::Value::CreateTensor<float>(mi, y, size_t(rows), out_shape, 2);
        binding.BindInput("features", in);
        binding.BindOutput("score", out);
        s.Run(ro, binding);
      }
      t1 = now_ns();
    } else {
      // The same without IO binding: Run() accepts a pre-allocated output OrtValue.
      t0 = now_ns();
      {
        Ort::Value in = Ort::Value::CreateTensor<float>(mi, x, size_t(rows) * N_FEAT, in_shape, 2);
        Ort::Value out = Ort::Value::CreateTensor<float>(mi, y, size_t(rows), out_shape, 2);
        s.Run(ro, IN_NAMES, &in, 1, OUT_NAMES, &out, 1);
      }
      t1 = now_ns();
    }
    if (i >= warm) lat[i - warm] = t1 - t0;
    checksum += double(y[0]);
  }
  double total_s = double(now_ns() - t_begin) / 1e9;
  std::sort(lat.begin(), lat.end());
  auto pct = [&](double p) { return lat[std::min(calls - 1, size_t(p * double(calls)))]; };
  double mean = 0;
  for (uint64_t v : lat) mean += double(v);
  mean /= double(calls);
  std::string name = model.substr(model.find_last_of('/') + 1);
  printf("latency model=%-9s rows=%-2" PRId64 " profile=%-2s prepack=%-3s variant=%-13s calls=%zu warmup=%zu pinned_cpu=%s "
         "min_ns=%" PRIu64 " p50_ns=%" PRIu64 " p90_ns=%" PRIu64 " p99_ns=%" PRIu64 " p999_ns=%" PRIu64 " max_ns=%" PRIu64
         " mean_ns=%.0f clock_step_ns=%" PRIu64 " wall_s=%.1f checksum=%.6g\n",
         name.c_str(), rows, profile.c_str(), env_size("S5_DISABLE_PREPACK", 0) == 1 ? "off" : "on", variant.c_str(), calls, warm,
         pinned ? std::to_string(cpu).c_str() : "no", lat[0], pct(0.50),
         pct(0.90), pct(0.99), pct(0.999), lat[calls - 1], mean, clock_step, total_s, checksum);
  return 0;
}

// ---------------------------------------------------------------------------------------------
// mem: RSS under each profile
// ---------------------------------------------------------------------------------------------
static const char* allocator_name() { return mallctl ? "jemalloc (LD_PRELOAD)" : "glibc malloc"; }
static void allocator_purge() {
  if (mallctl) {
    mallctl("thread.tcache.flush", nullptr, nullptr, nullptr, 0);
    int rc = mallctl("arena.4096.purge", nullptr, nullptr, nullptr, 0);  // 4096 = MALLCTL_ARENAS_ALL
    if (rc != 0) printf("  mallctl(arena.4096.purge) failed: %d\n", rc);
  } else {
    malloc_trim(0);
  }
}

static int cmd_mem(const std::string& model, const std::string& profile, const std::string& csv_path) {
  std::string name = model.substr(model.find_last_of('/') + 1);
  printf("== mem model=%s profile=%s allocator=%s %s\n", name.c_str(), profile.c_str(), allocator_name(), prepack_note());
  Ort::Env env(ORT_LOGGING_LEVEL_FATAL, "s5");
  Ort::SessionOptions so = base_options();
  apply_profile(env, so, profile, true);
  auto sess = load_as_designed(env, so, model);
  Ort::Session& s = *sess;

  const int64_t MAX_LOOP_ROWS = 2048, OVER_ROWS = 4096;
  Slot slot = make_slot(size_t(OVER_ROWS) * N_FEAT * 4, size_t(OVER_ROWS) * 4);
  fill_random(slot.in(), size_t(OVER_ROWS) * N_FEAT, 5);  // touch the whole slot now so its pages are in every sample
  memset(slot.out(), 0, size_t(OVER_ROWS) * 4);

  Ort::MemoryInfo mi = Ort::MemoryInfo::CreateCpu(OrtDeviceAllocator, OrtMemTypeDefault);
  Ort::IoBinding binding(s);
  long fails = 0, recovered = 0, refused = 0;
  std::string first_error;
  auto run_once = [&](int64_t rows, bool shrink) -> bool {
    int64_t in_shape[2] = {rows, N_FEAT}, out_shape[2] = {rows, 1};
    Ort::Value in = Ort::Value::CreateTensor<float>(mi, slot.in(), size_t(rows) * N_FEAT, in_shape, 2);
    Ort::Value out = Ort::Value::CreateTensor<float>(mi, slot.out(), size_t(rows), out_shape, 2);
    binding.BindInput("features", in);
    binding.BindOutput("score", out);
    Ort::RunOptions ro;
    if (shrink) ro.AddConfigEntry(kOrtRunOptionsConfigEnableMemoryArenaShrinkage, "cpu:0");
    try {
      s.Run(ro, binding);
      return true;
    } catch (const Ort::Exception& e) {
      fails++;
      if (first_error.empty()) first_error = e.what();
      return false;
    }
  };
  // Under an arena profile a refused request is retried once after a shrink run, which is what a worker
  // could do before answering RESOURCE_EXHAUSTED. Under profile A there is nothing to shrink.
  const bool arena = profile != "A";
  auto run = [&](int64_t rows, bool shrink) -> bool {
    if (run_once(rows, shrink)) return true;
    if (!arena || shrink) return false;
    run_once(1, true);
    if (run_once(rows, false)) { recovered++; fails--; return true; }
    fails--;  // count the request once
    refused++;
    return false;
  };

  FILE* csv = fopen(csv_path.c_str(), "w");
  if (!csv) DIE("open %s", csv_path.c_str());
  fprintf(csv, "phase,iter,rows,rss_kb,shared_kb,rss_minus_shared_kb\n");
  long iter = 0;
  auto sample = [&](const char* phase, int64_t rows) {
    Statm m = read_statm();
    fprintf(csv, "%s,%ld,%" PRId64 ",%ld,%ld,%ld\n", phase, iter, rows, m.rss_kb, m.shared_kb, m.rss_kb - m.shared_kb);
  };
  auto boundary = [&](const char* phase) {
    Statm m = read_statm();
    printf("  %-34s iter=%-6ld statm: rss=%7ld kB  shared(file+shmem)=%6ld kB  rss-shared=%7ld kB | status: RssAnon=%7ld kB  "
           "peak rss in phase (VmHWM)=%7ld kB | refused at first try=%ld, of which ran after a shrink=%ld, still refused=%ld\n",
           phase, iter, m.rss_kb, m.shared_kb, m.rss_kb - m.shared_kb, kv_kb("status", "RssAnon"), kv_kb("status", "VmHWM"), fails,
           recovered, refused);
    sample(phase, 0);
    reset_hwm();
  };

  std::mt19937 rng(42);
  auto small_rows = [&]() { return int64_t(1 + rng() % 32); };
  const int64_t big[4] = {256, 512, 1024, MAX_LOOP_ROWS};

  boundary("loaded");
  run(1, false);
  run(MAX_LOOP_ROWS, false);  // worker startup step 7: warm up at one row and at the largest row count
  run(1, false);
  boundary("warm (1 row, 2048 rows, 1 row)");

  const int cycles = int(env_size("S5_MEM_CYCLES", 5));
  for (int c = 1; c <= cycles; c++) {
    char label[64];
    for (int i = 0; i < 500; i++, iter++) {
      int64_t r = small_rows();
      run(r, false);
      if (i % 25 == 0) sample("small", r);
    }
    snprintf(label, sizeof label, "cycle %d: 500 runs of 1-32 rows", c);
    boundary(label);

    for (int i = 0; i < 200; i++, iter++) {
      int64_t r = (rng() % 10 == 0) ? big[rng() % 4] : small_rows();
      run(r, false);
      sample("mixed", r);
    }
    snprintf(label, sizeof label, "cycle %d: 200 mixed, 10%% 256-2048", c);
    boundary(label);

    for (int i = 0; i < 300; i++, iter++) {
      int64_t r = small_rows();
      run(r, false);
      if (i % 25 == 0) sample("small-after-mixed", r);
    }
    snprintf(label, sizeof label, "cycle %d: 300 runs of 1-32 rows", c);
    boundary(label);

    // TRIM, as §5 describes it for this profile.
    if (profile == "A") {
      allocator_purge();
      snprintf(label, sizeof label, "cycle %d: TRIM = allocator purge", c);
      boundary(label);
    } else {
      bool ok = run(1, true);
      snprintf(label, sizeof label, "cycle %d: TRIM 1/2 shrink run %s", c, ok ? "ok" : "FAILED");
      boundary(label);
      allocator_purge();
      snprintf(label, sizeof label, "cycle %d: TRIM 2/2 allocator purge", c);
      boundary(label);
    }
    for (int i = 0; i < 100; i++, iter++) {
      int64_t r = small_rows();
      run(r, false);
      if (i % 25 == 0) sample("small-after-trim", r);
    }
    snprintf(label, sizeof label, "cycle %d: 100 runs of 1-32 rows", c);
    boundary(label);
  }
  printf("  loop totals: refused at first try=%ld, ran after a shrink=%ld, still refused=%ld%s%s\n", fails, recovered, refused,
         fails ? "\n  first error text: " : "", first_error.c_str());

  // Does the runtime refuse a request that needs more than the cap? (4096 rows; hidden activations of the
  // MLP need 4096*(4096+2048)*4 = 96 MB.)
  first_error.clear();
  bool ok = run(OVER_ROWS, false);
  iter++;
  printf("  oversize request, %" PRId64 " rows: %s%s%s\n", OVER_ROWS, ok ? "ran" : "REFUSED", ok ? "" : " -> ", first_error.c_str());
  boundary("after oversize request");
  bool ok1 = run(1, false);
  iter++;
  printf("  next 1-row request after that: %s\n", ok1 ? "ran" : "FAILED");

  // The shrink key under profile A: the design says the two cannot be combined. Show what the runtime says.
  if (profile == "A") {
    first_error.clear();
    bool ok2 = run(1, true);
    printf("  profile A + run option \"%s\"=\"cpu:0\": %s%s%s\n", kOrtRunOptionsConfigEnableMemoryArenaShrinkage,
           ok2 ? "Run succeeded" : "Run REFUSED", ok2 ? "" : " -> ", first_error.c_str());
  } else {
    bool ok2 = run(1, true);
    allocator_purge();
    printf("  final shrink run: %s\n", ok2 ? "ok" : "FAILED");
    boundary("after final shrink + purge");
  }
  fclose(csv);
  printf("\n");
  return 0;
}

// ---------------------------------------------------------------------------------------------
// load: model from a mapped descriptor
// ---------------------------------------------------------------------------------------------
static void load_report(const char* label, const std::string& real_path) {
  SmapsSum m = smaps_for_path(real_path);
  printf("REPORT pid=%d %-12s model-file mapping: vmas=%d perms=%s Size=%ld Rss=%ld Pss=%ld Shared_Clean=%ld Shared_Dirty=%ld "
         "Private_Clean=%ld Private_Dirty=%ld Anonymous=%ld kB\n",
         getpid(), label, m.vmas, m.vmas ? m.perms.c_str() : "-", m.size, m.rss, m.pss, m.sh_clean, m.sh_dirty, m.pr_clean, m.pr_dirty,
         m.anon);
  printf("REPORT pid=%d %-12s whole process:      Rss=%ld Pss=%ld Pss_Anon=%ld Pss_File=%ld kB (smaps_rollup) | RssAnon=%ld RssFile=%ld "
         "kB (status)\n",
         getpid(), label, kv_kb("smaps_rollup", "Rss"), kv_kb("smaps_rollup", "Pss"), kv_kb("smaps_rollup", "Pss_Anon"),
         kv_kb("smaps_rollup", "Pss_File"), kv_kb("status", "RssAnon"), kv_kb("status", "RssFile"));
}

static Mapped g_own;  // the model mapping, when this program made it

static int cmd_load(const std::string& model, const std::string& variant, bool hold) {
  char rp[4096];
  if (!realpath(model.c_str(), rp)) DIE("realpath %s", model.c_str());
  std::string real_path = rp;
  if (!hold) printf("== load model=%s variant=%s %s\n", model.c_str(), variant.c_str(), prepack_note());
  Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "s5");
  Ort::SessionOptions so = base_options();
  apply_profile(env, so, "A", false);
  long anon0 = kv_kb("status", "RssAnon");

  std::unique_ptr<Ort::Session> sess;
  uint64_t t0 = now_ns();
  if (variant == "bytes+init") {  // what §7.1 names
    Mapped m = map_file_ro(model);
    g_own = m;
    so.AddConfigEntry(kOrtSessionOptionsConfigUseORTModelBytesDirectly, "1");
    so.AddConfigEntry(kOrtSessionOptionsConfigUseORTModelBytesForInitializers, "1");
    sess = std::make_unique<Ort::Session>(env, m.addr, m.len, so);
  } else if (variant == "bytes") {  // only the first of the two
    Mapped m = map_file_ro(model);
    g_own = m;
    so.AddConfigEntry(kOrtSessionOptionsConfigUseORTModelBytesDirectly, "1");
    sess = std::make_unique<Ort::Session>(env, m.addr, m.len, so);
  } else if (variant == "buffer") {  // mapped buffer, no options (also the only choice for plain .onnx)
    Mapped m = map_file_ro(model);
    g_own = m;
    sess = std::make_unique<Ort::Session>(env, m.addr, m.len, so);
  } else if (variant == "path") {  // by file name, no options: what most programs do
    sess = std::make_unique<Ort::Session>(env, model.c_str(), so);
  } else if (variant == "path-mmap") {  // by file name, the runtime maps the file itself (new key)
    so.AddConfigEntry(kOrtSessionOptionsConfigUseMemoryMappedOrtModel, "1");
    so.AddConfigEntry(kOrtSessionOptionsConfigUseORTModelBytesForInitializers, "1");
    sess = std::make_unique<Ort::Session>(env, model.c_str(), so);
  } else {
    DIE("unknown variant %s", variant.c_str());
  }
  double load_ms = double(now_ns() - t0) / 1e6;
  Mapped own = g_own;
  Ort::Session& s = *sess;
  if (!hold) {
    printf("session created in %.1f ms (VM timing); RssAnon before load=%ld kB\n", load_ms, anon0);
    load_report("after-load", real_path);
  }

  // Touch every weight: a few inferences at 1 row and one at 32.
  Ort::MemoryInfo mi = Ort::MemoryInfo::CreateCpu(OrtDeviceAllocator, OrtMemTypeDefault);
  std::vector<float> x(32 * N_FEAT), y(32);
  fill_random(x.data(), x.size(), 3);
  Ort::RunOptions ro;
  auto infer = [&](int n) {
    for (int i = 0; i < n; i++) {
      int64_t rows = i == n - 1 ? 32 : 1;
      int64_t in_shape[2] = {rows, N_FEAT}, out_shape[2] = {rows, 1};
      Ort::Value in = Ort::Value::CreateTensor<float>(mi, x.data(), size_t(rows) * N_FEAT, in_shape, 2);
      Ort::Value out = Ort::Value::CreateTensor<float>(mi, y.data(), size_t(rows), out_shape, 2);
      s.Run(ro, IN_NAMES, &in, 1, OUT_NAMES, &out, 1);
    }
  };
  infer(5);
  malloc_trim(0);  // do not let allocator slack from loading pass for weights
  if (!hold) {
    load_report("after-warmup", real_path);
    if (own.addr) {
      // Which copy of the weights does inference read? Drop this process's pages of the model mapping
      // (they stay in the page cache), infer again, and see whether the mapping is touched again.
      int rc = madvise(own.addr, own.len, MADV_DONTNEED);
      printf("madvise(model mapping, MADV_DONTNEED) -> %s\n", rc == 0 ? "ok" : strerror(errno));
      load_report("after-drop", real_path);
      infer(5);
      load_report("after-rerun", real_path);
    }
    printf("score[0]=%.7g\n\n", y[0]);
    return 0;
  }

  // Held open and driven over stdin by share.py, so two processes can be measured while both are alive.
  printf("READY\n");
  std::string line;
  while (std::getline(std::cin, line)) {
    if (line.rfind("report ", 0) == 0) {
      load_report(line.c_str() + 7, real_path);
      printf("DONE\n");
    } else if (line == "quit") {
      break;
    }
  }
  return 0;
}

// ---------------------------------------------------------------------------------------------
// rlimit: profile A is "not capped by ORT; relies on RLIMIT_DATA". What does the runtime do when it bites?
// ---------------------------------------------------------------------------------------------
static int cmd_rlimit(const std::string& model) {
  printf("== rlimit model=%s profile=A allocator=%s\n", model.c_str(), allocator_name());
  Ort::Env env(ORT_LOGGING_LEVEL_FATAL, "s5");
  Ort::SessionOptions so = base_options();
  apply_profile(env, so, "A", false);
  auto sess = load_as_designed(env, so, model);
  Ort::Session& s = *sess;
  const int64_t OVER_ROWS = 4096;
  Slot slot = make_slot(size_t(OVER_ROWS) * N_FEAT * 4, size_t(OVER_ROWS) * 4);
  fill_random(slot.in(), size_t(OVER_ROWS) * N_FEAT, 5);
  Ort::MemoryInfo mi = Ort::MemoryInfo::CreateCpu(OrtDeviceAllocator, OrtMemTypeDefault);
  Ort::IoBinding binding(s);
  auto run = [&](int64_t rows) -> std::string {
    int64_t in_shape[2] = {rows, N_FEAT}, out_shape[2] = {rows, 1};
    Ort::Value in = Ort::Value::CreateTensor<float>(mi, slot.in(), size_t(rows) * N_FEAT, in_shape, 2);
    Ort::Value out = Ort::Value::CreateTensor<float>(mi, slot.out(), size_t(rows), out_shape, 2);
    binding.BindInput("features", in);
    binding.BindOutput("score", out);
    try {
      s.Run(Ort::RunOptions{}, binding);
      return "ran";
    } catch (const Ort::Exception& e) {
      return std::string("REFUSED with OrtErrorCode ") + std::to_string(int(e.GetOrtErrorCode())) + ": " + e.what();
    } catch (const std::bad_alloc& e) {
      return std::string("std::bad_alloc escaped the API: ") + e.what();
    }
  };
  printf("  warm-up 1 row: %s; 32 rows: %s\n", run(1).c_str(), run(32).c_str());
  long vmdata_kb = kv_kb("status", "VmData");
  size_t headroom = size_t(32) << 20;
  rlimit rl = {rlim_t(vmdata_kb) * 1024 + headroom, rlim_t(vmdata_kb) * 1024 + headroom};
  int rc = setrlimit(RLIMIT_DATA, &rl);
  printf("  VmData now %ld kB; setrlimit(RLIMIT_DATA, VmData + 32 MB) -> %s\n", vmdata_kb, rc == 0 ? "ok" : strerror(errno));
  printf("  4096 rows (needs about 96 MB of activations for the MLP): %s\n", run(OVER_ROWS).c_str());
  printf("  1 row afterwards: %s\n", run(1).c_str());
  printf("  32 rows afterwards: %s\n", run(32).c_str());
  printf("  process still alive, RssAnon=%ld kB\n\n", kv_kb("status", "RssAnon"));
  return 0;
}

int main(int argc, char** argv) {
  setvbuf(stdout, nullptr, _IOLBF, 0);
  g_page = size_t(sysconf(_SC_PAGESIZE));
  std::string cmd = argc > 1 ? argv[1] : "";
  try {
    if (cmd == "options") return cmd_options();
    if (cmd == "zerocopy" && argc == 5) return cmd_zerocopy(argv[2], atoll(argv[3]), argv[4]);
    if (cmd == "latency" && argc == 7) return cmd_latency(argv[2], argv[3], size_t(atoll(argv[4])), atoll(argv[5]), argv[6]);
    if (cmd == "mem" && argc == 5) return cmd_mem(argv[2], argv[3], argv[4]);
    if (cmd == "load" && argc >= 4) return cmd_load(argv[2], argv[3], argc > 4 && std::string(argv[4]) == "--hold");
    if (cmd == "rlimit" && argc == 3) return cmd_rlimit(argv[2]);
  } catch (const Ort::Exception& e) {
    printf("UNCAUGHT Ort::Exception code=%d: %s\n", int(e.GetOrtErrorCode()), e.what());
    return 3;
  }
  fprintf(stderr, "usage: see the comment at the top of s5.cc\n");
  return 64;
}
