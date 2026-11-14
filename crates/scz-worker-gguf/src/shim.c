// A narrow C interface over llama.cpp's C API (release b11988), for scz-worker-gguf
// (worker §7.2). Compiled against the headers the build image installs in LLAMA_DIR.
//
// It keeps llama.cpp's structs on this side: Rust sees opaque handles, plain integers, and
// flat arrays. One model is shared by every consume thread; each consume thread has one
// context of its own, with its own batch, its own thread pool, and its own abort deadline.
//
// Nothing here can reach one of llama.cpp's assertions from a request: the caller has
// range-checked every token id against the vocabulary and bounded the batch, and both are
// checked again here before the batch is built.

#define _POSIX_C_SOURCE 200809L  // clock_gettime under -std=c11

#include <ggml-cpu.h>
#include <llama.h>

#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

// ── logging ─────────────────────────────────────────────────────────────────────────────

static int scz_log_min = GGML_LOG_LEVEL_WARN;
static _Thread_local int scz_log_last = GGML_LOG_LEVEL_NONE;
// Set on a thread from the moment its abort callback stops a call until the call returns:
// llama.cpp reports the stopped graph as three errors, which an abandoned request is not.
static _Thread_local int scz_log_quiet = 0;

// llama.cpp and ggml log through one callback; only messages at or above the configured
// level reach stderr, prefixed like the worker's own lines. A CONT message continues the
// previous one (the progress dots at load) and takes its level.
static void scz_log(enum ggml_log_level level, const char* text, void* ud) {
  (void)ud;
  int l = level == GGML_LOG_LEVEL_CONT ? scz_log_last : (int)level;
  if (level != GGML_LOG_LEVEL_CONT) scz_log_last = l;
  if (scz_log_quiet) return;
  if (l < scz_log_min || scz_log_min == GGML_LOG_LEVEL_NONE) return;
  fprintf(stderr, "sidecarzero-worker-gguf[%d]: llama.cpp: %s", (int)getpid(), text);
}

static void put_err(char* err, size_t errlen, const char* m) {
  if (errlen) {
    snprintf(err, errlen, "%s", m);
  }
}

static uint64_t scz_now_ns(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}

// Once, before anything else. `log_min` is a ggml_log_level; GGML_LOG_LEVEL_NONE silences it.
void scz_gguf_init(int32_t log_min) {
  scz_log_min = log_min;
  llama_log_set(scz_log, NULL);
  llama_backend_init();
}

// ── the model ───────────────────────────────────────────────────────────────────────────

struct scz_gguf_info {
  int32_t n_vocab;
  int32_t n_embd;       // the size of one pooled embedding
  int32_t n_ctx_train;  // the longest sequence the model was trained on
  int32_t pooling;      // enum llama_pooling_type, as the model declares it
  int32_t memoryless;   // 1: the model keeps no memory between calls (an encoder such as BERT)
  char desc[128];
};

// Load from `path` with the file memory-mapped: on the CPU, llama.cpp computes from the mapped
// weights unless it repacks them into a layout of its own (`use_extra_bufts`, `repack`).
struct llama_model* scz_gguf_load(const char* path, int32_t repack, char* err, size_t errlen) {
  struct llama_model_params p = llama_model_default_params();
  p.load_mode = LLAMA_LOAD_MODE_MMAP;
  p.n_gpu_layers = 0;
  p.use_extra_bufts = repack != 0;
  struct llama_model* m = llama_model_load_from_file(path, p);
  if (!m) put_err(err, errlen, "llama.cpp could not load the model (its log above says why)");
  return m;
}

void scz_gguf_model_free(struct llama_model* m) { llama_model_free(m); }

// What a context would be: its pooling and whether it has memory are properties of a
// context, so one is made with the model's defaults and thrown away.
int32_t scz_gguf_info(const struct llama_model* m, struct scz_gguf_info* info, char* err, size_t errlen) {
  memset(info, 0, sizeof *info);
  const struct llama_vocab* v = llama_model_get_vocab(m);
  info->n_vocab = llama_vocab_n_tokens(v);
  info->n_embd = llama_model_n_embd_out(m) > 0 ? llama_model_n_embd_out(m) : llama_model_n_embd(m);
  info->n_ctx_train = llama_model_n_ctx_train(m);
  llama_model_desc(m, info->desc, sizeof info->desc);
  struct llama_context_params cp = llama_context_default_params();
  cp.embeddings = true;
  cp.n_threads = cp.n_threads_batch = 1;
  cp.n_ctx = 256;
  cp.n_batch = cp.n_ubatch = 256;
  cp.n_seq_max = 1;
  cp.no_perf = true;
  struct llama_context* c = llama_init_from_model((struct llama_model*)m, cp);
  if (!c) {
    put_err(err, errlen, "llama.cpp could not make a context for the model");
    return -1;
  }
  info->pooling = llama_pooling_type(c);
  info->memoryless = llama_get_memory(c) == NULL;
  llama_free(c);
  return 0;
}

