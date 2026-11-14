package shm

import (
	"errors"
	"hash/crc32"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
)

// SlabClass is one slot class's geometry.
type SlabClass struct {
	SlotSize, SlotCount, ReqCap, RespCap uint32
}

// RingGeom is one ring: its header's offset in the data segment and its capacity.
type RingGeom struct {
	Off      uintptr
	Capacity uint64
}

// SlabGeom is one slot class: where its slot table and its slot data start.
type SlabGeom struct {
	TableOff, DataOff uintptr
	Class             SlabClass
}

// Directory is everything a client needs to find its way around, read once from the
// read-only control segment and checked. Nothing in the data segment describes its own
// layout (shm-abi §1): request threads use these local copies only.
type Directory struct {
	ArenaID           uint64
	DataLen           uint64
	StatusOff         uintptr
	StagingOff        uintptr
	StagingLen        uint64
	Rings             []RingGeom
	Slabs             []SlabGeom
	HbIntervalNs      uint64
	HbStaleNs         uint64
	SpinNs            uint64
	DefaultDeadlineNs uint64
	MaxDeadlineNs     uint64
}

// Slot is one slot: its record in the slot table and its data region, split into the
// request region (ReqCap bytes) and the response region after it.
type Slot struct {
	Rec     uintptr
	Data    uintptr
	ReqCap  uintptr
	RespCap uintptr
}

// RespOff is the start of the response region.
func (s Slot) RespOff() uintptr { return s.Data + s.ReqCap }

// SlotAt is slot idx of class, which the caller has bounds-checked.
func (d *Directory) SlotAt(class int, idx uint32) Slot {
	g := &d.Slabs[class]
	return Slot{
		Rec:     g.TableOff + uintptr(idx)*abi.SlotRecordLen,
		Data:    g.DataOff + uintptr(idx)*uintptr(g.Class.SlotSize),
		ReqCap:  uintptr(g.Class.ReqCap),
		RespCap: uintptr(g.Class.RespCap),
	}
}

var castagnoli = crc32.MakeTable(crc32.Castagnoli)

// LayoutCRC is the CRC-32C over the control header's IDENT, CONFIG, and DIRECTORY, with
// the checksum field read as zero. The header is const once shared, so it is copied out.
func LayoutCRC(ctl abi.Seg) uint32 {
	var h [abi.CtlHeaderLen]byte
	ctl.CopyOut(0, h[:])
	for i := 0; i < 4; i++ {
		h[abi.ControlHeaderLayoutCrc32c+i] = 0
	}
	crc := uint32(0)
	for _, r := range abi.ControlHeaderCrcRanges {
		crc = crc32.Update(crc, castagnoli, h[r[0]:r[1]])
	}
	return crc
}

// Errors from ReadDirectory.
var (
	ErrMagic     = errors.New("sidecarzero: the control segment has the wrong magic")
	ErrAbi       = errors.New("sidecarzero: the control segment speaks another ABI")
	ErrChecksum  = errors.New("sidecarzero: the control header's layout checksum is wrong")
	ErrSize      = errors.New("sidecarzero: a segment's size differs from its header")
	ErrDirectory = errors.New("sidecarzero: the control header's directory is inconsistent")
)

// ReadDirectory reads and checks the directory (sdk-contract §6, handshake): magic, ABI
// version, layout checksum, sizes, and every ring and slot class inside the data segment.
func ReadDirectory(ctl abi.Seg, dataLenActual uint64) (*Directory, error) {
	if ctl.Len() < abi.CtlHeaderLen {
		return nil, ErrSize
	}
	if ctl.ControlHeaderMagic(0) != abi.CtlMagic {
		return nil, ErrMagic
	}
	if ctl.ControlHeaderAbiMajor(0) != abi.AbiMajor || ctl.ControlHeaderAbiMinor(0) < abi.AbiMinorMin {
		return nil, ErrAbi
	}
	if ctl.ControlHeaderLayoutCrc32c(0) != LayoutCRC(ctl) {
		return nil, ErrChecksum
	}
	dataLen := ctl.ControlHeaderDataLen(0)
	if dataLen != dataLenActual || uintptr(ctl.ControlHeaderCtlLen(0)) != ctl.Len() {
		return nil, ErrSize
	}
	within := func(off, n uint64) bool { return off+n >= off && off+n <= dataLen && off%64 == 0 }
	nr, nc := int(ctl.ControlHeaderRingCount(0)), int(ctl.ControlHeaderSlabClassCount(0))
	if nr == 0 || nr > abi.MaxRings || nc == 0 || nc > abi.MaxSlabClasses {
		return nil, ErrDirectory
	}
	d := &Directory{ArenaID: ctl.ControlHeaderArenaId(0), DataLen: dataLen}
	for i := 0; i < nr; i++ {
		b := uintptr(abi.ControlHeaderRingDirOff + i*abi.ControlHeaderRingDirStride)
		off, capacity := ctl.RingDirEntryRingOff(b), uint64(ctl.RingDirEntryCapacity(b))
		if ctl.RingDirEntryKind(b) != abi.RingKindMpsc || ctl.RingDirEntryCellStride(b) != abi.CellLen ||
			capacity == 0 || capacity&(capacity-1) != 0 || capacity > 1<<20 || !within(off, abi.RingHeaderLen+capacity*abi.CellLen) {
			return nil, ErrDirectory
		}
		d.Rings = append(d.Rings, RingGeom{Off: uintptr(off), Capacity: capacity})
	}
	for i := 0; i < nc; i++ {
		b := uintptr(abi.ControlHeaderSlabDirOff + i*abi.ControlHeaderSlabDirStride)
		c := SlabClass{SlotSize: ctl.SlabDirEntrySlotSize(b), SlotCount: ctl.SlabDirEntrySlotCount(b), ReqCap: ctl.SlabDirEntryReqCap(b), RespCap: ctl.SlabDirEntryRespCap(b)}
		table, data := ctl.SlabDirEntryTableOff(b), ctl.SlabDirEntryDataOff(b)
		if ctl.SlabDirEntryRecordStride(b) != abi.SlotRecordLen || uint64(c.ReqCap)+uint64(c.RespCap) > uint64(c.SlotSize) ||
			c.SlotCount == 0 || !within(table, uint64(c.SlotCount)*abi.SlotRecordLen) || !within(data, uint64(c.SlotCount)*uint64(c.SlotSize)) {
			return nil, ErrDirectory
		}
		d.Slabs = append(d.Slabs, SlabGeom{TableOff: uintptr(table), DataOff: uintptr(data), Class: c})
	}
	m := uintptr(abi.ControlHeaderDataMapOff)
	status := ctl.DataMapStatusOff(m)
	if !within(status, abi.StatusPageLen) {
		return nil, ErrDirectory
	}
	d.StatusOff = uintptr(status)
	d.StagingOff = uintptr(ctl.DataMapStagingOff(m))
	d.StagingLen = uint64(ctl.DataMapStagingLen(m))
	d.HbIntervalNs = ctl.ControlHeaderHbIntervalNs(0)
	d.HbStaleNs = ctl.ControlHeaderHbStaleNs(0)
	d.SpinNs = ctl.ControlHeaderSpinNs(0)
	d.DefaultDeadlineNs = ctl.ControlHeaderDefaultDeadlineNs(0)
	d.MaxDeadlineNs = ctl.ControlHeaderMaxDeadlineNs(0)
	return d, nil
}
