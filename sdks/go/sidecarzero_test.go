package sidecarzero

import (
	"encoding/binary"
	"reflect"
	"sync"
	"testing"
	"time"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/shm"
)

// The test codec: 8+ request bytes; the first selects what the in-process model does. The
// answer is the request's first word plus one.
type testCodec struct{ fail *int }

const (
	failEncode = 1
	failDecode = 2
	failPanic  = 3
)

func (testCodec) SchemaFingerprint() uint64 { return 0 }
func (testCodec) Size(in []byte) int        { return len(in) }
func (c testCodec) Encode(in []byte, w SlotWriter) error {
	if c.fail != nil && *c.fail == failEncode {
		return ErrShape
	}
	if c.fail != nil && *c.fail == failPanic {
		panic("a codec that panics")
	}
	return w.Write(in)
}
func (c testCodec) Decode(_ []byte, r SlotReader) (uint64, error) {
	if c.fail != nil && *c.fail == failDecode || r.Len() != 8 {
		return 0, ErrShape
	}
	return r.Uint64(0)
}

func serveTest(req, resp []byte) (int, Status) {
	if len(req) < 8 {
		return 0, StatusBadRequest
	}
	switch req[1] {
	case 1:
		return 0, StatusModelError
	case 2:
		return 100000, StatusOK // an out-of-range length: refused
	}
	binary.LittleEndian.PutUint64(resp, binary.LittleEndian.Uint64(req)+1)
	return 8, StatusOK
}

func newTest(t testing.TB, opts ...Option) (*Client[[]byte, uint64], *InProcess, *int) {
	fail := new(int)
	c, ip, err := NewInProcess[[]byte, uint64](testCodec{fail}, func(in []byte, r FallbackReason) uint64 { return 7 }, serveTest, opts...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close(); ip.Close() })
	return c, ip, fail
}

func TestEntryLayout(t *testing.T) {
	for _, v := range []any{entry{}, shard{}, histShard{}, sampleCount{}} {
		if s := reflect.TypeOf(v).Size(); s%64 != 0 {
			t.Errorf("%T is %d bytes, not a whole number of cache lines", v, s)
		}
	}
}

func TestFallbackIsMandatory(t *testing.T) {
	if _, err := New[[]byte, uint64]("/nowhere", testCodec{}, nil); err != ErrNoFallback {
		t.Fatalf("got %v", err)
	}
	if _, _, err := NewInProcess[[]byte, uint64](testCodec{}, nil, serveTest); err != ErrNoFallback {
		t.Fatalf("got %v", err)
	}
}

func TestNotConnectedBeforeTheSidecarExists(t *testing.T) {
	c, err := New[[]byte, uint64](UnixPath("/nonexistent/scz.sock"), testCodec{}, func([]byte, FallbackReason) uint64 { return 5 })
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	o := c.Infer(make([]byte, 8), time.Millisecond)
	if o.Source != Fallback || o.Reason != NotConnected || o.Value != 5 {
		t.Fatalf("%+v", o)
	}
	if c.Stats().Fallback(NotConnected) != 1 {
		t.Fatal("counted")
	}
}

func TestModelAnswersAndEachFallback(t *testing.T) {
	c, ip, fail := newTest(t)
	req := make([]byte, 16)
	binary.LittleEndian.PutUint64(req, 41)
	if o := c.Infer(req, 20*time.Millisecond); o.Source != Model || o.Value != 42 || o.ModelGen != 1 {
		t.Fatalf("%+v", o)
	}
	check := func(what string, in []byte, want FallbackReason) {
		t.Helper()
		if o := c.Infer(in, 20*time.Millisecond); o.Source != Fallback || o.Reason != want || o.Value != 7 {
			t.Fatalf("%s: %+v", what, o)
		}
	}
	check("too large", make([]byte, 4000), TooLarge)
	*fail = failEncode
	check("encode", req, CodecError)
	*fail = failPanic
	check("a codec panic", req, CodecError)
	*fail = failDecode
	check("decode", req, CodecError)
	*fail = 0
	req[1] = 1
	check("status", req, ModelError)
	req[1] = 2
	check("bad length", req, WorkerFailed)
	if ip.a.data.StatusPageClientRejects(ip.a.dir.StatusOff) != 1 {
		t.Fatal("a refused response is counted in client_rejects")
	}
	req[1] = 0
	ip.SetHealthy(false)
	check("gate", req, GateDead)
	ip.SetHealthy(true)
	if o := c.Infer(req, 20*time.Millisecond); o.Source != Model {
		t.Fatalf("%+v", o)
	}
	s := c.Stats()
	if s.Model != 2 || s.Fallback(CodecError) != 3 || s.Fallback(GateDead) != 1 || s.Fallback(TooLarge) != 1 {
		t.Fatalf("%+v", s)
	}
	census(t, ip)
	if err := c.Close(); err != nil {
		t.Fatal(err)
	}
	check("closed", req, NotConnected)
}