int32_t scz_gguf_tokenize(const struct llama_model* m, const char* text, int32_t len, int32_t* out, int32_t max) {
  return llama_tokenize(llama_model_get_vocab(m), text, len, out, max, true, false);
}

// ── a context: one per consume thread ───────────────────────────────────────────────────

struct scz_gguf_ctx {
  struct llama_context* ctx;
  struct ggml_threadpool* tp;
  struct llama_batch batch;
  int32_t n_tokens;  // the most tokens one call carries, over all its sequences
  int32_t n_seqs;    // the most sequences one call carries
  int32_t n_embd;
  int32_t n_vocab;
  _Atomic uint64_t deadline_ns;  // CLOCK_MONOTONIC; the abort callback compares with it
  _Atomic int32_t aborted;       // set when the callback has asked llama.cpp to stop
};

// llama.cpp's abort callback (context params `abort_callback`): ggml's CPU backend calls it
// between the nodes of the graph, on the calling thread, and stops the graph when it returns
// true; llama_encode and llama_decode then return 2.
static bool scz_abort(void* data) {
  struct scz_gguf_ctx* c = data;
  if (scz_now_ns() < atomic_load_explicit(&c->deadline_ns, memory_order_relaxed)) return false;
  atomic_store_explicit(&c->aborted, 1, memory_order_relaxed);
  scz_log_quiet = 1;  // the callback runs on the thread that called llama_encode
  return true;
}

static uint32_t round_up(uint32_t x, uint32_t to) { return (x + to - 1) / to * to; }

// `n_tokens` bounds one call (llama.cpp's n_batch and n_ubatch: an encoder must see every
// token of a call in one ubatch). More than one thread gets a persistent ggml thread pool that
// sleeps between calls instead of polling.
struct scz_gguf_ctx* scz_gguf_ctx_new(struct llama_model* m, int32_t n_tokens, int32_t n_seqs, int32_t n_threads, char* err, size_t errlen) {
  struct scz_gguf_ctx* c = calloc(1, sizeof *c);
  if (!c) {
    put_err(err, errlen, "out of memory");
    return NULL;
  }
  c->n_tokens = n_tokens;
  c->n_seqs = n_seqs;
  c->n_embd = llama_model_n_embd_out(m) > 0 ? llama_model_n_embd_out(m) : llama_model_n_embd(m);
  c->n_vocab = llama_vocab_n_tokens(llama_model_get_vocab(m));
  atomic_store(&c->deadline_ns, UINT64_MAX);
  struct llama_context_params cp = llama_context_default_params();
  cp.embeddings = true;
  // Each sequence's share of the context is at least 256 cells; llama.cpp rounds otherwise.
  cp.n_ctx = (uint32_t)n_seqs * round_up((uint32_t)n_tokens, 256);
  cp.n_batch = cp.n_ubatch = (uint32_t)n_tokens;
  cp.n_seq_max = (uint32_t)n_seqs;
  cp.n_threads = cp.n_threads_batch = n_threads;
  cp.no_perf = true;
  cp.abort_callback = scz_abort;
  cp.abort_callback_data = c;
  c->ctx = llama_init_from_model(m, cp);
  if (!c->ctx) {
    put_err(err, errlen, "llama.cpp could not make a context (its log above says why)");
    free(c);
    return NULL;
  }
  if (n_threads > 1) {
    struct ggml_threadpool_params tpp = ggml_threadpool_params_default(n_threads);
    tpp.poll = 0;  // idle workers sleep: an idle pool burns no CPU (worker §5)
    c->tp = ggml_threadpool_new(&tpp);
    if (!c->tp) {
      put_err(err, errlen, "ggml could not start its thread pool");
      llama_free(c->ctx);
      free(c);
      return NULL;
    }
    llama_attach_threadpool(c->ctx, c->tp, c->tp);
  }
  c->batch = llama_batch_init(n_tokens, 0, 1);
  return c;
}

void scz_gguf_ctx_free(struct scz_gguf_ctx* c) {
  if (!c) return;
  llama_batch_free(c->batch);
  llama_free(c->ctx);
  if (c->tp) ggml_threadpool_free(c->tp);
  free(c);
}

