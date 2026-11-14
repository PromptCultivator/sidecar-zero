# S4 — Cross-Language Arrow IPC Pre-Stamped Template Interoperability

[← Findings Summary](../FINDINGS.md)

## Objective

Verify that Arrow C++ / PyArrow, Rust (`arrow-rs`), Go (`arrow-go`), and Java (`arrow-java`) stock Arrow IPC readers all accept a pre-stamped Arrow IPC `RecordBatch` frame whose buffers are spaced at `max_rows` capacity on 64-byte boundaries, whose `bodyLength` is declared at full capacity, and whose 8-byte metadata length fields are patched in place for varying row counts ([03 — Arrow Data Plane](../../docs/components/03-arrow-data-plane.md)).

---

## Experimental Setup

Executed via `./run.sh` across four language containers:

| Reader Implementation | Version | Toolchain |
|---|---|---|
| PyArrow / Arrow C++ | `pyarrow 26.0.1` / `Arrow C++ 26.0.1` | Python `3.12.16` |
| Rust (`arrow-rs`) | `arrow-ipc` / `arrow-array` `60.1.0` | `rustc 1.99.1` |
| Go (`arrow-go`) | `github.com/apache/arrow-go/v18 v18.8.0` | Go `1.27.4` |
| Java (`arrow-java`) | `arrow-vector` / `arrow-memory-unsafe` `19.0.0` | Eclipse Temurin `21.0.12` |

Three schemas (`launch`: `FixedSizeList<float32>[256]` at `max_rows = 32`; `prims`: 4 non-nullable primitives at `max_rows = 100`; `nullable`: 4 nullable primitives at `max_rows = 100`) were rendered both by PyArrow and by `arrow-rs` (`alignment = 64`), stamped once into a buffer pre-filled with `0xA5` poison bytes, and patched in place across row-count sequences `max → 1 → middle → max`.

---

## Measured Results

### 1. Cross-Language Reader Acceptance Matrix (`results/matrix.txt`)

Every cell below tested 3 row counts (`1`, `17`/`37`, `32`/`100`) and verified exact row count, null counts, per-column FNV-1a value checksums, zero-copy buffer borrowing, and full array validation (`validate(full=True)` / `ArrayData::validate_full` / `ValueVectorUtility.validateFull`):

| Reader | IPC API | `launch` (1 / 17 / 32 rows) | `prims` (1 / 37 / 100 rows) | `nullable` (1 / 37 / 100 rows) |
|---|---|---|---|---|
| PyArrow / C++ | `pa.ipc.read_record_batch` (zero-copy mmap) | `ok / ok / ok` | `ok / ok / ok` | `ok / ok / ok` |
| PyArrow / C++ | `pa.ipc.open_stream` | `ok / ok / ok` | `ok / ok / ok` | `ok / ok / ok` |
| Rust (`arrow-rs`) | `read_record_batch` (`from_custom_allocation`) | `ok / ok / ok` | `ok / ok / ok` | `ok / ok / ok` |
| Rust (`arrow-rs`) | `StreamDecoder` (`with_require_alignment(true)`) | `ok / ok / ok` | `ok / ok / ok` | `ok / ok / ok` |
| Rust (`arrow-rs`) | `StreamReader` | `ok / ok / ok` | `ok / ok / ok` | `ok / ok / ok` |
| Go (`arrow-go`) | `ipc.NewMessage` + `NewReaderFromMessageReader` | `ok / ok / ok` | `ok / ok / ok` | `ok / ok / ok` |
| Go (`arrow-go`) | `ipc.NewReader` | `ok / ok / ok` | `ok / ok / ok` | `ok / ok / ok` |
| Java (`arrow-java`) | `MessageSerializer.deserializeRecordBatch` | `ok / ok / ok` | `ok / ok / ok` | `ok / ok / ok` |
| Java (`arrow-java`) | `ArrowStreamReader` | `ok / ok / ok` | `ok / ok / ok` | `ok / ok / ok` |

All **162/162 combinations** (81 from PyArrow-rendered templates + 81 from `arrow-rs`-rendered templates) passed with zero errors, zero warnings, and identical checksums.

### 2. Measured Patch Offsets (`launch` Schema: `float32[rows, 256]`)

`prefix_len = 192 B`, `body_len = 32,768 B`, `frame_cap = 32,960 B`. Only 4 eight-byte aligned fields in the 192-byte Flatbuffer prefix change between row counts:

| Flatbuffer Field | Formula | Byte Offset (`arrow-rs` render) | Value at 1 / 17 / 32 Rows |
|---|---|---|---|
| `RecordBatch.length` | `rows × 1` | `72` | `1 / 17 / 32` |
| `nodes[0].length` (list node) | `rows × 1` | `88` | `1 / 17 / 32` |
| `nodes[1].length` (child float32 node) | `rows × 256` | `104` | `256 / 4,352 / 8,192` |
| `buffers[2].length` (values buffer) | `rows × 1,024` | `168` | `1,024 / 17,408 / 32,768` |

---

## Architectural Conclusions

1. **`arrow-rs` at 64-byte alignment produces the exact capacity template directly:** Rendering a batch at `max_rows` with `IpcWriteOptions::try_new(64, false, MetadataVersion::V5)` places the body and every buffer on 64-byte boundaries without post-render buffer relocation.
2. **Walk the Flatbuffer AST to locate `PatchDesc` offsets ([03 — Arrow Data Plane §10](../../docs/components/03-arrow-data-plane.md#10-template-derivation-in-the-worker)):** Diffing two renders can miss fields whose values coincide across two row counts; walking the Flatbuffer `RecordBatch` (`length`, `nodes[i].length`, `nodes[i].null_count`, `buffers[j].length`) finds every patch offset deterministically.
3. **Render nullable columns with at least one null during template generation:** Stock Arrow IPC writers omit the validity bitmap buffer when `null_count == 0` at render time; rendering `max_rows` with at least one null ensures the template reserves the validity bitmap buffer at its capacity offset.
4. **Pre-validate buffer bounds before invoking generic `arrow-rs` readers ([03 — Arrow Data Plane §8](../../docs/components/03-arrow-data-plane.md#8-validation-and-safety)):** Because `arrow-rs`'s `Buffer::slice` panics if a corrupt Flatbuffer `Buffer` descriptor exceeds `bodyLength`, SidecarZero validates all node and buffer bounds before calling `arrow-rs` on generic frames.
