//go:build linux

package abi

import (
	"syscall"
	"unsafe"
)

const (
	futexWaitBitset   = 9
	futexWake         = 1
	futexBitsetAny    = 0xFFFFFFFF
	madvPopulateRead  = 22
	madvPopulateWrite = 23
)

// FutexWaitLow32 sleeps while the low half of the 64-bit word at off holds expected, until
// deadlineNs (absolute CLOCK_MONOTONIC), with a shared futex (signaling §5). False only on a
// timeout; a spurious return is possible, so callers re-read the word. The caller's goroutine
// holds its OS thread while it sleeps; the runtime hands its P to another thread.
func (s Seg) FutexWaitLow32(off uintptr, expected uint32, deadlineNs uint64) bool {
	return futexWait(s.addr(off, 8), expected, deadlineNs)
}

// FutexWait32 is FutexWaitLow32 on a 32-bit word.
func (s Seg) FutexWait32(off uintptr, expected uint32, deadlineNs uint64) bool {
	return futexWait(s.addr(off, 4), expected, deadlineNs)
}

func futexWait(addr uintptr, expected uint32, deadlineNs uint64) bool {
	var tsp uintptr
	var ts syscall.Timespec
	if deadlineNs != 0 {
		ts = syscall.Timespec{Sec: int64(deadlineNs / 1e9), Nsec: int64(deadlineNs % 1e9)}
		tsp = uintptr(unsafe.Pointer(&ts))
	}
	_, _, e := syscall.Syscall6(syscall.SYS_FUTEX, addr, futexWaitBitset, uintptr(expected), tsp, 0, futexBitsetAny)
	return e != syscall.ETIMEDOUT
}

// FutexWakeLow32 wakes up to n waiters on the 64-bit word at off. It never blocks, so it skips
// the scheduler's system-call bookkeeping.
func (s Seg) FutexWakeLow32(off uintptr, n uint32) {
	syscall.RawSyscall6(syscall.SYS_FUTEX, s.addr(off, 8), futexWake, uintptr(n), 0, 0, 0)
}

// FutexWake32 wakes up to n waiters on the 32-bit word at off.
func (s Seg) FutexWake32(off uintptr, n uint32) {
	syscall.RawSyscall6(syscall.SYS_FUTEX, s.addr(off, 4), futexWake, uintptr(n), 0, 0, 0)
}

// Mapping is one shared mapping of a memfd. Its memory is outside the Go heap: the garbage
// collector neither scans nor moves it, and only Unmap releases it.
type Mapping struct {
	b []byte
}

// Map maps length bytes of fd, MAP_SHARED, read-only or read-write.
func Map(fd int, length int, writable bool) (*Mapping, error) {
	prot := syscall.PROT_READ
	if writable {
		prot |= syscall.PROT_WRITE
	}
	b, err := syscall.Mmap(fd, 0, length, prot, syscall.MAP_SHARED)
	if err != nil {
		return nil, err
	}
	return &Mapping{b: b}, nil
}

// Seg is the view of the mapping.
func (m *Mapping) Seg() Seg {
	return Seg{base: unsafe.Pointer(unsafe.SliceData(m.b)), n: uintptr(len(m.b))}
}

// Populate fills the page tables now, so no request takes a first-touch fault (sdk-contract
// §6). Kernels before 5.14 lack the advice; the pages then fault in on first use.
func (m *Mapping) Populate(write bool) {
	advice := madvPopulateRead
	if write {
		advice = madvPopulateWrite
	}
	_ = syscall.Madvise(m.b, advice)
}

// Unmap releases the mapping. Only once no call can still be inside it.
func (m *Mapping) Unmap() error {
	if m.b == nil {
		return nil
	}
	err := syscall.Munmap(m.b)
	m.b = nil
	return err
}
