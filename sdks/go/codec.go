package sidecarzero

import (
	"errors"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/shm"
)

// Codec turns an input into a request in a slot and the response into an output (sdk-go §2).
//
// A codec either declares columns, and then binds only to a registry with the same schema
// fingerprint and writes Arrow frames laid out by its templates (arrow-data-plane §7, §9), or
// declares none (fingerprint 0), carries opaque bytes, and binds only to an empty registry.
// Any other pairing falls back with SCHEMA_MISMATCH. Every method must be safe to call from
// many goroutines at once, and must not allocate if the client is to stay allocation-free.
type Codec[In, Out any] interface {
	// SchemaFingerprint is Fingerprint of the codec's columns, or 0 for opaque bytes. Called
	// once, by New.
	SchemaFingerprint() uint64
	// Size is what the request needs: its length in bytes for opaque bytes, its row count for
	// frames. The smallest slot class that holds it is used; none: TOO_LARGE. Negative: the
	// input cannot be encoded (CODEC_ERROR).
	Size(in In) int
	// Encode writes the request. A non-nil error (or a panic) releases the slot and resolves
	// the call to the fallback with CODEC_ERROR.
	Encode(in In, w SlotWriter) error
	// Decode reads the response. It receives the input too, so that it can decode into
	// buffers the caller passed with it instead of allocating. A non-nil error (or a panic)
	// resolves the call to the fallback with CODEC_ERROR.
	Decode(in In, r SlotReader) (Out, error)
}

// Codec errors are pre-allocated: constructing one per call would allocate.
var (
	// ErrShape: the input or the response does not have the shape the codec expects.
	ErrShape = errors.New("sidecarzero: wrong shape")
	// ErrOverflow: it does not fit the region.
	ErrOverflow = errors.New("sidecarzero: does not fit the slot")
	// ErrCodecPanic: the codec panicked; recovered, it counts as a codec error.
	ErrCodecPanic = errors.New("sidecarzero: the codec panicked")
)

// ElemType is a column's physical element type (abi/scz_abi.toml [enums.elem_type]).
type ElemType uint8

const (
	I8          ElemType = abi.ElemTypeI8
	I16         ElemType = abi.ElemTypeI16
	I32         ElemType = abi.ElemTypeI32
	I64         ElemType = abi.ElemTypeI64
	U8          ElemType = abi.ElemTypeU8
	U16         ElemType = abi.ElemTypeU16
	U32         ElemType = abi.ElemTypeU32
	U64         ElemType = abi.ElemTypeU64
	F16         ElemType = abi.ElemTypeF16
	F32         ElemType = abi.ElemTypeF32
	F64         ElemType = abi.ElemTypeF64
	FixedBinary ElemType = abi.ElemTypeFixedBinary
)

// Column is one declared column: its name, element type, list size (1 for a scalar, N for
// FixedSizeList<T>[N], the byte width for FixedBinary), and nullability.
type Column struct {
	Name     string
	Elem     ElemType
	ListSize uint32
	Nullable bool
}

// Schema is a codec's request and response columns.
type Schema struct {
	Request, Response []Column
}

// Fingerprint is the schema fingerprint of arrow-data-plane §9 (xxHash3-64, seed 0).
func Fingerprint(s Schema) uint64 {
	conv := func(cs []Column) []shm.Column {
		out := make([]shm.Column, len(cs))
		for i, c := range cs {
			out[i] = shm.Column{Name: c.Name, Elem: uint8(c.Elem), ListSize: c.ListSize, Nullable: c.Nullable}
		}
		return out
	}
	return shm.Fingerprint(shm.Schema{Request: conv(s.Request), Response: conv(s.Response)})
}

// SlotWriter writes a request into its slot's request region, bounds-checked. For opaque
// bytes it takes bytes; for frames the frame's metadata is already in place and it takes
// column values, each write checked against the column's type and the frame's row count.
// It is valid only during Encode.
type SlotWriter struct {
	seg  abi.Seg
	off  uintptr
	cap  uintptr
	n    *uint32 // opaque bytes: one past the highest byte written (in the caller's entry)
	tmpl *shm.Template
	rows uint32
}

// Cap is the region's capacity in bytes.
func (w SlotWriter) Cap() int { return int(w.cap) }

// Len is one past the highest byte written so far (opaque bytes).
func (w SlotWriter) Len() int {
	if w.n == nil {
		return 0
	}
	return int(*w.n)
}

// Rows is the frame's row count (frames).
func (w SlotWriter) Rows() int { return int(w.rows) }

