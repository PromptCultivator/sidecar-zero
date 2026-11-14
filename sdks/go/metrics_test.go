package sidecarzero

import (
	"encoding/binary"
	"fmt"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// sdk-contract §8: the sampled histograms, the export and its text format, the hook. Mirrors
// sdks/rust/tests/metrics.rs, the unit tests of sdks/rust/src/metrics.rs, and the Java SDK's
// MetricsTests.

const absent = Endpoint("/nonexistent/scz.sock")

func every(n int) Option { return WithSampleEvery(n) }

// resetSampling zeroes every stripe's sample count, so that the next goroutine to call starts
// at zero whichever stripe its stack falls in. Only between calls: tests run one at a time.
func resetSampling() {
	for i := range sampleCounts.c {
		sampleCounts.c[i].n.Store(0)
	}
}

// onFreshGoroutine runs f on a goroutine of its own, its stack grown first and then every
// sample count zeroed, and waits for it. A goroutine is known by where its stack is, so a
// stack that moved (grew) in the middle of f would move it to another stripe; grown first,
// it does not.
func onFreshGoroutine(f func()) {
	done := make(chan struct{})
	go func() {
		defer close(done)
		growStack(32)
		resetSampling()
		f()
	}()
	<-done
}

// growStack uses about n KB of stack.
//
//go:noinline
func growStack(n int) byte {
	var pad [1024]byte
	if n > 0 {
		pad[n%1024] = growStack(n - 1)
	}
	return pad[0]
}

// busy spins for more than d of the monotonic clock, so the time is at least d whatever the
// timer does.
func busy(d time.Duration) {
	for t := time.Now(); time.Since(t) <= d; {
	}
}

func newAbsent(t *testing.T, fallback FallbackFunc[[]byte, uint64], opts ...Option) *Client[[]byte, uint64] {
	t.Helper()
	c, err := New[[]byte, uint64](absent, testCodec{}, fallback, opts...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close() })
	return c
}

func seven([]byte, FallbackReason) uint64 { return 7 }

// ── without a sidecar ──────────────────────────────────────────────────────────────────────

func TestADurationLandsInTheFirstBucketAtLeastAsLarge(t *testing.T) {
	var sh [1]histShard
	for _, ns := range []uint64{0, 1_000, 1_001, 100_000_000, 100_000_001, math.MaxUint64 / 4} {
		sh[0].h[hModel].record(ns)
	}
	r := sumHist(sh[:], hModel)
	if r.Cumulative[0] != 2 {
		t.Errorf("0 and exactly 1 µs: %+v", r)
	}
	if r.Cumulative[1] != 3 {
		t.Errorf("1.001 µs: %+v", r)
	}
	if r.Cumulative[15] != 4 {
		t.Errorf("exactly 100 ms: %+v", r)
	}
	if r.Cumulative[16] != 6 || r.Count != 6 {
		t.Errorf("+Inf holds the rest: %+v", r)
	}
	b := BucketsSeconds()
	for i, ns := range boundsNs {
		if b[i] != float64(ns)/1e9 {
			t.Errorf("bound %d: %v s and %d ns", i, b[i], ns)
		}
	}
	if s := sumHist(sh[:], hFallback); s.Count != 0 || s.SumSeconds != 0 {
		t.Errorf("the other histograms are apart: %+v", s)
	}
}

