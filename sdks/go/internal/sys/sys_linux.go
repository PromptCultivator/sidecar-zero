//go:build linux

package sys

import (
	"syscall"
	"unsafe"
)

const clockMonotonic = 1

// MonotonicNs reads CLOCK_MONOTONIC with a real system call (syscall does not use the vDSO):
// hundreds of nanoseconds. The request path uses the SDK's cheap clock instead; this is its
// reference, read on the background goroutine to calibrate and check it (sdk-go §4).
func MonotonicNs() uint64 {
	var ts syscall.Timespec
	syscall.RawSyscall(syscall.SYS_CLOCK_GETTIME, clockMonotonic, uintptr(unsafe.Pointer(&ts)), 0)
	return uint64(ts.Sec)*1e9 + uint64(ts.Nsec)
}