// WriteAt writes opaque bytes at offset at.
func (w SlotWriter) WriteAt(at int, p []byte) error {
	if w.tmpl != nil || w.n == nil {
		return ErrShape
	}
	if at < 0 || uintptr(at) > w.cap || w.cap-uintptr(at) < uintptr(len(p)) {
		return ErrOverflow
	}
	w.seg.CopyIn(w.off+uintptr(at), p)
	if end := uint32(at + len(p)); end > *w.n {
		*w.n = end
	}
	return nil
}

// Write appends opaque bytes after the highest byte written so far.
func (w SlotWriter) Write(p []byte) error { return w.WriteAt(w.Len(), p) }

func (w SlotWriter) column(col int, elem ElemType) (*shm.ColumnLoc, error) {
	if w.tmpl == nil || col < 0 || col >= len(w.tmpl.Columns) {
		return nil, ErrShape
	}
	c := &w.tmpl.Columns[col]
	if elem != 0 && c.Column.Elem != uint8(elem) {
		return nil, ErrShape
	}
	return c, nil
}

// WriteF32 writes column col's values for every row, row-major: rows × list size floats.
// Shorter or longer input is refused before anything is written (sdk-go §2: Go's copy would
// silently copy a short slice).
func (w SlotWriter) WriteF32(col int, values []float32) error {
	c, err := w.column(col, F32)
	if err != nil {
		return err
	}
	if len(values) != int(w.rows)*int(c.Column.ListSize) {
		return ErrShape
	}
	w.seg.CopyInF32(w.off+uintptr(c.ValuesOff), values)
	return nil
}

// WriteF32Row writes the values of row row alone.
func (w SlotWriter) WriteF32Row(col, row int, values []float32) error {
	c, err := w.column(col, F32)
	if err != nil {
		return err
	}
	if len(values) != int(c.Column.ListSize) || row < 0 || row >= int(w.rows) {
		return ErrShape
	}
	w.seg.CopyInF32(w.off+uintptr(c.ValuesOff)+uintptr(row)*uintptr(c.RowBytes), values)
	return nil
}

// WriteColumnBytes writes column col's raw value bytes for every row, any fixed-width type.
func (w SlotWriter) WriteColumnBytes(col int, b []byte) error {
	c, err := w.column(col, 0)
	if err != nil {
		return err
	}
	if len(b) != int(w.rows)*int(c.RowBytes) {
		return ErrShape
	}
	w.seg.CopyIn(w.off+uintptr(c.ValuesOff), b)
	return nil
}

// SlotReader reads a response whose length was read once and checked against its region.
// It is valid only during Decode: after Decode returns the slot is released.
type SlotReader struct {
	seg  abi.Seg
	off  uintptr
	len  uintptr
	tmpl *shm.Template
	rows uint32
}

// Len is the response's length in bytes.
func (r SlotReader) Len() int { return int(r.len) }

// Rows is the frame's row count (frames).
func (r SlotReader) Rows() int { return int(r.rows) }

// CopyTo copies len(dst) response bytes at offset at into dst.
func (r SlotReader) CopyTo(at int, dst []byte) error {
	if at < 0 || uintptr(at) > r.len || r.len-uintptr(at) < uintptr(len(dst)) {
		return ErrOverflow
	}
	r.seg.CopyOut(r.off+uintptr(at), dst)
	return nil
}

// Uint64 reads a little-endian uint64 at offset at.
func (r SlotReader) Uint64(at int) (uint64, error) {
	var b [8]byte
	if err := r.CopyTo(at, b[:]); err != nil {
		return 0, err
	}
	return uint64(b[0]) | uint64(b[1])<<8 | uint64(b[2])<<16 | uint64(b[3])<<24 | uint64(b[4])<<32 | uint64(b[5])<<40 | uint64(b[6])<<48 | uint64(b[7])<<56, nil
}

func (r SlotReader) column(col int, elem ElemType) (*shm.ColumnLoc, error) {
	if r.tmpl == nil || col < 0 || col >= len(r.tmpl.Columns) {
		return nil, ErrShape
	}
	c := &r.tmpl.Columns[col]
	if elem != 0 && c.Column.Elem != uint8(elem) {
		return nil, ErrShape
	}
	return c, nil
}

// ReadF32 copies column col's values for every row into dst, which must hold exactly rows ×
// list size floats.
func (r SlotReader) ReadF32(col int, dst []float32) error {
	c, err := r.column(col, F32)
	if err != nil {
		return err
	}
	if len(dst) != int(r.rows)*int(c.Column.ListSize) {
		return ErrShape
	}
	r.seg.CopyOutF32(r.off+uintptr(c.ValuesOff), dst)
	return nil
}

