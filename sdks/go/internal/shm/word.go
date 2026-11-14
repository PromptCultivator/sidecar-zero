// Package shm is the client side of the arena protocol over internal/abi's accessors: the slot
// and gate words, the directory, the ring, slot transitions, the registry and its templates,
// and the model seqlock. It is the Go counterpart of the parts of scz-shm a client uses, and
// follows them step for step; every loop over memory another process can write is bounded.
package shm

import "github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"

// ── slot word: [ epoch:32 | reserved:21 | OWNER:2 | WAITER:1 | state:8 ] ──

// State is a slot word's state.
func State(w uint64) uint32 { return uint32((w & abi.SlotWordStateMask) >> abi.SlotWordStateShift) }

// Epoch is a slot word's epoch.
func Epoch(w uint64) uint32 { return uint32((w & abi.SlotWordEpochMask) >> abi.SlotWordEpochShift) }

// Waiter reports a slot word's WAITER bit.
func Waiter(w uint64) bool { return w&abi.SlotWordWaiterMask != 0 }

// Owner is a slot word's OWNER field.
func Owner(w uint64) uint32 { return uint32((w & abi.SlotWordOwnerMask) >> abi.SlotWordOwnerShift) }

// Word is a slot word with the given epoch and state, WAITER and OWNER clear.
func Word(epoch, state uint32) uint64 {
	return uint64(epoch)<<abi.SlotWordEpochShift | (uint64(state)<<abi.SlotWordStateShift)&abi.SlotWordStateMask
}

// WithOwner sets a word's OWNER field.
func WithOwner(w uint64, owner uint32) uint64 {
	return w&^abi.SlotWordOwnerMask | (uint64(owner)<<abi.SlotWordOwnerShift)&abi.SlotWordOwnerMask
}

// WithWaiter sets a word's WAITER bit.
func WithWaiter(w uint64) uint64 { return w | abi.SlotWordWaiterMask }

// Low32 is the half of the word a futex waits on (little-endian: the half at its address).
func Low32(w uint64) uint32 { return uint32(w) }

// ── gate word: [ worker_gen:32 | shed_permille:16 | reason:8 | state:8 ] ──

// GateState is a gate word's state.
func GateState(g uint64) uint32 { return uint32((g & abi.GateWordStateMask) >> abi.GateWordStateShift) }

// GateReason is a gate word's reason.
func GateReason(g uint64) uint32 {
	return uint32((g & abi.GateWordReasonMask) >> abi.GateWordReasonShift)
}

// GateGen is a gate word's worker generation.
func GateGen(g uint64) uint32 {
	return uint32((g & abi.GateWordWorkerGenMask) >> abi.GateWordWorkerGenShift)
}

// Gate packs a gate word.
func Gate(gen, reason, state uint32) uint64 {
	return uint64(gen)<<abi.GateWordWorkerGenShift | (uint64(reason)<<abi.GateWordReasonShift)&abi.GateWordReasonMask | (uint64(state)<<abi.GateWordStateShift)&abi.GateWordStateMask
}

// Healthy reports whether a gate word lets calls in. A state a reader does not know is not.
func Healthy(g uint64) bool { return GateState(g) == abi.GateStateHealthy }
