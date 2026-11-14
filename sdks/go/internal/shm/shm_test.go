package shm

import (
	"bufio"
	"os"
	"strconv"
	"strings"
	"testing"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
)

func TestXXH3Vectors(t *testing.T) {
	f, err := os.Open("testdata/xxh3.txt")
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	data := make([]byte, 4096)
	for i := range data {
		data[i] = byte(i*31 + 7)
	}
	sc := bufio.NewScanner(f)
	n := 0
	for sc.Scan() {
		line := sc.Text()
		if strings.HasPrefix(line, "#") {
			continue
		}
		parts := strings.Fields(line)
		l, _ := strconv.Atoi(parts[0])
		want, _ := strconv.ParseUint(strings.TrimPrefix(parts[1], "0x"), 16, 64)
		if got := XXH3(data[:l]); got != want {
			t.Errorf("len %d: got %#x, want %#x", l, got, want)
		}
		n++
	}
	if n < 1000 {
		t.Fatalf("only %d vectors", n)
	}
}

// The golden schema fingerprints, abi/golden/fingerprints.txt.
func TestFingerprintGoldenVectors(t *testing.T) {
	f, err := os.Open("../../../../abi/golden/fingerprints.txt")
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	sc := bufio.NewScanner(f)
	n := 0
	for sc.Scan() {
		line := sc.Text()
		if strings.HasPrefix(line, "#") || strings.TrimSpace(line) == "" {
			continue
		}
		parts := strings.Fields(line)
		want, err := strconv.ParseUint(strings.TrimPrefix(parts[0], "0x"), 16, 64)
		if err != nil || len(parts) != 3 {
			t.Fatalf("bad line %q", line)
		}
		s := Schema{Request: columns(t, parts[1]), Response: columns(t, parts[2])}
		if got := Fingerprint(s); got != want {
			t.Errorf("%s %s: got %#x, want %#x", parts[1], parts[2], got, want)
		}
		n++
	}
	if n == 0 {
		t.Fatal("no vectors")
	}
}

// columns parses generate.py's notation: name:TYPE[N] with ? for nullable, comma separated, - for none.
func columns(t *testing.T, s string) []Column {
	if s == "-" {
		return nil
	}
	var out []Column
	for _, c := range strings.Split(s, ",") {
		name, rest, _ := strings.Cut(c, ":")
		ty, size, _ := strings.Cut(rest, "[")
		size = strings.TrimSuffix(size, "]")
		nullable := strings.HasSuffix(ty, "?")
		ty = strings.TrimSuffix(ty, "?")
		n, err := strconv.Atoi(size)
		if err != nil {
			t.Fatalf("bad column %q", c)
		}
		var elem uint32
		for v := uint32(0); v < 32; v++ {
			if abi.ElemTypeName(v) == ty {
				elem = v
			}
		}
		if elem == 0 {
			t.Fatalf("unknown type %q", ty)
		}
		out = append(out, Column{Name: name, Elem: uint8(elem), ListSize: uint32(n), Nullable: nullable})
	}
	return out
}

func TestWords(t *testing.T) {
	w := WithWaiter(WithOwner(Word(0xDEADBEEF, abi.SlotStateProcessing), 2))
	if Epoch(w) != 0xDEADBEEF || State(w) != abi.SlotStateProcessing || Owner(w) != 2 || !Waiter(w) {
		t.Fatalf("%#x", w)
	}
	g := Gate(7, 3, abi.GateStateHealthy)
	if GateGen(g) != 7 || GateReason(g) != 3 || !Healthy(g) || Healthy(0) {
		t.Fatalf("%#x", g)
	}
}

