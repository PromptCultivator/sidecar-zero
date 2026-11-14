// Spike S4, Go side (github.com/apache/arrow-go/v18).
//
//	s4-go FRAMES_DIR     read every frame in manifest.tsv, print one JSON line per (frame, mode)
//
// Arrow Go has no public "decode one RecordBatch message against a known
// schema" function: a Reader always takes the Schema message first. So a slot is
// read as the registry's Schema message followed by the slot's region. Modes:
//
//	ipc.NewReader                  the stock stream reader over Schema message + region bytes. It copies
//	                               the body into memory from its allocator.
//	NewReaderFromMessageReader     the "message reader" route docs/components/03-arrow-data-plane.md
//	                               section 7.4 names: the region (an anonymous shared mmap, page-aligned
//	                               like a slot) is wrapped with memory.NewBufferBytes, turned into an
//	                               ipc.Message with ipc.NewMessage, and handed to the stock Reader.
//
// Arrow Go has no full validation for primitive arrays; ValidateFull is called
// on the arrays that have one (FixedSizeList here).
package main

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"unsafe"

	"github.com/apache/arrow-go/v18/arrow"
	"github.com/apache/arrow-go/v18/arrow/array"
	"github.com/apache/arrow-go/v18/arrow/ipc"
	"github.com/apache/arrow-go/v18/arrow/memory"
)

const lib = "arrow-go v18.8.0"

type colOut struct {
	Name  string `json:"name"`
	Nulls int    `json:"nulls"`
	Fnv   string `json:"fnv"`
}

type result struct {
	Reader       string   `json:"reader"`
	Lib          string   `json:"lib"`
	Mode         string   `json:"mode"`
	Frame        string   `json:"frame"`
	Ok           bool     `json:"ok"`
	Rows         *int64   `json:"rows,omitempty"`
	Schema       string   `json:"schema,omitempty"`
	ValidateFull string   `json:"validate_full,omitempty"`
	Cols         []colOut `json:"cols,omitempty"`
	Borrowed     *bool    `json:"borrowed,omitempty"`
	Detail       string   `json:"detail,omitempty"`
	Error        string   `json:"error,omitempty"`
}

// ---------------------------------------------------------------- checksum

func feed(h *uint64, b []byte) {
	for _, x := range b {
		*h = (*h ^ uint64(x)) * 0x100000001b3
	}
}

func feedElem(h *uint64, a arrow.Array, i int) error {
	var b [8]byte
	switch v := a.(type) {
	case *array.Float32:
		binary.LittleEndian.PutUint32(b[:4], math.Float32bits(v.Value(i)))
		feed(h, b[:4])
	case *array.Float64:
		binary.LittleEndian.PutUint64(b[:8], math.Float64bits(v.Value(i)))
		feed(h, b[:8])
	case *array.Int64:
		binary.LittleEndian.PutUint64(b[:8], uint64(v.Value(i)))
		feed(h, b[:8])
	case *array.Int32:
		binary.LittleEndian.PutUint32(b[:4], uint32(v.Value(i)))
		feed(h, b[:4])
	case *array.Uint8:
		feed(h, []byte{v.Value(i)})
	default:
		return fmt.Errorf("unexpected element type %s", a.DataType())
	}
	return nil
}

func columnSummary(col arrow.Array) (int, string, error) {
	h := uint64(0xcbf29ce484222325)
	for r := 0; r < col.Len(); r++ {
		if col.IsNull(r) {
			feed(&h, []byte{0})
			continue
		}
		feed(&h, []byte{1})
		if l, ok := col.(*array.FixedSizeList); ok {
			start, end := l.ValueOffsets(r)
			vals := l.ListValues()
			for j := start; j < end; j++ {
				if err := feedElem(&h, vals, int(j)); err != nil {
					return 0, "", err
				}
			}
		} else if err := feedElem(&h, col, r); err != nil {
			return 0, "", err
		}
	}
	return col.NullN(), fmt.Sprintf("%016x", h), nil
}

func valuesAddr(col arrow.Array) uintptr {
	d := col.Data()
	if l, ok := col.(*array.FixedSizeList); ok {
		d = l.ListValues().Data()
	}
	b := d.Buffers()[1].Bytes()
	if len(b) == 0 {
		return 0
	}
	return uintptr(unsafe.Pointer(&b[0]))
}

// ---------------------------------------------------------------- region

func mmapRegion(b []byte) ([]byte, error) {
	n := len(b)
	if n == 0 {
		n = 1
	}
	m, err := syscall.Mmap(-1, 0, n, syscall.PROT_READ|syscall.PROT_WRITE, syscall.MAP_ANON|syscall.MAP_SHARED)
	if err != nil {
		return nil, err
	}
	copy(m, b)
	return m[:len(b)], nil
}

// splitMessage wraps one encapsulated message found at the start of b, without copying.
func splitMessage(b []byte) (*ipc.Message, error) {
	if len(b) < 8 || binary.LittleEndian.Uint32(b) != 0xFFFFFFFF {
		return nil, errors.New("no continuation marker")
	}
	metaLen := int(int32(binary.LittleEndian.Uint32(b[4:])))
	if metaLen < 4 || 8+metaLen > len(b) {
		return nil, errors.New("metadata_size runs past the region")
	}
	meta := memory.NewBufferBytes(b[8 : 8+metaLen])
	probe := ipc.NewMessage(meta, memory.NewBufferBytes(nil))
	bodyLen := int(probe.BodyLen())
	probe.Release()
	if bodyLen < 0 || 8+metaLen+bodyLen > len(b) {
		return nil, errors.New("bodyLength runs past the region")
	}
	return ipc.NewMessage(meta, memory.NewBufferBytes(b[8+metaLen:8+metaLen+bodyLen])), nil
}