func TestTheTextFormatNamesEachFamilyOnce(t *testing.T) {
	var p PrometheusText
	p.Counter("a_total", []Label{{"source", "fallback"}, {"reason", `x"y`}}, 3)
	p.Counter("a_total", []Label{{"source", "model"}}, 4)
	p.Gauge("b", nil, 1.5)
	var cum [17]uint64
	cum[16] = 2
	b := BucketsSeconds()
	p.Histogram("c_seconds", nil, b[:], cum[:], 0.25, 2)
	s := p.String()
	if strings.Count(s, "# TYPE a_total counter") != 1 {
		t.Errorf("one TYPE line per family:\n%s", s)
	}
	for _, want := range []string{
		"a_total{source=\"fallback\",reason=\"x\\\"y\"} 3\n",
		"a_total{source=\"model\"} 4\n",
		"b 1.5\n",
		"c_seconds_bucket{le=\"0.000001\"} 0\n",
		"c_seconds_bucket{le=\"+Inf\"} 2\n",
		"c_seconds_sum 0.25\nc_seconds_count 2\n",
	} {
		if !strings.Contains(s, want) {
			t.Errorf("missing %q in\n%s", want, s)
		}
	}
	// Every le as Rust's Display writes the f64 bound.
	for _, le := range []string{"0.000001", "0.000002", "0.000005", "0.00001", "0.00002", "0.00005", "0.0001", "0.0002", "0.0005",
		"0.001", "0.002", "0.005", "0.01", "0.02", "0.05", "0.1"} {
		if !strings.Contains(s, "c_seconds_bucket{le=\""+le+"\"} 0\n") {
			t.Errorf("le %s:\n%s", le, s)
		}
	}
	if n := strings.Count(s, "c_seconds_bucket"); n != 17 {
		t.Errorf("%d buckets", n)
	}
	// The escapes, in order; labels with le after them.
	var e PrometheusText
	e.Gauge("g", []Label{{"k", "a\\b\nc\"d"}}, 0)
	e.Histogram("h", []Label{{"source", "model"}}, b[:1], []uint64{1, 1}, 1e-6, 1)
	want := "# TYPE g gauge\ng{k=\"a\\\\b\\nc\\\"d\"} 0\n" +
		"# TYPE h histogram\nh_bucket{source=\"model\",le=\"0.000001\"} 1\nh_bucket{source=\"model\",le=\"+Inf\"} 1\n" +
		"h_sum{source=\"model\"} 0.000001\nh_count{source=\"model\"} 1\n"
	if e.String() != want {
		t.Errorf("got\n%s\nwant\n%s", e.String(), want)
	}
}

func TestNumbersAreWrittenAsRustWritesThem(t *testing.T) {
	for _, c := range []struct {
		v    float64
		want string
	}{
		{0, "0"}, {2, "2"}, {1.5, "1.5"}, {1e-6, "0.000001"}, {123456789, "123456789"}, {1e21, "1000000000000000000000"},
		{0.1, "0.1"}, {0.30000000000000004, "0.30000000000000004"}, {4.5e-7, "0.00000045"}, {0.25, "0.25"},
		{float64(math.MaxUint64), "18446744073709552000"}, {5e-324, "0." + strings.Repeat("0", 323) + "5"},
		// Exactly halfway between two shortest decimals: Rust takes the one further from zero,
		// strconv the even one.
		{751435862906.78125, "751435862906.7813"}, {-751435862906.78125, "-751435862906.7813"}, {2647518583414.28125, "2647518583414.2813"},
		{math.Copysign(0, -1), "-0"}, {math.Inf(1), "inf"}, {math.Inf(-1), "-inf"}, {math.NaN(), "NaN"},
	} {
		if got := formatFloat(c.v); got != c.want {
			t.Errorf("%v: %s, want %s", c.v, got, c.want)
		}
	}
}