// The result codes of scz_gguf_embed.
enum { SCZ_OK = 0, SCZ_ABORTED = 1, SCZ_EXHAUSTED = 2, SCZ_INVALID = 3, SCZ_FAILED = 4 };

// Embed `n_seqs` sequences in one call: `tokens` holds their ids back to back, `lens` their
// lengths. Writes each sequence's pooled embedding, as llama.cpp returns it (not normalized),
// to `out`, `n_embd` floats per sequence. llama.cpp is asked to stop at `deadline_ns`.
int32_t scz_gguf_embed(struct scz_gguf_ctx* c, const int32_t* tokens, const int32_t* lens, int32_t n_seqs, uint64_t deadline_ns, float* out) {
  if (n_seqs < 1 || n_seqs > c->n_seqs) return SCZ_INVALID;
  int32_t k = 0;
  for (int32_t s = 0; s < n_seqs; s++) {
    if (lens[s] < 1 || lens[s] > c->n_tokens - k) return SCZ_INVALID;
    for (int32_t i = 0; i < lens[s]; i++, k++) {
      if (tokens[k] < 0 || tokens[k] >= c->n_vocab) return SCZ_INVALID;
      c->batch.token[k] = tokens[k];
      c->batch.pos[k] = i;
      c->batch.n_seq_id[k] = 1;
      c->batch.seq_id[k][0] = s;
      c->batch.logits[k] = 1;
    }
  }
  c->batch.n_tokens = k;
  atomic_store_explicit(&c->aborted, 0, memory_order_relaxed);
  atomic_store_explicit(&c->deadline_ns, deadline_ns, memory_order_relaxed);
  // An encoder (no memory) is run with llama_encode; a decoder used for embeddings starts
  // from an empty memory every call.
  llama_memory_t mem = llama_get_memory(c->ctx);
  int32_t r;
  if (mem) {
    llama_memory_clear(mem, true);
    r = llama_decode(c->ctx, c->batch);
  } else {
    r = llama_encode(c->ctx, c->batch);
  }
  atomic_store_explicit(&c->deadline_ns, UINT64_MAX, memory_order_relaxed);
  scz_log_quiet = 0;
  if (r != 0) {
    if (atomic_load_explicit(&c->aborted, memory_order_relaxed)) return SCZ_ABORTED;
    return r == -2 || r == 1 ? SCZ_EXHAUSTED : SCZ_FAILED;
  }
  for (int32_t s = 0; s < n_seqs; s++) {
    const float* e = llama_get_embeddings_seq(c->ctx, s);
    if (!e) return SCZ_FAILED;
    memcpy(out + (size_t)s * (size_t)c->n_embd, e, (size_t)c->n_embd * sizeof(float));
  }
  return SCZ_OK;
}

// ── a reference, for tests ──────────────────────────────────────────────────────────────

// The plainest use of llama.cpp: a fresh context with its default parameters, one sequence
// through llama_batch_get_one and llama_decode, the pooled embedding of sequence 0. No
// thread pool, no abort callback, no packing: none of what the worker's path adds.
int32_t scz_gguf_reference(struct llama_model* m, const int32_t* tokens, int32_t n, float* out, char* err, size_t errlen) {
  struct llama_context_params cp = llama_context_default_params();
  cp.embeddings = true;
  cp.no_perf = true;
  struct llama_context* c = llama_init_from_model(m, cp);
  if (!c) {
    put_err(err, errlen, "no context");
    return -1;
  }
  int32_t r = llama_decode(c, llama_batch_get_one((llama_token*)tokens, n));
  const float* e = r == 0 ? llama_get_embeddings_seq(c, 0) : NULL;
  if (e) {
    int32_t d = llama_model_n_embd_out(m) > 0 ? llama_model_n_embd_out(m) : llama_model_n_embd(m);
    memcpy(out, e, (size_t)d * sizeof(float));
  } else {
    snprintf(err, errlen, "llama_decode returned %d", r);
  }
  llama_free(c);
  return e ? 0 : -1;
}

// ── quantizing, for tests ───────────────────────────────────────────────────────────────

// Write `in` quantized to `ftype` (enum llama_ftype) at `out`. 0 on success.
int32_t scz_gguf_quantize(const char* in, const char* out, int32_t ftype, int32_t nthread) {
  llama_model_quantize_params p = llama_model_quantize_default_params();
  p.ftype = (enum llama_ftype)ftype;
  p.nthread = nthread;
  return (int32_t)llama_model_quantize(in, out, &p);
}