// The layout writer reproduces abi/golden/arena.txt word for word: every non-zero 64-bit
// word of the golden arena, and no other.
func TestGoldenArena(t *testing.T) {
	f, err := os.Open("../../../../abi/golden/arena.txt")
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	want := map[string]map[uint64]uint64{"control": {}, "data": {}}
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		p := strings.Fields(sc.Text())
		if len(p) != 3 || strings.HasPrefix(p[0], "#") || p[0] == "segments" {
			continue
		}
		off, _ := strconv.ParseUint(strings.TrimPrefix(p[1], "0x"), 16, 64)
		v, _ := strconv.ParseUint(strings.TrimPrefix(p[2], "0x"), 16, 64)
		want[p[0]][off] = v
	}
	g := Geometry{Rings: []uint64{8}, SlabClasses: []SlabClass{{SlotSize: 4096, SlotCount: 16, ReqCap: 2048, RespCap: 2048}}, StagingLen: 0x10000}
	l, err := ComputeLayout(g)
	if err != nil {
		t.Fatal(err)
	}
	if l.DataLen != 0x40000 {
		t.Fatalf("data_len %#x", l.DataLen)
	}
	ctl, data := abi.NewLocal(CtlLen), abi.NewLocal(uintptr(l.DataLen))
	cfg := DefaultConfig(0x0123456789ABCDEF, 1)
	WriteControl(ctl.Seg(), l, cfg)
	WriteData(data.Seg(), l, cfg)
	for name, seg := range map[string]abi.Seg{"control": ctl.Seg(), "data": data.Seg()} {
		got := map[uint64]uint64{}
		for off := uintptr(0); off < seg.Len(); off += 8 {
			if v := seg.Word64(off); v != 0 {
				got[uint64(off)] = v
			}
		}
		for off, v := range want[name] {
			if got[off] != v {
				t.Errorf("%s %#x: got %#x, want %#x", name, off, got[off], v)
			}
		}
		for off, v := range got {
			if _, ok := want[name][off]; !ok {
				t.Errorf("%s %#x: %#x, which the golden arena does not have", name, off, v)
			}
		}
	}
	// And the client reads its directory back.
	d, err := ReadDirectory(ctl.Seg(), l.DataLen)
	if err != nil {
		t.Fatal(err)
	}
	if d.ArenaID != 0x0123456789ABCDEF || len(d.Rings) != 1 || d.Rings[0].Capacity != 8 || d.Slabs[0].Class.SlotCount != 16 || d.MaxDeadlineNs != abi.DefaultMaxDeadlineNs {
		t.Fatalf("%+v", d)
	}
	// A damaged header is refused.
	abi.Supervisor(ctl.Seg()).SetRingDirEntryCapacity(abi.ControlHeaderRingDirOff, 1<<30)
	if _, err := ReadDirectory(ctl.Seg(), l.DataLen); err != ErrChecksum {
		t.Fatalf("got %v", err)
	}
}

func testArena(t *testing.T, ringCap uint64) (abi.Seg, *Directory) {
	l, err := ComputeLayout(Geometry{Rings: []uint64{ringCap}, SlabClasses: []SlabClass{{SlotSize: 4096, SlotCount: 4, ReqCap: 2048, RespCap: 2048}}, StagingLen: 4096})
	if err != nil {
		t.Fatal(err)
	}
	ctl, data := abi.NewLocal(CtlLen), abi.NewLocal(uintptr(l.DataLen))
	WriteControl(ctl.Seg(), l, DefaultConfig(1, 1))
	WriteData(data.Seg(), l, DefaultConfig(1, 1))
	d, err := ReadDirectory(ctl.Seg(), l.DataLen)
	if err != nil {
		t.Fatal(err)
	}
	return data.Seg(), d
}