// The count is the goroutine's, over every client: two clients called in turn share it.
func TestAGoroutinesSampleCountIsSharedByEveryClientItCalls(t *testing.T) {
	a, b := newAbsent(t, seven, every(4)), newAbsent(t, seven, every(4))
	c := newAbsent(t, seven, every(3))
	x := make([]byte, 8)
	onFreshGoroutine(func() {
		for i := 0; i < 8; i++ {
			if i%2 == 0 {
				a.Infer(x, time.Millisecond)
			} else {
				b.Infer(x, time.Millisecond)
			}
		}
	})
	// The goroutine's calls 0..7: 0 and 4 are timed, both a's.
	if na, nb := a.Stats().LatencyFallback.Count, b.Stats().LatencyFallback.Count; na != 2 || nb != 0 {
		t.Errorf("a %d, b %d timed", na, nb)
	}
	// Not a power of two: calls 0, 3, 6, 9 of 10.
	onFreshGoroutine(func() {
		for i := 0; i < 10; i++ {
			c.Infer(x, time.Millisecond)
		}
	})
	if s := c.Stats(); s.LatencyFallback.Count != 4 || s.FallbackTime.Count != 4 {
		t.Errorf("one in three: %+v, %+v", s.LatencyFallback, s.FallbackTime)
	}
}

// Nothing listens there: every call falls back with NOT_CONNECTED.
func TestTheHookSeesEveryFallbackAndTheFallbackIsTimed(t *testing.T) {
	var seen atomic.Int64
	c := newAbsent(t, func(in []byte, _ FallbackReason) uint64 {
		busy(200 * time.Microsecond)
		return 7
	}, every(1), WithOnFallback(func(r FallbackReason) {
		if r != NotConnected {
			t.Errorf("reason %v", r)
		}
		seen.Add(1)
	}))
	onFreshGoroutine(func() {
		for i := 0; i < 20; i++ {
			if o := c.Infer(make([]byte, 8), time.Millisecond); o.IsModel() || o.Value != 7 {
				t.Errorf("%+v", o)
			}
		}
	})
	if seen.Load() != 20 {
		t.Fatalf("the hook saw %d", seen.Load())
	}
	s := c.Stats()
	if s.Fallback(NotConnected) != 20 || s.FallbackTime.Count != 20 || s.LatencyFallback.Count != 20 {
		t.Fatalf("%+v", s)
	}
	if s.FallbackTime.SumSeconds < 20*200e-6 {
		t.Errorf("%+v", s.FallbackTime)
	}
	if s.LatencyFallback.SumSeconds < s.FallbackTime.SumSeconds {
		t.Errorf("%+v vs %+v", s.LatencyFallback, s.FallbackTime)
	}
	if s.FallbackTime.Cumulative[7] != 0 {
		t.Errorf("nothing at or under 200 µs: %+v", s.FallbackTime)
	}
	if s.LatencyModel.Count != 0 {
		t.Errorf("no model answers: %+v", s.LatencyModel)
	}
}

// What the hook panics with propagates; the fallback does not run, and the call is counted.
func TestAPanicInTheHookPropagates(t *testing.T) {
	var fallbacks atomic.Int64
	boom := fmt.Errorf("from the hook")
	c := newAbsent(t, func([]byte, FallbackReason) uint64 { fallbacks.Add(1); return 0 }, WithOnFallback(func(FallbackReason) { panic(boom) }))
	func() {
		defer func() {
			if p := recover(); p != boom {
				t.Errorf("recovered %v", p)
			}
		}()
		c.Infer(make([]byte, 8), time.Millisecond)
		t.Error("returned")
	}()
	if fallbacks.Load() != 0 || c.Stats().Fallback(NotConnected) != 1 {
		t.Errorf("the fallback ran %d times; %+v", fallbacks.Load(), c.Stats())
	}
}