// census fails unless every slot is FREE with WAITER and OWNER clear.
func census(t *testing.T, ip *InProcess) {
	t.Helper()
	d := ip.a.dir
	for i := uint32(0); i < d.Slabs[0].Class.SlotCount; i++ {
		w := ip.a.data.SlotRecordWord(d.SlotAt(0, i).Rec)
		if shm.State(w) != abi.SlotStateFree || shm.Waiter(w) || shm.Owner(w) != 0 {
			t.Fatalf("slot %d: %#x", i, w)
		}
	}
}

func TestConcurrentGoroutinesAndSessions(t *testing.T) {
	c, ip, _ := newTest(t)
	var wg sync.WaitGroup
	for g := 0; g < 32; g++ {
		wg.Add(1)
		go func(g int) {
			defer wg.Done()
			s := c.Session()
			defer s.Close()
			req := make([]byte, 8)
			for i := 0; i < 200; i++ {
				binary.LittleEndian.PutUint64(req, uint64(g*1000+i))
				var o Outcome[uint64]
				if i%2 == 0 {
					o = s.Infer(req, 50*time.Millisecond)
				} else {
					o = c.Infer(req, 50*time.Millisecond)
				}
				if o.Source == Model && o.Value != uint64(g*1000+i)+1 {
					t.Errorf("an answer for another call: %d", o.Value)
				}
			}
		}(g)
	}
	wg.Wait()
	if s := c.Stats(); s.Model+s.Fallback(Deadline)+s.Fallback(NoSlot) != 32*200 {
		t.Fatalf("%+v", s)
	}
	census(t, ip)
}

func TestTheBreakerSheds(t *testing.T) {
	c, ip, _ := newTest(t)
	ip.mode.Store(behaveHoldOdd)
	req := make([]byte, 8)
	req[0] = 1 // held: every call misses its deadline
	r := ip.a.dir.Rings[0]
	for i := 0; i < breakerAfter; i++ {
		if o := c.Infer(req, 200*time.Microsecond); o.Reason != Deadline {
			t.Fatalf("call %d: %+v", i, o)
		}
	}
	tail := ip.a.data.RingHeaderTail(r.Off)
	if o := c.Infer(req, 200*time.Microsecond); o.Reason != Overloaded {
		t.Fatalf("%+v", o)
	}
	if ip.a.data.RingHeaderTail(r.Off) != tail {
		t.Fatal("a shed call published")
	}
	time.Sleep(2 * time.Millisecond)
	req[0] = 0
	if o := c.Infer(req, 20*time.Millisecond); o.Source != Model {
		t.Fatalf("the probe: %+v", o)
	}
	if o := c.Infer(req, 20*time.Millisecond); o.Source != Model {
		t.Fatalf("closed again: %+v", o)
	}
	time.Sleep(10 * time.Millisecond)
	census(t, ip)
}

func TestUnwindReleasesTheSlotAndPublishesVoid(t *testing.T) {
	c, ip, _ := newTest(t)
	req := make([]byte, 8)
	r := ip.a.dir.Rings[0]
	tail := ip.a.data.RingHeaderTail(r.Off)
	shm.UnwindOnce.Store(true)
	func() {
		defer func() {
			if p := recover(); p != shm.UnwindPanic {
				t.Fatalf("recovered %v", p)
			}
		}()
		c.Infer(req, 20*time.Millisecond)
	}()
	if ip.a.data.RingHeaderTail(r.Off) != tail+1 {
		t.Fatal("the unwound call's cell was published")
	}
	census(t, ip)
	if o := c.Infer(req, 20*time.Millisecond); o.Source != Model {
		t.Fatalf("%+v", o)
	}
	if s := c.Stats(); s.Model != 1 {
		t.Fatalf("an unwound call counts nowhere: %+v", s)
	}
}

func TestAFallbackThatPanicsHasNothingHeld(t *testing.T) {
	fail := new(int)
	c, ip, err := NewInProcess[[]byte, uint64](testCodec{fail}, func([]byte, FallbackReason) uint64 { panic("the fallback") }, serveTest)
	if err != nil {
		t.Fatal(err)
	}
	defer ip.Close()
	defer c.Close()
	*fail = failEncode
	func() {
		defer func() { _ = recover() }()
		c.Infer(make([]byte, 8), 20*time.Millisecond)
	}()
	census(t, ip)
	if c.Stats().Fallback(CodecError) != 1 {
		t.Fatal("counted before the fallback ran")
	}
	for i := range c.k.entries {
		if c.k.entries[i].owner.Load() != 0 || c.k.entries[i].inCall.Load() != 0 {
			t.Fatal("an entry left held")
		}
	}
}

