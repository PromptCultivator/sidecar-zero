//go:build !linux

package abi

import (
	"errors"
	"time"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/sys"
)

// Off Linux there is no futex and nothing to map: the client never connects (sdk-contract §9).
// The waits below poll, for the protocol's unit tests over local memory.

func (s Seg) FutexWaitLow32(off uintptr, expected uint32, deadlineNs uint64) bool {
	return poll(func() bool { return uint32(s.ld64(off)) != expected }, deadlineNs)
}

func (s Seg) FutexWait32(off uintptr, expected uint32, deadlineNs uint64) bool {
	return poll(func() bool { return s.ld32(off) != expected }, deadlineNs)
}

func poll(changed func() bool, deadlineNs uint64) bool {
	for !changed() {
		if sys.MonotonicNs() >= deadlineNs {
			return false
		}
		time.Sleep(20 * time.Microsecond)
	}
	return true
}

func (s Seg) FutexWakeLow32(off uintptr, n uint32) {}
func (s Seg) FutexWake32(off uintptr, n uint32)    {}

type Mapping struct{}

func Map(fd int, length int, writable bool) (*Mapping, error) {
	return nil, errors.New("sidecarzero: shared memory is supported on Linux only")
}
func (m *Mapping) Seg() Seg            { return Seg{} }
func (m *Mapping) Populate(write bool) {}
func (m *Mapping) Unmap() error        { return nil }