// A hook that panics on a fallback taken with an entry held (TOO_LARGE): the entry and the
// slot are released, and the panic times nothing.
func TestAPanicInTheHookReleasesWhatTheCallHeld(t *testing.T) {
	boom := fmt.Errorf("from the hook")
	c, ip, _ := newTest(t, WithCallers(2), every(1), WithOnFallback(func(FallbackReason) { panic(boom) }))
	req := make([]byte, 8)
	for i := 0; i < 10; i++ {
		func() {
			defer func() {
				if p := recover(); p != boom {
					t.Errorf("recovered %v", p)
				}
			}()
			c.Infer(make([]byte, 4000), 20*time.Millisecond)
		}()
	}
	// With two entries, a leaked one would show at once as NO_SLOT.
	for i := 0; i < 10; i++ {
		if o := c.Infer(req, 20*time.Millisecond); !o.IsModel() {
			t.Fatalf("after the hook panicked: %+v", o)
		}
	}
	s := c.Stats()
	if s.Fallback(TooLarge) != 10 || s.Model != 10 {
		t.Errorf("%+v", s)
	}
	if s.FallbackTime.Count != 0 || s.LatencyFallback.Count != 0 || s.LatencyModel.Count != 10 {
		t.Errorf("a panicking hook times nothing: %+v %+v %+v", s.FallbackTime, s.LatencyFallback, s.LatencyModel)
	}
	census(t, ip)
	for i := range c.k.entries {
		if c.k.entries[i].owner.Load() != 0 || c.k.entries[i].inCall.Load() != 0 {
			t.Fatal("an entry left held")
		}
	}
}

func TestSamplingCanBeTurnedOff(t *testing.T) {
	c := newAbsent(t, seven, every(0))
	for i := 0; i < 100; i++ {
		c.Infer(make([]byte, 8), time.Millisecond)
	}
	s := c.Stats()
	if s.Fallback(NotConnected) != 100 || s.LatencyFallback.Count != 0 || s.FallbackTime.Count != 0 {
		t.Fatalf("nothing timed: %+v", s)
	}
	text := c.PrometheusText()
	for _, want := range []string{
		"scz_client_calls_total{source=\"fallback\",reason=\"NOT_CONNECTED\"} 100\n",
		"scz_client_fallback_seconds_count 0\n",
		"scz_client_model_gen 0\n",
		"scz_client_reconnects_total 0\n",
	} {
		if !strings.Contains(text, want) {
			t.Errorf("missing %q in\n%s", want, text)
		}
	}
}

// The in-process double: model answers timed through Infer and a Session, and the counters
// (model generation, budget clamped, breaker open).
func TestTheInProcessDoubleTimesModelAnswersAndCounts(t *testing.T) {
	var hooked atomic.Int64
	c, ip, _ := newTest(t, every(1), WithOnFallback(func(FallbackReason) { hooked.Add(1) }))
	req := make([]byte, 8)
	for i := 0; i < 30; i++ {
		if o := c.Infer(req, 20*time.Millisecond); !o.IsModel() {
			t.Fatalf("%+v", o)
		}
	}
	s := c.Session()
	for i := 0; i < 20; i++ {
		if o := s.Infer(req, 20*time.Millisecond); !o.IsModel() {
			t.Fatalf("%+v", o)
		}
	}
	// Above the double's max_deadline_ns (abi default): clamped, and counted.
	if o := s.Infer(req, time.Duration(ip.a.dir.MaxDeadlineNs)+time.Second); !o.IsModel() {
		t.Fatalf("%+v", o)
	}
	s.Close()
	c.Infer(make([]byte, 4000), 20*time.Millisecond) // TOO_LARGE
	st := c.Stats()
	if st.Model != 51 || st.LatencyModel.Count != 51 || st.LatencyFallback.Count != 1 || st.FallbackTime.Count != 1 || hooked.Load() != 1 {
		t.Fatalf("%+v (hooked %d)", st, hooked.Load())
	}
	if st.LatencyModel.SumSeconds <= 0 || st.LatencyModel.Cumulative[16] != 51 {
		t.Errorf("%+v", st.LatencyModel)
	}
	if st.Clamped != 1 || st.ModelGen != 1 || st.BreakerOpen != 0 {
		t.Errorf("clamped %d, model gen %d, breaker open %d", st.Clamped, st.ModelGen, st.BreakerOpen)
	}
	// The breaker of every caller held open: the gauge counts them.
	for i := range c.k.entries[:5] {
		c.k.entries[i].brkUntil.Store(math.MaxUint64)
	}
	if st := c.Stats(); st.BreakerOpen != 5 {
		t.Errorf("breaker open %d", st.BreakerOpen)
	}
	if text := c.PrometheusText(); !strings.Contains(text, "scz_client_breaker_open 5\n") || !strings.Contains(text, "scz_client_state 2\n") {
		t.Errorf("%s", text)
	}
	for i := range c.k.entries[:5] {
		c.k.entries[i].brkUntil.Store(0)
	}
}

