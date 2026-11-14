package shm

import "github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"

// The client's slot transitions (shm-abi §6.1). Plain stores only out of CLAIMED, DONE, and
// FAILED, which belong to the client alone; every exit from READY or PROCESSING is a CAS loop
// bounded at CAS_ATTEMPTS that tolerates the WAITER bit changing.

// TryClaim is FREE → CLAIMED with the epoch advanced. It returns the new epoch.
func TryClaim(data abi.Seg, rec uintptr) (uint32, bool) {
	w := data.SlotRecordWord(rec)
	if State(w) != abi.SlotStateFree {
		return 0, false
	}
	e := Epoch(w) + 1
	if !data.CasSlotRecordWord(rec, w, Word(e, abi.SlotStateClaimed)) {
		return 0, false
	}
	return e, true
}

// SetReady is CLAIMED → READY. After it the request region belongs to the worker.
func SetReady(data abi.Seg, rec uintptr, e uint32) {
	data.SetSlotRecordWord(rec, Word(e, abi.SlotStateReady))
}

// Release is CLAIMED, DONE, or FAILED → FREE.
func Release(data abi.Seg, rec uintptr, e uint32) {
	data.SetSlotRecordWord(rec, Word(e, abi.SlotStateFree))
}

// Cancel is the outcome of CancelIfReady.
type Cancel uint8

const (
	Cancelled Cancel = iota
	// NotReady: no longer READY at this epoch; the worker or the sweep owns the outcome.
	NotReady
	// GaveUp: the word kept changing; something writes it that should not.
	GaveUp
)

// CancelIfReady is READY → FREE, for a request that never reached the worker.
func CancelIfReady(data abi.Seg, rec uintptr, e uint32) Cancel {
	for i := 0; i < abi.CasAttempts; i++ {
		w := data.SlotRecordWord(rec)
		if State(w) != abi.SlotStateReady || Epoch(w) != e {
			return NotReady
		}
		if data.CasSlotRecordWord(rec, w, Word(e, abi.SlotStateFree)) {
			return Cancelled
		}
	}
	return GaveUp
}

// Abandon is the outcome of AbandonSlot.
type Abandon uint8

const (
	// Freed: still queued, freed on the spot.
	Freed Abandon = iota
	// Abandoned: being processed, handed to the worker to free, OWNER kept.
	Abandoned
	// AbandonDone: the worker won the race; the answer is valid.
	AbandonDone
	// AbandonFailed: the sweep failed the slot.
	AbandonFailed
	// AbandonGaveUp: the word kept changing, or names another epoch.
	AbandonGaveUp
)

// AbandonSlot is what a caller does once its deadline has passed (sdk-contract §4).
func AbandonSlot(data abi.Seg, rec uintptr, e uint32) Abandon {
	for i := 0; i < abi.CasAttempts; i++ {
		w := data.SlotRecordWord(rec)
		if Epoch(w) != e {
			return AbandonGaveUp
		}
		switch State(w) {
		case abi.SlotStateReady:
			if data.CasSlotRecordWord(rec, w, Word(e, abi.SlotStateFree)) {
				return Freed
			}
		case abi.SlotStateProcessing:
			// The one transition by a non-owner that carries OWNER across (shm-abi §6.1).
			if data.CasSlotRecordWord(rec, w, WithOwner(Word(e, abi.SlotStateAbandoned), Owner(w))) {
				return Abandoned
			}
		case abi.SlotStateDone:
			return AbandonDone
		case abi.SlotStateFailed:
			return AbandonFailed
		default:
			return AbandonGaveUp
		}
	}
	return AbandonGaveUp
}