func TestParkingWithoutSpinningAndThePollingWait(t *testing.T) {
	c, ip, _ := newTest(t)
	c.SetSpinCeiling(0)
	ip.mode.Store(behaveAfterPark)
	req := make([]byte, 8)
	for i := 0; i < 200; i++ {
		if o := c.Infer(req, 50*time.Millisecond); o.Source != Model {
			t.Fatalf("parked: %+v", o)
		}
	}
	c.k.parkedCap.Store(0) // every caller beyond the cap: polls
	ip.mode.Store(behaveDelay)
	ip.delay.Store(100_000)
	for i := 0; i < 50; i++ {
		if o := c.Infer(req, 50*time.Millisecond); o.Source != Model {
			t.Fatalf("polling: %+v", o)
		}
	}
	if c.k.parked.Load() != 0 {
		t.Fatal("the parked count is back at zero")
	}
}

func TestClockSkewFallsBackUntilRecalibrated(t *testing.T) {
	c, _, _ := newTest(t)
	req := make([]byte, 8)
	c.SkewClock(50_000_000)
	if !c.ClockFault() {
		t.Fatal("the check after a skew finds it")
	}
	if o := c.Infer(req, 20*time.Millisecond); o.Reason != ClockFault {
		t.Fatalf("%+v", o)
	}
	end := time.Now().Add(3 * time.Second)
	for c.ClockFault() && time.Now().Before(end) {
		time.Sleep(10 * time.Millisecond)
	}
	if c.ClockFault() {
		t.Fatal("recalibrated within a few checks")
	}
	if o := c.Infer(req, 20*time.Millisecond); o.Source != Model {
		t.Fatalf("%+v", o)
	}
}

func TestPrebuiltCodecs(t *testing.T) {
	if got := NewDenseF32Score(256).SchemaFingerprint(); got != 0x57078e8d7a2e3733 {
		t.Fatalf("fingerprint %#x", got)
	}
	if got := NewDenseF32Batch(128).SchemaFingerprint(); got != 0x98a14c33dfa1052a {
		t.Fatalf("fingerprint %#x", got)
	}
	d := NewDenseF32Batch(256)
	// Short input (sdk-go §10): refused before anything is written.
	if d.Size(F32Batch{Features: make([]float32, 200)}) >= 0 {
		t.Fatal("a 200-element row for 256 features")
	}
	if d.Size(F32Batch{Features: make([]float32, 512)}) != 2 {
		t.Fatal("two rows")
	}
	if NewDenseF32Score(256).Size(make([]float32, 255)) >= 0 {
		t.Fatal("short")
	}
	// The writer checks lengths against the template.
	tmpl := &shm.Template{Columns: []shm.ColumnLoc{{Column: shm.Column{Name: "features", Elem: abi.ElemTypeF32, ListSize: 4}, ValuesOff: 64, RowBytes: 16}}}
	mem := abi.NewLocal(4096)
	w := SlotWriter{seg: mem.Seg(), off: 0, cap: 2048, tmpl: tmpl, rows: 2}
	if w.WriteF32(0, make([]float32, 7)) != ErrShape || w.WriteF32(0, []float32{1, 2, 3, 4, 5, 6, 7, 8}) != nil {
		t.Fatal("WriteF32 length check")
	}
	r := SlotReader{seg: mem.Seg(), off: 0, len: 2048, tmpl: tmpl, rows: 2}
	out := make([]float32, 8)
	if r.ReadF32(0, out) != nil || out[7] != 8 {
		t.Fatal(out)
	}
	if v, err := r.F32(0, 5); err != nil || v != 6 {
		t.Fatal(v, err)
	}
	if _, err := r.F32(0, 8); err != ErrShape {
		t.Fatal("past the rows")
	}
}

func TestReferenceFallbacks(t *testing.T) {
	x := []float32{0, 0.9, 2}
	if ThresholdFallback(1, 0.8, 0.9, 0.1)(x, Deadline) != 0.9 || ThresholdFallback(7, 0.8, 0.9, 0.1)(x, Deadline) != 0.1 {
		t.Fatal("threshold")
	}
	if LinearFallback([]float32{1, 2, 3, 4}, 0.5)(x, Deadline) != 0.5+1.8+6 {
		t.Fatal("linear")
	}
	if ConstantFallback[[]float32, float32](3)(nil, NoSlot) != 3 {
		t.Fatal("constant")
	}
	if d := MeasureFallback(LinearFallback(make([]float32, 256), 0), make([]float32, 256), 10000); d <= 0 {
		t.Fatal(d)
	}
}