// recorder is a sink that notes which call it got, and checks what it was given.
type recorder struct {
	t    *testing.T
	pass strings.Builder
	keep [][]Label
}

func (r *recorder) Counter(name string, labels []Label, value uint64) {
	r.pass.WriteByte('c')
	r.keep = append(r.keep, labels)
}
func (r *recorder) Gauge(name string, labels []Label, value float64) {
	r.pass.WriteByte('g')
	r.keep = append(r.keep, labels)
}
func (r *recorder) Histogram(name string, labels []Label, bounds []float64, cumulative []uint64, sum float64, count uint64) {
	if len(bounds) != 16 || len(cumulative) != 17 || cumulative[16] != count || bounds[15] != 0.1 {
		r.t.Errorf("%s: %v %v %v", name, bounds, cumulative, count)
	}
	bounds[0], cumulative[0] = -1, 99 // the sink's own
	r.pass.WriteByte('h')
	r.keep = append(r.keep, labels)
}

// ── against scz-worker-echo ────────────────────────────────────────────────────────────────

// sidecar starts a supervisor running worker, both from SCZ_BINS (test.sh sets it; otherwise
// the repository's target/release), and returns its endpoint. Off Linux the test is skipped,
// and when SCZ_BINS is unset and nothing is built there; with SCZ_BINS set, a missing binary
// fails the test.
func sidecar(t *testing.T, worker string) Endpoint {
	t.Helper()
	if runtime.GOOS != "linux" {
		t.Skip("a sidecar runs on Linux only")
	}
	bins, set := os.LookupEnv("SCZ_BINS")
	if !set {
		bins = filepath.Join("..", "..", "target", "release")
	}
	sup := filepath.Join(bins, "scz-supervisor")
	for _, b := range []string{sup, filepath.Join(bins, worker)} {
		if _, err := os.Stat(b); err != nil {
			if set {
				t.Fatalf("%s is not built (SCZ_BINS=%s)", b, bins)
			}
			t.Skipf("%s is not built: build the workspace, or set SCZ_BINS", b)
		}
	}
	dir := t.TempDir()
	sock := filepath.Join(dir, "scz.sock")
	log, err := os.Create(filepath.Join(dir, "supervisor.log"))
	if err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(sup, "--socket="+sock, "--worker="+filepath.Join(bins, worker), "--admin="+filepath.Join(dir, "admin.sock"), "--hb-stale-ms=100")
	cmd.Stdout, cmd.Stderr = log, log
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
		log.Close()
		if t.Failed() {
			b, _ := os.ReadFile(log.Name())
			t.Logf("supervisor log:\n%s", b)
		}
	})
	return UnixPath(sock)
}

