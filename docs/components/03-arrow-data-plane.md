# 03 — Arrow Data Plane

[← High-level design](../high-level-design.md) · depends on [01 — SHM ABI](01-shm-abi.md)

This document specifies the byte layout inside each slot's request and response regions and how all five SDKs and workers read and write valid Apache Arrow IPC frames without parsing flatbuffers or allocating on the hot path.

---

## 1. Motivation: zero-parse, zero-allocation Arrow IPC

Apache Arrow IPC is the ideal wire format for shared-memory inference: it is a language-neutral standard with readers in Rust, C++, Java, Go, and Python; buffer locations are offsets relative to the message body (making frames relocatable across address spaces); and columns are contiguous and SIMD-aligned.

Used naively, however, stock Arrow IPC readers and writers violate microsecond latency and zero-allocation goals:

- Every message carries flatbuffer metadata that a generic reader must parse on every call.
- Stock readers allocate `RecordBatch`, array, and buffer wrapper objects per message even when column buffers are zero-copy.
- Stock writers serialize flatbuffer metadata and compute alignment padding on every call.

For a 1 KB feature vector on a sub-microsecond transport budget, per-call flatbuffer serialization and parsing would dominate total latency.

---

## 2. Design: pre-stamped frame templates

**Key observation:** for a fixed schema of fixed-width columns, an Arrow IPC `RecordBatch` message consists of identical metadata bytes every time except for the column values and a small set of 8-byte `int64` length fields at fixed byte offsets in the flatbuffer prefix: the batch row count, each field node's length, and each buffer's byte length. For a `FixedSizeList<float32>[N]` column (two field nodes: the list and its child primitive array), there are four 8-byte fields in total.

SidecarZero exploits this invariance:

1. **Template rendering at model load:** When a model is loaded, the worker renders its input and output schemas and one **capacity template** per direction and slot class — a complete Arrow IPC encapsulated message laid out for the maximum number of rows (`max_rows`) that slot class can hold. The supervisor verifies the flatbuffers and offset descriptors before publishing them in the read-only control segment's registry block.
2. **One-time slot stamping:** Each slot region is **stamped** with its template's metadata prefix the first time it is used, and the `(registry_epoch, template_id)` stamp is recorded in the slot record. The prefix remains in place across subsequent requests reusing that slot.
3. **Hot-path offset writes:** On each request, the SDK writes column values directly at the fixed byte offsets learned when it bound to the registry. If the request's row count differs from the row count the slot last held, the SDK rewrites the 8-byte length fields listed in `PatchDesc[]`. No flatbuffer library, no Arrow dependency, and zero heap allocations run on the hot path.
4. **Full Arrow IPC compatibility:** Every slot's request and response region is at all times a complete, spec-conformant Arrow IPC encapsulated message that stock Arrow readers (`arrow-rs`, Arrow C++, Arrow Java, Arrow Go, `pyarrow`) can read directly without modification.

---

## 3. Slot data layout

```
slot data (slot_size bytes, 64-byte aligned)
├── request region (req_cap bytes, a multiple of 64)
│     0x00  continuation marker   0xFFFFFFFF
│     0x04  metadata_size         int32
│     0x08  flatbuffer Message { RecordBatch { length, nodes[], buffers[] } }
│           padding to a 64-byte boundary
│     body_off:
│           column buffers, each starting on a 64-byte boundary,
│           each sized for the template's max_rows
└── response region (resp_cap bytes)
      same structure
```

- **Buffers are placed at `max_rows` capacity**, so buffer start offsets never shift when a request carries fewer rows than `max_rows`. A request with fewer rows writes to the front of each buffer and records the shorter byte lengths in the metadata patches.
- **The frame's total length (`frame_cap`) is constant** for a given template. The message body length is declared at full capacity; any unused tail at the end of a column buffer is simply outside the active buffer length.
- Arrow IPC requires 8-byte alignment of the message body and each buffer. Using 64-byte alignment satisfies the Arrow specification while placing every column buffer on a cache-line and AVX-512 boundary.