// F32 reads element i of column col (row-major) with one atomic load.
func (r SlotReader) F32(col, i int) (float32, error) {
	c, err := r.column(col, F32)
	if err != nil {
		return 0, err
	}
	if i < 0 || i >= int(r.rows)*int(c.Column.ListSize) {
		return 0, ErrShape
	}
	return r.seg.Float32(r.off + uintptr(c.ValuesOff) + uintptr(i)*4), nil
}

// ── prebuilt codecs ────────────────────────────────────────────────────────────────────

// DenseF32 is the launch workload's codec: rows of N float32 features in, one float32 score
// per row out (`features: float32[rows, N]` → `score: float32[rows, 1]`, column names
// configurable). The request is one FixedSizeList<float32>[N] column, the response one
// float32 column. It is a small value; pass it by value.
type DenseF32 struct {
	N      int
	Input  string
	Output string
	fp     uint64
}

// NewDenseF32 is DenseF32 with N features and the columns "features" and "score".
func NewDenseF32(n int) DenseF32 {
	return DenseF32{N: n, Input: "features", Output: "score"}.withFingerprint()
}

func (d DenseF32) withFingerprint() DenseF32 {
	d.fp = Fingerprint(d.Schema())
	return d
}

// Schema is the codec's columns.
func (d DenseF32) Schema() Schema {
	return Schema{Request: []Column{{Name: d.Input, Elem: F32, ListSize: uint32(d.N)}}, Response: []Column{{Name: d.Output, Elem: F32, ListSize: 1}}}
}

// F32Batch is DenseF32Batch's input: rows × N features, row-major, and room for the scores,
// which Decode fills and returns, so that a call allocates nothing.
type F32Batch struct {
	Features []float32
	Scores   []float32
}

// DenseF32Batch carries any number of rows per call (the batch slot class when it is on).
type DenseF32Batch struct{ DenseF32 }

// NewDenseF32Batch is DenseF32Batch with N features and the columns "features" and "score".
func NewDenseF32Batch(n int) DenseF32Batch { return DenseF32Batch{NewDenseF32(n)} }

func (d DenseF32Batch) SchemaFingerprint() uint64 {
	if d.fp == 0 {
		return Fingerprint(d.Schema())
	}
	return d.fp
}

func (d DenseF32Batch) Size(in F32Batch) int {
	if d.N <= 0 || len(in.Features) == 0 || len(in.Features)%d.N != 0 {
		return -1
	}
	return len(in.Features) / d.N
}

func (d DenseF32Batch) Encode(in F32Batch, w SlotWriter) error {
	if len(in.Features) != w.Rows()*d.N {
		return ErrShape
	}
	return w.WriteF32(0, in.Features)
}

func (d DenseF32Batch) Decode(in F32Batch, r SlotReader) ([]float32, error) {
	if len(in.Scores) < r.Rows() {
		return nil, ErrShape
	}
	out := in.Scores[:r.Rows()]
	return out, r.ReadF32(0, out)
}

// DenseF32Score is one row in, one score out: In is the row's N features.
type DenseF32Score struct{ DenseF32 }

// NewDenseF32Score is DenseF32Score with N features and the columns "features" and "score".
func NewDenseF32Score(n int) DenseF32Score { return DenseF32Score{NewDenseF32(n)} }

func (d DenseF32Score) SchemaFingerprint() uint64 {
	if d.fp == 0 {
		return Fingerprint(d.Schema())
	}
	return d.fp
}

func (d DenseF32Score) Size(in []float32) int {
	if len(in) != d.N {
		return -1
	}
	return 1
}

func (d DenseF32Score) Encode(in []float32, w SlotWriter) error { return w.WriteF32(0, in) }

func (d DenseF32Score) Decode(_ []float32, r SlotReader) (float32, error) { return r.F32(0, 0) }

// RawCall is the Raw codec's input: the request bytes, and a buffer for the response.
type RawCall struct {
	Request  []byte
	Response []byte
}

// Raw carries opaque bytes, as the echo and chaos workers expect: the request as given, the
// response copied into RawCall.Response and returned as a slice of it.
type Raw struct{}

func (Raw) SchemaFingerprint() uint64             { return 0 }
func (Raw) Size(in RawCall) int                   { return len(in.Request) }
func (Raw) Encode(in RawCall, w SlotWriter) error { return w.Write(in.Request) }
func (Raw) Decode(in RawCall, r SlotReader) ([]byte, error) {
	if r.Len() > len(in.Response) {
		return nil, ErrOverflow
	}
	out := in.Response[:r.Len()]
	return out, r.CopyTo(0, out)
}