type sliceMessages struct {
	msgs []*ipc.Message
	i    int
}

func (s *sliceMessages) Message() (*ipc.Message, error) {
	if s.i >= len(s.msgs) {
		return nil, io.EOF
	}
	m := s.msgs[s.i]
	s.i++
	return m, nil
}
func (s *sliceMessages) Release() {}
func (s *sliceMessages) Retain()  {}

// ---------------------------------------------------------------- modes

func describe(rec arrow.RecordBatch, out *result, base uintptr, n int) error {
	rows := rec.NumRows()
	out.Rows = &rows
	var fs []string
	for _, f := range rec.Schema().Fields() {
		s := f.Name + ": " + f.Type.String()
		if !f.Nullable {
			s += " not null"
		}
		fs = append(fs, s)
	}
	out.Schema = strings.Join(fs, "; ")
	out.ValidateFull = "n/a"
	borrowed := true
	for i, col := range rec.Columns() {
		if v, ok := col.(interface{ ValidateFull() error }); ok {
			if err := v.ValidateFull(); err != nil {
				out.ValidateFull = fmt.Sprintf("column %d: %v", i, err)
			} else if out.ValidateFull == "n/a" {
				out.ValidateFull = "ok"
			}
		}
		nulls, fnv, err := columnSummary(col)
		if err != nil {
			return err
		}
		out.Cols = append(out.Cols, colOut{rec.Schema().Field(i).Name, nulls, fnv})
		a := valuesAddr(col)
		if a < base || a >= base+uintptr(n) {
			borrowed = false
		}
	}
	if n > 0 {
		out.Borrowed = &borrowed
	}
	return nil
}

func drain(rdr *ipc.Reader, out *result, base uintptr, n int) error {
	if !rdr.Next() {
		if err := rdr.Err(); err != nil {
			return err
		}
		return errors.New("stream ended before a batch")
	}
	if err := describe(rdr.RecordBatch(), out, base, n); err != nil {
		return err
	}
	if rdr.Next() {
		out.Detail += " a second batch followed"
	} else if err := rdr.Err(); err != nil && !errors.Is(err, io.EOF) {
		out.Detail += " after the frame: " + err.Error()
	} else {
		out.Detail += " stream ended cleanly after the frame"
	}
	return nil
}

func modeStream(schemaMsg, frame []byte, out *result) error {
	rdr, err := ipc.NewReader(io.MultiReader(bytes.NewReader(schemaMsg), bytes.NewReader(frame)),
		ipc.WithAllocator(memory.NewGoAllocator()))
	if err != nil {
		return err
	}
	defer rdr.Release()
	return drain(rdr, out, 0, 0)
}

func modeMessages(schemaMsg, frame []byte, out *result) error {
	region, err := mmapRegion(frame)
	if err != nil {
		return err
	}
	// The mapping is left in place: the batch borrows it.
	sm, err := splitMessage(schemaMsg)
	if err != nil {
		return fmt.Errorf("schema message: %w", err)
	}
	fm, err := splitMessage(region)
	if err != nil {
		return err
	}
	base := uintptr(unsafe.Pointer(&region[0]))
	out.Detail = fmt.Sprintf("message type=%s body=%dB region@%dmod4096;", fm.Type(), fm.BodyLen(), base%4096)
	rdr, err := ipc.NewReaderFromMessageReader(&sliceMessages{msgs: []*ipc.Message{sm, fm}},
		ipc.WithAllocator(memory.NewGoAllocator()))
	if err != nil {
		return err
	}
	defer rdr.Release()
	return drain(rdr, out, base, len(region))
}

func run(frameID, mode string, f func(*result) error) {
	out := result{Reader: "go", Lib: lib, Mode: mode, Frame: frameID}
	func() {
		defer func() {
			if p := recover(); p != nil {
				out = result{Reader: "go", Lib: lib, Mode: mode, Frame: frameID, Error: fmt.Sprintf("PANIC: %v", p)}
			}
		}()
		if err := f(&out); err != nil {
			out = result{Reader: "go", Lib: lib, Mode: mode, Frame: frameID, Error: err.Error(), Detail: out.Detail}
			return
		}
		out.Ok = true
	}()
	b, _ := json.Marshal(out)
	fmt.Println(string(b))
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: s4-go FRAMES_DIR")
		os.Exit(2)
	}
	dir := os.Args[1]
	manifest, err := os.ReadFile(filepath.Join(dir, "manifest.tsv"))
	if err != nil {
		panic(err)
	}
	for _, line := range strings.Split(strings.TrimSpace(string(manifest)), "\n") {
		f := strings.Split(line, "\t")
		frameID := f[0]
		schemaMsg, err := os.ReadFile(filepath.Join(dir, f[5]))
		if err != nil {
			panic(err)
		}
		frame, err := os.ReadFile(filepath.Join(dir, f[6]))
		if err != nil {
			panic(err)
		}
		run(frameID, "ipc.NewReader", func(o *result) error { return modeStream(schemaMsg, frame, o) })
		run(frameID, "NewReaderFromMessageReader", func(o *result) error { return modeMessages(schemaMsg, frame, o) })
	}
}