---

## 4. Registry block

Located by `registry_off` and `registry_len` in the control header and protected by the model seqlock (`model_seq`).

Because the registry lives in the write-sealed control segment, a loading worker writes its candidate registry into `staging[beat_slot]` in the data segment ([shm-abi §3](01-shm-abi.md#3-zone-1--control-header)). Before copying it into the control segment, the supervisor:

1. Runs a flatbuffer verifier over every template prefix and both `Schema` messages so no malformed flatbuffer is ever published.
2. Checks every offset, length, and patch position against the prefix length and the region capacities (`req_cap` / `resp_cap`) of the template's slot class.
3. Recomputes `schema_fingerprint` from the `ColumnDesc` array ([§9](#9-schema-binding-and-evolution)) and publishes that value. Upon activation, the worker reads the published registry back from the control segment and refuses to serve unless the bytes match what it staged.
4. Compares the candidate against the currently published registry and increments `registry_epoch` if and only if the contents differ.

### Structures

Every structure is fixed-size, little-endian, and naturally aligned, with strides recorded in `RegistryHeader` (normative offsets in [`abi/scz_abi.toml`](../../abi/scz_abi.toml)):

```
RegistryHeader (64 bytes)
    magic                 u64      ASCII "SCZREG\0\0"
    registry_len          u32
    template_count        u16
    template_stride       u16      48
    column_stride         u16      32
    patch_stride          u16      12
    in_schema_off / len   u32 ×2   Arrow IPC Schema message for requests
    out_schema_off / len  u32 ×2   Arrow IPC Schema message for responses
    templates_off         u32      TemplateDesc[template_count]
    columns_off           u32      ColumnDesc[column_total]
    patches_off           u32      PatchDesc[patch_total]
    names_off / len       u32 ×2   column names, UTF-8, concatenated
    column_total          u32      total ColumnDesc entries across all templates
    patch_total           u32      total PatchDesc entries across all templates

TemplateDesc (48 bytes)
    template_id           u16      never 0
    direction             u8       REQUEST (1) or RESPONSE (2)
    flags                 u8
    slab_class            u16      the slot class this template is laid out for
    column_count          u16
    max_rows              u32      maximum rows the frame can carry
    prefix_off / len      u32 ×2   the pre-rendered metadata bytes, within the registry
    body_off              u32      offset of the body from the start of the region
    frame_cap             u32      total bytes of the frame; constant for the template
    first_column          u32      start index into ColumnDesc[]
    first_patch           u32      start index into PatchDesc[]
    patch_count           u16
    rows_field_off        u32      offset where the i64 row count sits in the prefix

ColumnDesc (32 bytes)
    values_off            u32      offset of the values buffer from the start of the region
    row_bytes             u32      bytes of values per row
    validity_off          u32      0 (non-nullable)
    null_count_off        u32      0 (non-nullable)
    list_size             u32      1 for scalars; N for FixedSizeList<T>[N]
    name_off              u32      into the names area
    name_len              u16
    elem_type             u8       physical type code (i8…i64, u8…u64, f16, f32, f64, fixed binary)
    nullable              u8       0 in ABI 1.0

PatchDesc (12 bytes)
    prefix_off            u32      position of an 8-byte i64 field in the prefix
    scale                 u32
    kind                  u8       ROWS_SCALED: value = rows × scale
                                   BITMAP_BYTES: value = ceil(rows / 8)
```

Each template carries its own `ColumnDesc` slice (`first_column .. first_column + column_count`) because buffer offsets depend on the template's `max_rows` capacity. The patch slice (`first_patch .. first_patch + patch_count`) includes the batch row count at `rows_field_off` as well as all field-node lengths and buffer byte lengths. A column of `list_size == 1` is encoded as a scalar primitive column; `FixedSizeList` is used whenever `list_size > 1`. Template selection is governed exclusively by the registry (`TemplateDesc.slab_class` and `TemplateDesc.direction`) so templates can change across model reloads while `slab_dir` remains sealed in the control header.

### Templates and slot classes

There is one request template and one response template per slot class. A request for `n` rows uses the smallest slot class whose request template satisfies `max_rows ≥ n`. A template's `max_rows` is the maximum rows the slot class can hold, capped at the model's fixed batch dimension if the model has one ([worker §4](05-worker.md#rows-and-the-models-batch-dimension)).

| Slot class | Slot size | Capacity for 256 `float32` features |
|---|---|---|
| Small (`0`) | 4 KB | 2 rows |
| Batch (`1`) | 128 KB | ~90 rows |

If a caller requests more rows than the largest configured template supports, the SDK falls back immediately with `FallbackReason::TooLarge`.

### Stamps

Each slot record stores two 32-bit stamps: `req_stamp` (written by the client) and `resp_stamp` (written by the worker):

```
(registry_epoch & 0xFFFF) << 16  |  template_id
```

Zero means unstamped. Before writing into a region, the writer compares the slot's stamp against the expected value. Only on a mismatch (first use of the slot or after a schema-changing reload) does it copy the template prefix (`prefix_len` bytes) into the slot and update the stamp.

---

## 5. Canonical tensor encoding

Feature vectors are encoded as a single non-nullable column of **`FixedSizeList<float32>[N]`** rather than `N` separate scalar columns:

| Property | `N` scalar columns | One `FixedSizeList<float32>[N]` column |
|---|---|---|
| Memory layout of one row's features | Scattered across `N` separate column buffers | Contiguous `[rows, N]` array |
| Mapping to a runtime input tensor `[rows, N]` | Requires a gather copy | Zero-copy: the values buffer **is** the tensor |
| Flatbuffer metadata size | Grows as O(N) | Constant O(1) |

For a 256-feature model, the request values buffer is `rows × 256 × 4` contiguous bytes, which the worker passes directly to ONNX Runtime as a `[rows, 256]` `float32` tensor via `OrtCreateTensorWithDataAsOrtValue` without copying. Multi-input models declare multiple columns, each mapped by name to the corresponding model input tensor.

**Tree-classifier export rule.** Tree-classifier converters (`onnxmltools`, `skl2onnx`) emit a `ZipMap` sequence of maps by default for class probabilities. Because maps are not fixed-width tensors, models must be exported with `zipmap=False` so probabilities are emitted as a `[rows, K]` `float32` tensor (`FixedSizeList<float32>[K]`) or a `[rows]` scalar score.

---

## 6. Supported data types

| Category | Types | Encoding |
|---|---|---|
| **Numeric tensors & scalars** | `int8`, `int16`, `int32`, `int64`, `uint8`, `uint16`, `uint32`, `uint64`, `float16`, `float32`, `float64`, `FixedSizeBinary`, and `FixedSizeList<T>[N]` of any of these (non-nullable) | Pre-stamped capacity template; zero-parse offset read/write |
| **Missing values** | `NaN` (floating-point) or sentinel values (e.g. `-1` token padding for GGUF) | Carried in-band without nullable validity bitmaps |

---

## 7. Read and write paths

### 7.1 Client write

```
class = smallest slot class whose request template has max_rows >= rows   // none: fall back, TOO_LARGE
tmpl  = binding.request_template[class]              // resolved when the client bound
base  = start of the slot's request region

want = (binding.registry_epoch16 << 16) | tmpl.id
if slot.req_stamp != want:                            // first use, or the registry changed
    memcpy(base, tmpl.prefix, tmpl.prefix_len); slot.req_stamp = want

if load_i64(base + tmpl.rows_field_off) != rows:      // the frame remembers the row count it last held
    for p in tmpl.patches: store_i64(base + p.prefix_off, value(p, rows))

memcpy(base + col.values_off, features, rows * col.row_bytes)
slot.req_len = tmpl.frame_cap
```

A caller that consistently sends the same number of rows (e.g. `rows = 1`) never rewrites a single length field after the first call on a slot — the entire encoding step reduces to one stamp compare, one row-count compare, and a `memcpy` of the feature bytes. SDKs also provide in-place writer views so callers can compute features directly inside the slot's request buffer.

### 7.2 Worker read

After claiming the slot (`READY → PROCESSING`), using local copies of slot-record fields:

```
stamp = slot.req_stamp
if epoch16(stamp) != my registry epoch:       complete(STALE_REGISTRY)
tmpl = request template for this slot's class
if id(stamp) != tmpl.id:                      complete(BAD_REQUEST)
if slot.req_len != tmpl.frame_cap:            complete(BAD_REQUEST)
rows = load_i64(base + tmpl.rows_field_off)
if rows < 1 or rows > tmpl.max_rows:          complete(BAD_REQUEST)
wrap [values_off, values_off + rows × row_bytes) as the model's input tensor
```

The worker never parses the flatbuffer during inference. Every error path sets `slot.status` and transitions the slot to `DONE`.

### 7.3 Worker write and client read

Symmetric with the request path. The worker checks `resp_stamp`, copies the response prefix if needed, updates length patches if `rows` changed, binds the model's output tensor directly to `[base + col.values_off, ...)` so the inference engine writes its output straight into shared memory, and records `resp_len`, `status = OK`, and `model_gen` before transitioning `PROCESSING → DONE`.

Before decoding, the client verifies that `resp_stamp` matches its expected `(registry_epoch16 << 16) | resp_tmpl.id`, that `resp_len == resp_tmpl.frame_cap`, and that the response row count equals the request row count. Any violation increments `client_rejects` and triggers the local fallback with `FallbackReason::WorkerFailed`.

---

## 8. Template rendering and cross-language Arrow reader interoperability

Templates are rendered in [`crates/scz-worker-core/src/render.rs`](../../crates/scz-worker-core/src/render.rs) and validated in [`crates/scz-shm/src/registry.rs`](../../crates/scz-shm/src/registry.rs) using `arrow-ipc` configured with 64-byte buffer alignment:

1. **Non-zero sentinel rendering:** Flatbuffers omit fields whose value equals the type default (`0`). To guarantee that every length field (`RecordBatch.length`, `FieldNode.length`, `Buffer.length`) is materialized in the flatbuffer vtable, `scz-worker-core` renders the template at `max_rows` (which is always ≥ 1) and locates the exact byte offset of each field by walking the flatbuffer tables (`Message → RecordBatch → nodes[] / buffers[]`), never by diffing two renders.
2. **Self-verification:** Immediately after rendering a template, `scz-worker-core` parses the rendered frame back through `arrow_ipc::reader::StreamReader` and verifies that every column's buffer address and length match `ColumnDesc` and `PatchDesc`.
3. **Cross-language reader validation:** Empirical validation in [`spikes/s4-arrow-templates/`](../../spikes/s4-arrow-templates/FINDING.md) verified that frames stamped once at `max_rows = 64` and patched in-place to `rows ∈ {1, 7, 64, 1}` are accepted and decoded with bit-identical values by all five stock Arrow implementations (`arrow-rs`, Arrow C++, Arrow Java, Arrow Go, and `pyarrow`).

| Language | Zero-copy Arrow library wrapper | Stock reader entry point |
|---|---|---|
| Rust | `Buffer::from_custom_allocation` over the region | `arrow_ipc::reader` |
| C++ | `arrow::Buffer` over the region | `arrow::ipc::ReadRecordBatch` |
| Java | `ArrowBuf` over `MemorySegment` | `MessageSerializer.deserializeRecordBatch` + `VectorLoader` |
| Go | `memory.NewBufferBytes` | `ipc.NewMessage` + `ipc.NewReaderFromMessageReader` |
| Python | `pa.py_buffer(memoryview)` | `pa.ipc.read_record_batch` |

Stock Arrow libraries are never required or linked by any SDK on the inference path; they are used only in cross-language interoperability tests and optional generic tooling.

---

## 9. Lifetime and safety invariants

- **Borrowed views are scoped to slot ownership.** A zero-copy response view is valid only while the caller holds the `DONE` slot, enforced by lifetime or scope guards in each language (`ShmCell` slice tied to a guard lifetime in Rust, RAII move-only `Response` guard in C++20, `AutoCloseable` in Java, explicit `Release()` / copy-out default in Go, context manager in Python).
- **No double-fetch (`TOCTOU`).** Every field in the data segment (`req_len`, `resp_len`, `req_stamp`, `resp_stamp`, `rows`) is read once using an atomic or volatile load into a local variable and bounds-checked before use.
- **Integer index columns are copied by the worker before inference.** Because the data segment remains mapped writable by the host while a request is `PROCESSING`, a buggy or hostile thread in the host could mutate request bytes mid-inference. Floating-point feature mutations can only affect the numerical result of that host's own request, whereas out-of-bounds integer vocabulary/embedding indices (`int32`/`int64`) can cause ONNX Runtime's `Gather` operator or `llama.cpp` token lookup to read out of bounds and segfault the worker. Workers therefore copy integer columns into a private buffer and clamp or validate indices before passing them to the runtime ([worker §7.1](05-worker.md#71-onnx-runtime)).

---

## 10. Schema binding and evolution

### Schema fingerprint

`schema_fingerprint` is `xxHash3-64` (seed `0`) over the canonical little-endian byte stream:

```
"SCZSCHEMA" 0x01
for direction in (REQUEST, RESPONSE):
    column_count u16
    for each column, in order:
        name_len u16, name bytes (UTF-8, exactly as declared)
        elem_type u8, list_size u32, nullable u8
```

Hashing the logical column descriptions rather than the Arrow `Schema` flatbuffer bytes ensures that the worker, the supervisor, and all five SDKs (which do not link Arrow flatbuffer builders) compute the exact same 64-bit fingerprint, pinned across all languages by [`abi/golden/`](../../abi/golden/).

### Strict and lenient binding

- **Strict (default):** The SDK codec's computed `schema_fingerprint` must equal the control header's `schema_fingerprint`. On mismatch, the client enters `SCHEMA_REJECTED` and every call falls back with `FallbackReason::SchemaMismatch`.
- **Lenient (opt-in):** The SDK binds columns by name. All request columns required by the model must be supplied with matching types and list sizes; extra response columns produced by the model that the caller did not request are ignored.

### Live schema evolution across model reloads

When a model reload replaces the schema under a running client, three mechanisms guarantee that a request encoded for schema A is never evaluated by a model expecting schema B:

1. **Seqlock ordering:** The supervisor increments `registry_epoch` and updates `schema_fingerprint` under `model_seq` *before* updating `gate.worker_gen` to `HEALTHY`.
2. **Client admission check:** On every call, the SDK compares `gate.worker_gen` (already loaded as part of the gate check) against the generation its binding was last confirmed under. When `worker_gen` changes, the SDK reads `registry_epoch`:
   - If `registry_epoch` is unchanged (warm standby promotion or same-schema model reload), the binding is immediately confirmed for the new generation and the call proceeds without fallback.
   - If `registry_epoch` changed, the SDK transitions to `REBINDING`; calls fall back with `FallbackReason::Rebinding` while the background thread re-reads the registry and transitions to `BOUND` or `SCHEMA_REJECTED`.
3. **Worker stamp validation:** Any request that passed client admission just before the reload carries the old `registry_epoch` in `slot.req_stamp`. The new worker sees `epoch16(req_stamp) != registry_epoch`, completes the slot with `status = STALE_REGISTRY`, and the SDK falls back and triggers rebinding.
