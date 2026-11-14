// Seg and its primitives: the only code in the SDK that touches arena memory.
//
// Every word another process can write is reached through an atomic load or store (sdk-go
// §3: "every read of a field another process can write is an atomic load"), through the
// generated accessors in abi.go, which are the only callers of the unexported primitives
// below. Payload regions (a slot's request and response bytes) are copied in and out in bulk
// with CopyIn and CopyOut, bounds-checked against the segment; nothing hands out a Go slice
// over arena memory, so no code outside this package can read or write it by accident. The
// lint in lint_test.go enforces that.

package abi

import (
	"math"
	"strconv"
	"sync/atomic"
	"unsafe"
)

// Seg is a view of one segment: a mapped control or data segment, or ordinary memory standing
// in for one in tests. It is valid while the memory is: the SDK unmaps a segment only once no
// call can still be inside it (sdk-contract §6, "Retiring an old arena").
type Seg struct {
	base unsafe.Pointer
	n    uintptr
}

// Len is the segment's length in bytes.
func (s Seg) Len() uintptr { return s.n }

// Valid reports whether the view refers to memory.
func (s Seg) Valid() bool { return s.base != nil }

// Same reports whether two views are of the same memory.
func (s Seg) Same(o Seg) bool { return s.base == o.base && s.n == o.n }

//go:noinline
func outside(off, w, n uintptr) {
	panic("sidecarzero: offset " + strconv.FormatUint(uint64(off), 16) + " (width " + strconv.FormatUint(uint64(w), 10) + ") outside a segment of " + strconv.FormatUint(uint64(n), 10) + " bytes")
}

func (s Seg) at(off, w uintptr) unsafe.Pointer {
	if off&(w-1) != 0 || off > s.n || s.n-off < w {
		outside(off, w, s.n)
	}
	return unsafe.Add(s.base, off)
}

func (s Seg) ld64(off uintptr) uint64    { return atomic.LoadUint64((*uint64)(s.at(off, 8))) }
func (s Seg) st64(off uintptr, v uint64) { atomic.StoreUint64((*uint64)(s.at(off, 8)), v) }
func (s Seg) ld32(off uintptr) uint32    { return atomic.LoadUint32((*uint32)(s.at(off, 4))) }
func (s Seg) st32(off uintptr, v uint32) { atomic.StoreUint32((*uint32)(s.at(off, 4)), v) }
func (s Seg) add64(off uintptr, d uint64) uint64 {
	return atomic.AddUint64((*uint64)(s.at(off, 8)), d)
}
func (s Seg) add32(off uintptr, d uint32) uint32 {
	return atomic.AddUint32((*uint32)(s.at(off, 4)), d)
}
func (s Seg) cas64(off uintptr, old, new uint64) bool {
	return atomic.CompareAndSwapUint64((*uint64)(s.at(off, 8)), old, new)
}
func (s Seg) cas32(off uintptr, old, new uint32) bool {
	return atomic.CompareAndSwapUint32((*uint32)(s.at(off, 4)), old, new)
}

// Word64 loads the 64-bit word at off, for memory whose layout the ABI file describes as
// words rather than fields: a frame's length fields, the registry bytes under the model
// seqlock, a frame's row count.
func (s Seg) Word64(off uintptr) uint64 { return s.ld64(off) }

// SetWord64 stores the 64-bit word at off: a frame's length fields, in a request region the
// caller owns under the slot protocol.
func (s Seg) SetWord64(off uintptr, v uint64) { s.st64(off, v) }

// Word32 loads the 32-bit word at off: one float32 of a response frame.
func (s Seg) Word32(off uintptr) uint32 { return s.ld32(off) }

// Float32 loads one float32 at off, as a single atomic load of its bits.
func (s Seg) Float32(off uintptr) float32 { return math.Float32frombits(s.ld32(off)) }

func (s Seg) span(off, n uintptr) unsafe.Pointer {
	if off > s.n || s.n-off < n {
		outside(off, n, s.n)
	}
	return unsafe.Add(s.base, off)
}

// CopyIn copies src into the segment at off: a payload region the caller owns under the slot
// protocol (a request region between CLAIMED and READY).
func (s Seg) CopyIn(off uintptr, src []byte) {
	if len(src) == 0 {
		return
	}
	p := s.span(off, uintptr(len(src)))
	copy(unsafe.Slice((*byte)(p), len(src)), src)
}

// CopyOut copies len(dst) bytes at off out of the segment: a response region the caller owns
// (DONE, before the release). The other process may still write it; the copy is what the
// codec decodes, so it sees one set of bytes.
func (s Seg) CopyOut(off uintptr, dst []byte) {
	if len(dst) == 0 {
		return
	}
	p := s.span(off, uintptr(len(dst)))
	copy(dst, unsafe.Slice((*byte)(p), len(dst)))
}

// CopyInF32 copies float32 values into the segment at off, little-endian (both supported
// architectures are), off 4-byte aligned.
func (s Seg) CopyInF32(off uintptr, src []float32) {
	if len(src) == 0 {
		return
	}
	if off&3 != 0 {
		outside(off, 4, s.n)
	}
	p := s.span(off, uintptr(len(src))*4)
	copy(unsafe.Slice((*float32)(p), len(src)), src)
}

// CopyOutF32 copies len(dst) float32 values at off out of the segment.
func (s Seg) CopyOutF32(off uintptr, dst []float32) {
	if len(dst) == 0 {
		return
	}
	if off&3 != 0 {
		outside(off, 4, s.n)
	}
	p := s.span(off, uintptr(len(dst))*4)
	copy(dst, unsafe.Slice((*float32)(p), len(dst)))
}

// CopyWithin copies n bytes from src to dst inside the segment (the in-process echo worker).
func (s Seg) CopyWithin(src, dst, n uintptr) {
	if n == 0 {
		return
	}
	ps, pd := s.span(src, n), s.span(dst, n)
	copy(unsafe.Slice((*byte)(pd), n), unsafe.Slice((*byte)(ps), n))
}

// addr is the address of the word at off, for the futex system call only.
func (s Seg) addr(off uintptr, w uintptr) uintptr { return uintptr(s.at(off, w)) }

// Local is ordinary, page-aligned, zeroed memory standing in for a segment: tests, golden
// dumps, and the in-process test double. No other process can map it. The memory lives as
// long as the Local or any Seg over it is reachable.
type Local struct {
	mem []byte
	seg Seg
}

// NewLocal allocates n bytes, page-aligned.
func NewLocal(n uintptr) *Local {
	mem := make([]byte, n+4096)
	p := unsafe.Pointer(unsafe.SliceData(mem))
	pad := (4096 - uintptr(p)&4095) & 4095
	return &Local{mem: mem, seg: Seg{base: unsafe.Add(p, pad), n: n}}
}

// Seg is the view of the memory.
func (l *Local) Seg() Seg { return l.seg }

// WorkerSeg is a view with the worker's stores: the in-process worker behind the test double
// and the SDK's own tests (lint_test.go keeps it out of the client).
type WorkerSeg struct{ s Seg }

// Worker is the worker's view of a segment.
func Worker(s Seg) WorkerSeg { return WorkerSeg{s} }

// SupervisorSeg is a view with the supervisor's stores, for building an arena in ordinary
// memory: the in-process double and the SDK's own tests.
type SupervisorSeg struct{ s Seg }

// Supervisor is the supervisor's view of a segment.
func Supervisor(s Seg) SupervisorSeg { return SupervisorSeg{s} }
