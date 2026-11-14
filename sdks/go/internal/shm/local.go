package shm

import (
	"errors"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
)

// The supervisor's and the worker's side of the arena, as far as an arena in ordinary memory
// needs them: the SDK's in-process test double (sdk-contract §9) and its own tests. They are
// written against abi's SupervisorSeg and WorkerSeg views, which the client never uses.

// Geometry is what a supervisor asks for.
type Geometry struct {
	Rings       []uint64 // capacities
	SlabClasses []SlabClass
	StagingLen  uint64
}

// Config is the per-arena values that are not layout.
type Config struct {
	ArenaID, CreatedMonoNs                                            uint64
	HbIntervalNs, HbStaleNs, SpinNs, DefaultDeadlineNs, MaxDeadlineNs uint64
}

// DefaultConfig is Config with the ABI file's defaults.
func DefaultConfig(arenaID, created uint64) Config {
	return Config{ArenaID: arenaID, CreatedMonoNs: created, HbIntervalNs: abi.DefaultHbIntervalNs, HbStaleNs: abi.DefaultHbStaleNs,
		SpinNs: abi.DefaultSpinNs, DefaultDeadlineNs: abi.DefaultDefaultDeadlineNs, MaxDeadlineNs: abi.DefaultMaxDeadlineNs}
}

// Layout is where each region of the data segment starts.
type Layout struct {
	StatusOff, StagingOff, StagingLen uint64
	Rings                             []RingGeom
	Slabs                             []SlabGeom
	DataLen                           uint64
}

func up(v, a uint64) uint64 { return (v + a - 1) / a * a }

// ComputeLayout is the placement rule of abi/scz_abi.toml [layout].
func ComputeLayout(g Geometry) (Layout, error) {
	const page = 4096
	if len(g.Rings) == 0 || len(g.Rings) > abi.MaxRings || len(g.SlabClasses) == 0 || len(g.SlabClasses) > abi.MaxSlabClasses {
		return Layout{}, errors.New("ring or slab class count")
	}
	l := Layout{StagingOff: abi.StatusPageLen, StagingLen: g.StagingLen}
	end := uint64(abi.StatusPageLen) + abi.BeatSlots*g.StagingLen
	for _, c := range g.Rings {
		if c < 2 || c > 1<<20 || c&(c-1) != 0 {
			return Layout{}, errors.New("ring capacity")
		}
		off := up(end, page)
		l.Rings = append(l.Rings, RingGeom{Off: uintptr(off), Capacity: c})
		end = off + abi.RingHeaderLen + c*abi.CellLen
	}
	for _, c := range g.SlabClasses {
		if c.SlotSize == 0 || c.SlotSize%64 != 0 || c.SlotCount == 0 || c.ReqCap%64 != 0 || uint64(c.ReqCap)+uint64(c.RespCap) > uint64(c.SlotSize) {
			return Layout{}, errors.New("slab class")
		}
		table := up(end, page)
		end = table + uint64(c.SlotCount)*abi.SlotRecordLen
		data := up(end, page)
		end = data + uint64(c.SlotCount)*uint64(c.SlotSize)
		l.Slabs = append(l.Slabs, SlabGeom{TableOff: uintptr(table), DataOff: uintptr(data), Class: c})
	}
	l.DataLen = 1
	for l.DataLen < end {
		l.DataLen <<= 1
	}
	return l, nil
}

// CtlLen is the control segment's length: the header plus space for the registry.
const CtlLen = abi.CtlHeaderLen + abi.CtlRegistryLen

