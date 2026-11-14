package shm

import (
	"sync/atomic"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
)

// Descriptor is what a cell carries, apart from seq. The slot record is authoritative; a
// worker acts only on SlabClass, SlotIndex, SlotEpoch, and DeadlineNs (shm-abi §4.2).
type Descriptor struct {
	RequestID  uint64
	SlotIndex  uint32
	DeadlineNs uint64
	SlotEpoch  uint32
	SlabClass  uint16
	EnqueueNs  uint64
	Flags      uint32
	Route      uint32
}

// EnqueueResult says whether an enqueue published.
type EnqueueResult uint8

const (
	// Enqueued: the descriptor is published and the consumer woken if it had parked.
	Enqueued EnqueueResult = iota
	// RingFull: the ring has no free cell.
	RingFull
	// Exhausted: every attempt was lost. In a consistent ring that takes 64 other producers
	// in about a microsecond; in practice it means a corrupt sequence word. A ring fault.
	Exhausted
)

// UnwindOnce is the test hook of sdk-contract §11 item 11: armed, the next Enqueue in this
// process panics between claiming a cell and publishing it, as a caller that unwinds there
// would. Only the conformance agent arms it.
var UnwindOnce atomic.Bool

// UnwindPanic is the value the armed enqueue panics with.
var UnwindPanic = unwindHook{}

type unwindHook struct{}

func (unwindHook) Error() string {
	return "sidecarzero test hook: unwinding between claiming a ring cell and publishing it"
}

// Cell is the offset of the cell at pos.
func (r RingGeom) Cell(pos uint64) uintptr {
	return r.Off + abi.RingHeaderCellsOff + uintptr(pos&(r.Capacity-1))*abi.CellLen
}

// writeCell writes the descriptor's fields and zeroes the cell's padding: the same bytes the
// reference client writes (eight words: seq, request_id, 0, slot_index << 32, deadline,
// slot_epoch | slab_class << 32, enqueue_ns, flags | route << 32).
func writeCell(data abi.Seg, cell uintptr, d *Descriptor) {
	data.SetCellRequestId(cell, d.RequestID)
	data.ClearCellPadding(cell)
	data.SetCellSlotIndex(cell, d.SlotIndex)
	data.SetCellDeadlineMonoNs(cell, d.DeadlineNs)
	data.SetCellSlotEpoch(cell, d.SlotEpoch)
	data.SetCellSlabClass(cell, d.SlabClass)
	data.SetCellEnqueueMonoNs(cell, d.EnqueueNs)
	data.SetCellFlags(cell, d.Flags)
	data.SetCellRoute(cell, d.Route)
}

// Enqueue is the producer side (shm-abi §4.3): it publishes d and wakes the consumer if it
// has parked. At most ENQUEUE_ATTEMPTS tries.
func Enqueue(data abi.Seg, r RingGeom, d *Descriptor) EnqueueResult {
	tail := r.Off
	for i := 0; i < abi.EnqueueAttempts; i++ {
		pos := data.RingHeaderTail(tail)
		cell := r.Cell(pos)
		seq := data.CellSeq(cell)
		if seq == pos {
			if data.CasRingHeaderTail(tail, pos, pos+1) {
				publish(data, r, cell, pos, d)
				return Enqueued
			}
		} else if seq < pos {
			return RingFull
		}
		// Another producer took the cell first, or the word is corrupt: go round again.
		CPURelax()
	}
	return Exhausted
}

// publish fills and publishes a claimed cell. Every claimed cell is published (sdk-contract
// §4): if the call unwinds here, the deferred guard publishes the cell as void, with the
// waker rule, and the panic goes on. An unpublished cell would stall the ring for every
// later call; the consumer skips a void one.
func publish(data abi.Seg, r RingGeom, cell uintptr, pos uint64, d *Descriptor) {
	crashPoint("ring.enqueue.claimed")
	done := false
	defer func() {
		if !done {
			void := Descriptor{Flags: abi.CellFlagVoid}
			writeCell(data, cell, &void)
			data.SetCellSeq(cell, pos+1)
			WakeIfParked(data, r)
		}
	}()
	if UnwindOnce.Load() && UnwindOnce.Swap(false) {
		panic(UnwindPanic)
	}
	writeCell(data, cell, d)
	crashPoint("ring.enqueue.written")
	done = true
	// The publish. Go's atomics are sequentially consistent, so it is also the store-load
	// barrier before the wait-word load below and the caller's gate re-check.
	data.SetCellSeq(cell, pos+1)
	WakeIfParked(data, r)
}

// WakeIfParked is the waker rule (signaling §3.1), applied after every publish.
func WakeIfParked(data abi.Seg, r RingGeom) {
	w := data.RingHeaderConsumerWait(r.Off)
	if w == abi.ConsumerWaitAwake {
		return
	}
	if w == abi.ConsumerWaitParked {
		data.CasRingHeaderConsumerWait(r.Off, abi.ConsumerWaitParked, abi.ConsumerWaitWaking)
	}
	data.FutexWake32(r.Off+abi.RingHeaderConsumerWait, 1)
}