func TestRingFifoFullAndVoid(t *testing.T) {
	data, d := testArena(t, 4)
	r := d.Rings[0]
	for i := uint64(0); i < 4; i++ {
		if Enqueue(data, r, &Descriptor{RequestID: i, SlotIndex: uint32(i), SlotEpoch: 3, SlabClass: 0}) != Enqueued {
			t.Fatal(i)
		}
	}
	if Enqueue(data, r, &Descriptor{}) != RingFull {
		t.Fatal("a full ring")
	}
	head := uint64(0)
	for i := uint64(0); i < 4; i++ {
		got, ok := Dequeue(data, r, &head)
		if !ok || got.RequestID != i {
			t.Fatal(i, got)
		}
	}
	// Byte equality with the reference: the cell's eight words.
	x := Descriptor{RequestID: ^uint64(0), SlotIndex: 0xFFFFFFFE, DeadlineNs: 1 << 63, SlotEpoch: 0x80000001, SlabClass: 0xFFFF, EnqueueNs: 42, Flags: abi.CellFlagVoid, Route: 0xABCD}
	cell := r.Cell(4)
	data.SetWord64(cell+0x10, 0x5555) // padding the enqueue must zero
	if Enqueue(data, r, &x) != Enqueued {
		t.Fatal("enqueue")
	}
	words := []uint64{5, x.RequestID, 0, uint64(x.SlotIndex) << 32, x.DeadlineNs, uint64(x.SlotEpoch) | uint64(x.SlabClass)<<32, x.EnqueueNs, uint64(x.Flags) | uint64(x.Route)<<32}
	for i, w := range words {
		if got := data.Word64(cell + uintptr(i*8)); got != w {
			t.Errorf("word %d: %#x, want %#x", i, got, w)
		}
	}
	got, _ := Dequeue(data, r, &head)
	if got != x {
		t.Fatalf("%+v", got)
	}
	// A sequence word ahead of tail: bounded, Exhausted.
	data.SetCellSeq(r.Cell(data.RingHeaderTail(r.Off)), 1_000_000)
	if Enqueue(data, r, &x) != Exhausted {
		t.Fatal("a corrupt sequence word must end in Exhausted")
	}
}

func TestUnwindPublishesAVoidCell(t *testing.T) {
	data, d := testArena(t, 4)
	r := d.Rings[0]
	UnwindOnce.Store(true)
	func() {
		defer func() {
			if p := recover(); p != UnwindPanic {
				t.Fatalf("recovered %v", p)
			}
		}()
		Enqueue(data, r, &Descriptor{RequestID: 9})
		t.Fatal("did not unwind")
	}()
	head := uint64(0)
	got, ok := Dequeue(data, r, &head)
	if !ok || got.Flags&abi.CellFlagVoid == 0 || got.RequestID != 0 {
		t.Fatalf("%v %+v", ok, got)
	}
	if Enqueue(data, r, &Descriptor{RequestID: 10}) != Enqueued {
		t.Fatal("the ring keeps moving")
	}
}

func TestSlotTransitions(t *testing.T) {
	data, d := testArena(t, 4)
	s := d.SlotAt(0, 2)
	e, ok := TryClaim(data, s.Rec)
	if !ok || e != 1 {
		t.Fatal(e, ok)
	}
	if _, ok := TryClaim(data, s.Rec); ok {
		t.Fatal("one owner at a time")
	}
	SetReady(data, s.Rec, e)
	if !WorkerClaim(data, s.Rec, e, 2) {
		t.Fatal("worker claim")
	}
	if AbandonSlot(data, s.Rec, e) != Abandoned || Owner(data.SlotRecordWord(s.Rec)) != 2 {
		t.Fatal("an abandon must carry OWNER across")
	}
	if Complete(data, s.Rec, e) || State(data.SlotRecordWord(s.Rec)) != abi.SlotStateFree {
		t.Fatal("the worker frees an abandoned slot")
	}
	e, _ = TryClaim(data, s.Rec)
	SetReady(data, s.Rec, e)
	if CancelIfReady(data, s.Rec, e) != Cancelled || WorkerClaim(data, s.Rec, e, 1) {
		t.Fatal("a cancelled request is stale for the worker")
	}
	// A WAITER bit flipped under a CAS loop: bounded.
	e, _ = TryClaim(data, s.Rec)
	SetReady(data, s.Rec, e)
	if got := CancelIfReady(data, s.Rec, e+1); got != NotReady {
		t.Fatal(got)
	}
}