// WriteControl writes the control header for l. Only before the segment is shared.
func WriteControl(ctl abi.Seg, l Layout, cfg Config) {
	h := abi.Supervisor(ctl)
	h.SetControlHeaderMagic(0, abi.CtlMagic)
	h.SetControlHeaderAbiMajor(0, abi.AbiMajor)
	h.SetControlHeaderAbiMinor(0, abi.AbiMinor)
	h.SetControlHeaderHeaderLen(0, abi.CtlHeaderLen)
	h.SetControlHeaderDataLen(0, l.DataLen)
	h.SetControlHeaderArenaId(0, cfg.ArenaID)
	h.SetControlHeaderCtlLen(0, uint32(ctl.Len()))
	h.SetControlHeaderPageSize(0, 4096)
	h.SetControlHeaderRingCount(0, uint16(len(l.Rings)))
	h.SetControlHeaderSlabClassCount(0, uint16(len(l.Slabs)))
	h.SetControlHeaderFeatureFlags(0, 0)
	h.SetControlHeaderCreatedMonoNs(0, cfg.CreatedMonoNs)
	h.SetControlHeaderHbIntervalNs(0, cfg.HbIntervalNs)
	h.SetControlHeaderHbStaleNs(0, cfg.HbStaleNs)
	h.SetControlHeaderSpinNs(0, cfg.SpinNs)
	h.SetControlHeaderDefaultDeadlineNs(0, cfg.DefaultDeadlineNs)
	h.SetControlHeaderMaxDeadlineNs(0, cfg.MaxDeadlineNs)
	for i, r := range l.Rings {
		b := uintptr(abi.ControlHeaderRingDirOff + i*abi.ControlHeaderRingDirStride)
		h.SetRingDirEntryRingOff(b, uint64(r.Off))
		h.SetRingDirEntryCapacity(b, uint32(r.Capacity))
		h.SetRingDirEntryKind(b, abi.RingKindMpsc)
		h.SetRingDirEntryCellStride(b, abi.CellLen)
	}
	for i, s := range l.Slabs {
		b := uintptr(abi.ControlHeaderSlabDirOff + i*abi.ControlHeaderSlabDirStride)
		h.SetSlabDirEntryTableOff(b, uint64(s.TableOff))
		h.SetSlabDirEntryDataOff(b, uint64(s.DataOff))
		h.SetSlabDirEntrySlotSize(b, s.Class.SlotSize)
		h.SetSlabDirEntrySlotCount(b, s.Class.SlotCount)
		h.SetSlabDirEntryReqCap(b, s.Class.ReqCap)
		h.SetSlabDirEntryRespCap(b, s.Class.RespCap)
		h.SetSlabDirEntryRecordStride(b, abi.SlotRecordLen)
	}
	m := uintptr(abi.ControlHeaderDataMapOff)
	h.SetDataMapStatusOff(m, l.StatusOff)
	h.SetDataMapStagingOff(m, l.StagingOff)
	h.SetDataMapStagingLen(m, uint32(l.StagingLen))
	h.SetControlHeaderLayoutCrc32c(0, LayoutCRC(ctl))
}

// WriteData initializes a fresh, zeroed data segment. Only before it is shared.
func WriteData(data abi.Seg, l Layout, cfg Config) {
	sv := abi.Supervisor(data)
	sv.SetStatusPageMagic(uintptr(l.StatusOff), abi.DataMagic)
	sv.SetStatusPageArenaId(uintptr(l.StatusOff), cfg.ArenaID)
	for i, r := range l.Rings {
		sv.SetRingHeaderRingId(r.Off, uint32(i))
		for p := uint64(0); p < r.Capacity; p++ {
			data.SetCellSeq(r.Cell(p), p)
		}
	}
}

// ── the worker's side ─────────────────────────────────────────────────────────────────

// Dequeue is the consumer side (shm-abi §4.4): head is the consumer's own copy.
func Dequeue(data abi.Seg, r RingGeom, head *uint64) (Descriptor, bool) {
	pos := *head
	cell := r.Cell(pos)
	if data.CellSeq(cell) != pos+1 {
		return Descriptor{}, false
	}
	d := Descriptor{
		RequestID:  data.CellRequestId(cell),
		SlotIndex:  data.CellSlotIndex(cell),
		DeadlineNs: data.CellDeadlineMonoNs(cell),
		SlotEpoch:  data.CellSlotEpoch(cell),
		SlabClass:  data.CellSlabClass(cell),
		EnqueueNs:  data.CellEnqueueMonoNs(cell),
		Flags:      data.CellFlags(cell),
		Route:      data.CellRoute(cell),
	}
	w := abi.Worker(data)
	w.SetCellSeq(cell, pos+r.Capacity) // step A: the cell is free for the next lap
	w.SetRingHeaderHead(r.Off, pos+1)  // step B
	*head = pos + 1
	return d, true
}

// WorkerClaim is READY → PROCESSING at the descriptor's epoch, keeping WAITER and setting
// OWNER; a failure caused only by WAITER changing is retried.
func WorkerClaim(data abi.Seg, rec uintptr, e uint32, owner uint32) bool {
	w := abi.Worker(data)
	for i := 0; i < abi.CasAttempts; i++ {
		v := data.SlotRecordWord(rec)
		if Epoch(v) != e || State(v) != abi.SlotStateReady {
			return false
		}
		if w.CasSlotRecordWord(rec, v, WithOwner(Word(e, abi.SlotStateProcessing)|v&abi.SlotWordWaiterMask, owner)) {
			return true
		}
	}
	return false
}

// Complete is PROCESSING → DONE, waking a parked client, or ABANDONED → FREE if the client
// gave up meanwhile. The response and the record's worker fields must already be written.
// It returns whether the client will see the answer.
func Complete(data abi.Seg, rec uintptr, e uint32) bool {
	w := abi.Worker(data)
	for i := 0; i < abi.CasAttempts; i++ {
		v := data.SlotRecordWord(rec)
		if Epoch(v) != e {
			return false
		}
		switch State(v) {
		case abi.SlotStateAbandoned:
			if w.CasSlotRecordWord(rec, v, Word(e, abi.SlotStateFree)) {
				return false
			}
		case abi.SlotStateProcessing:
			if w.CasSlotRecordWord(rec, v, Word(e, abi.SlotStateDone)) {
				if Waiter(v) {
					data.FutexWakeLow32(rec, 1)
				}
				return true
			}
		default:
			return false
		}
	}
	return false
}