func TestSampledCallsAreTimedAndExported(t *testing.T) {
	ep := sidecar(t, "scz-worker-echo")
	c, err := New[[]byte, uint64](ep, testCodec{}, seven, every(4))
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	if !c.AwaitBound(10 * time.Second) {
		t.Fatalf("not bound: %v", c.State())
	}
	req := make([]byte, 8)
	binary.LittleEndian.PutUint64(req, 41)
	// On a goroutine of its own, its sample count zeroed first.
	onFreshGoroutine(func() {
		for i := 0; i < 400; i++ {
			if o := c.Infer(req, 90*time.Millisecond); !o.IsModel() || o.Value != 41 {
				t.Errorf("call %d: %+v", i, o)
				return
			}
		}
		// A budget above the sidecar's 100 ms maximum is clamped, and counted.
		if o := c.Infer(req, time.Second); !o.IsModel() {
			t.Errorf("%+v", o)
		}
	})
	if t.Failed() {
		t.FailNow()
	}
	s := c.Stats()
	if s.Model != 401 {
		t.Fatalf("model %d: %+v", s.Model, s)
	}
	if s.LatencyModel.Count != 101 {
		t.Errorf("the first call, then one in four: %+v", s.LatencyModel)
	}
	if s.LatencyModel.SumSeconds <= 0 || s.LatencyModel.Cumulative[16] != 101 {
		t.Errorf("%+v", s.LatencyModel)
	}
	if s.LatencyFallback.Count != 0 {
		t.Errorf("%+v", s.LatencyFallback)
	}
	if s.Clamped != 1 {
		t.Errorf("clamped %d", s.Clamped)
	}
	if s.ModelGen < 1 {
		t.Errorf("model gen %d", s.ModelGen)
	}
	text := c.PrometheusText()
	for _, want := range []string{
		"scz_client_calls_total{source=\"model\"} 401\n",
		"scz_client_calls_total{source=\"fallback\",reason=\"DEADLINE\"} 0\n",
		"scz_client_latency_seconds_count{source=\"model\"} 101\n",
		"scz_client_latency_seconds_bucket{source=\"model\",le=\"+Inf\"} 101\n",
		"scz_client_budget_clamped_total 1\n",
		"scz_client_reconnects_total 0\n",
		"scz_client_state 2\n",
		"# TYPE scz_client_breaker_open gauge\n",
		"# TYPE scz_client_fallback_seconds histogram\n",
	} {
		if !strings.Contains(text, want) {
			t.Errorf("missing %q in\n%s", want, text)
		}
	}
	if n := strings.Count(text, "scz_client_calls_total{"); n != 1+int(numReasons) {
		t.Errorf("every reason, zeros included: %d series", n)
	}
	for _, r := range Reasons() {
		if !strings.Contains(text, "scz_client_calls_total{source=\"fallback\",reason=\""+r.String()+"\"} ") {
			t.Errorf("no series for %v", r)
		}
	}
	// The families in the order every SDK exports them, each with one TYPE line.
	order := []string{"# TYPE scz_client_calls_total counter", "# TYPE scz_client_latency_seconds histogram", "# TYPE scz_client_fallback_seconds histogram",
		"# TYPE scz_client_budget_clamped_total counter", "# TYPE scz_client_breaker_open gauge", "# TYPE scz_client_late_wakes_total counter",
		"# TYPE scz_client_reconnects_total counter", "# TYPE scz_client_state gauge", "# TYPE scz_client_model_gen gauge"}
	at := -1
	for _, o := range order {
		i := strings.Index(text, o+"\n")
		if i <= at {
			t.Errorf("%s out of order:\n%s", o, text)
		}
		at = i
	}
	if n := strings.Count(text, "# TYPE "); n != len(order) {
		t.Errorf("%d TYPE lines:\n%s", n, text)
	}
	// A sink sees the same pass; what it keeps or changes is its own.
	r := &recorder{t: t}
	c.Export(r)
	if want := strings.Repeat("c", 1+int(numReasons)) + "hhhcgccgg"; r.pass.String() != want {
		t.Errorf("the pass: %s, want %s", r.pass.String(), want)
	}
	if l := r.keep[0]; len(l) != 1 || l[0] != (Label{"source", "model"}) {
		t.Errorf("%v", l)
	}
	if l := r.keep[1+int(Deadline)]; len(l) != 2 || l[0] != (Label{"source", "fallback"}) || l[1] != (Label{"reason", "DEADLINE"}) {
		t.Errorf("%v", l)
	}
	if b := BucketsSeconds(); b[0] != 1e-6 || !strings.Contains(c.PrometheusText(), "le=\"0.000001\"") {
		t.Error("a sink's changes reached the client")
	}
}
