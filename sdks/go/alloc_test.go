package sidecarzero

import (
	"fmt"
	"os"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

// The allocation gate (testing §7; sdk-go §6): the runtime's allocation count must not move
// across a million calls on each path, the sampled timing and the hook of sdk-contract §8
// included. testing.AllocsPerRun rounds to a whole number, so one
// allocation in ten calls would read as zero; this reads runtime.MemStats.Mallocs and
// TotalAlloc before and after instead. The calls run against the in-process double, which is
// the real call path over the real protocol; its worker allocates nothing per request, and
// the monitor goroutine (whose once-a-second file reads allocate) is not started.
//
// SCZ_ALLOC_CALLS sets the calls per path (test.sh: 1,000,000; default 20,000). The polling
// wait runs its n calls on 16 goroutines.

func allocCalls(t *testing.T) int {
	if raceEnabled {
		t.Skip("the race detector allocates")
	}
	if v, err := strconv.Atoi(os.Getenv("SCZ_ALLOC_CALLS")); err == nil && v > 0 {
		return v
	}
	return 20_000
}

func mallocs() (uint64, uint64) {
	var m runtime.MemStats
	runtime.ReadMemStats(&m)
	return m.Mallocs, m.TotalAlloc
}

func gateClient(t *testing.T, opts ...Option) (*Client[[]byte, uint64], *InProcess) {
	opts = append([]Option{WithLogger(func(string, ...any) {})}, opts...)
	c, ip, err := newInProcessClient[[]byte, uint64](testCodec{}, func([]byte, FallbackReason) uint64 { return 7 }, serveTest, false, opts...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close(); ip.Close() })
	return c, ip
}

// gate runs warm-up calls, then n calls on each of g goroutines (started and parked at a
// barrier before the count is read), and fails if anything was allocated. Inside the window
// the test itself does nothing that can allocate: it waits by polling a counter, with a
// sleep whose timer was made before, and the OS threads a parked caller can need were
// started before (the runtime allocates when it starts one).
func gate(t *testing.T, name string, g, n int, call func(i int)) {
	t.Helper()
	prewarmThreads(g + 2*runtime.GOMAXPROCS(0) + 8)
	var ready sync.WaitGroup
	var finished atomic.Int64
	start := make(chan struct{})
	ready.Add(g)
	for k := 0; k < g; k++ {
		go func() {
			for i := 0; i < 64; i++ {
				call(i)
			}
			ready.Done()
			<-start
			for i := 0; i < n; i++ {
				call(i)
			}
			finished.Add(1)
		}()
	}
	ready.Wait()
	warmTimers()
	runtime.GC()
	runtime.GC()
	before := profile()
	// What runs after a collection (the unique package's cleanup, the scavenger) runs now,
	// not inside the window.
	time.Sleep(50 * time.Millisecond)
	surprises.n.Store(0)
	surprises.set.Store(0)
	m0, b0 := mallocs()
	t0 := time.Now()
	close(start)
	for finished.Load() < int64(g) {
		runtime.Gosched()
	}
	el := time.Since(t0)
	m1, b1 := mallocs()
	t.Logf("%-30s %9d calls  %6.2f s  %d allocations, %d bytes", name, g*n, el.Seconds(), m1-m0, b1-b0)
	// Outcomes other than the path's: a few are missed deadlines on a busy machine and say
	// nothing about allocation; more than one call in a thousand means the path itself was
	// not the one measured.
	if k := surprises.n.Load(); k > 0 {
		first := Outcome[uint64]{Source: Source(surprises.src.Load()), Reason: FallbackReason(surprises.reason.Load())}
		t.Logf("%s: %d of %d calls answered otherwise (the first: %v %v)", name, k, g*n, first.Source, first.Reason)
		if k*1000 > int64(g*n) {
			t.Errorf("%s: %d of %d calls did not take the path measured", name, k, g*n)
		}
	}
	if m1 != m0 || b1 != b0 {
		t.Errorf("%s: %d allocations, %d bytes over %d calls", name, m1-m0, b1-b0, g*n)
		if os.Getenv("SCZ_ALLOC_DEBUG") == "1" {
			runtime.GC()
			runtime.GC()
			for stk, n := range profile() {
				if n > before[stk] && !strings.Contains(stk, "sidecar-zero/sdks/go.profile") && !strings.Contains(stk, "testing.(*common)") {
					t.Logf("%d more at:\n%s", n-before[stk], stk)
				}
			}
		}
	}
}

// profile is the memory profile's allocation counts by stack (SCZ_ALLOC_DEBUG=1, with
// runtime.MemProfileRate set to 1 by TestMain).
func profile() map[string]int64 {
	out := map[string]int64{}
	if os.Getenv("SCZ_ALLOC_DEBUG") != "1" {
		return out
	}
	recs := make([]runtime.MemProfileRecord, 100000)
	n, _ := runtime.MemProfile(recs, true)
	for _, r := range recs[:n] {
		var b strings.Builder
		frames := runtime.CallersFrames(r.Stack())
		for {
			f, more := frames.Next()
			fmt.Fprintf(&b, "    %s %s:%d\n", f.Function, f.File, f.Line)
			if !more {
				break
			}
		}
		out[b.String()] += r.AllocObjects
	}
	return out
}

func TestMain(m *testing.M) {
	if os.Getenv("SCZ_ALLOC_DEBUG") == "1" {
		runtime.MemProfileRate = 1
	}
	os.Exit(m.Run())
}

// warmTimers gives every P's timer heap room for many timers: a sleep that lands on a P
// whose heap is full grows it, which is an allocation of the runtime's, not the SDK's.
func warmTimers() {
	var wg sync.WaitGroup
	for i := 0; i < 64*runtime.GOMAXPROCS(0); i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := 0; j < 4; j++ {
				time.Sleep(time.Millisecond)
			}
		}()
	}
	wg.Wait()
}

func prewarmThreads(n int) {
	var ready, done sync.WaitGroup
	release := make(chan struct{})
	ready.Add(n)
	done.Add(n)
	for i := 0; i < n; i++ {
		go func() {
			runtime.LockOSThread()
			ready.Done()
			<-release
			runtime.UnlockOSThread()
			done.Done()
		}()
	}
	ready.Wait()
	close(release)
	done.Wait()
}

// What expect saw that it did not want, kept without allocating and reported by gate after
// its window. Reporting it on the spot (t.Errorf) would allocate inside the window, so a call
// that missed its deadline on a busy host would read as an allocation by the SDK.
var surprises struct {
	n                atomic.Int64
	src, reason, set atomic.Int32
}

func expect(t *testing.T, o Outcome[uint64], src Source, r FallbackReason) {
	if o.Source != src || (src == Fallback && o.Reason != r) {
		if surprises.n.Add(1) == 1 {
			surprises.src.Store(int32(o.Source))
			surprises.reason.Store(int32(o.Reason))
			surprises.set.Store(1)
		}
	}
}

func TestAllocationGate(t *testing.T) {
	n := allocCalls(t)
	req := make([]byte, 8)

	t.Run("model-spinning", func(t *testing.T) {
		c, _ := gateClient(t)
		gate(t, "model answer, spinning", 1, n, func(int) { expect(t, c.Infer(req, 20*time.Millisecond), Model, 0) })
	})
	t.Run("model-parked", func(t *testing.T) {
		c, ip := gateClient(t)
		c.SetSpinCeiling(0)
		ip.mode.Store(behaveAfterPark)
		gate(t, "model answer, after parking", 1, n, func(int) { expect(t, c.Infer(req, 50*time.Millisecond), Model, 0) })
	})
	t.Run("fallback-admission", func(t *testing.T) {
		c, ip := gateClient(t)
		ip.SetHealthy(false)
		gate(t, "fallback at admission", 1, n, func(int) { expect(t, c.Infer(req, 20*time.Millisecond), Fallback, GateDead) })
	})
	t.Run("not-connected", func(t *testing.T) {
		c, _ := gateClient(t)
		c.Close()
		gate(t, "fallback, not connected", 1, n, func(int) { expect(t, c.Infer(req, 20*time.Millisecond), Fallback, NotConnected) })
	})
	t.Run("shed", func(t *testing.T) {
		c, _ := gateClient(t)
		for i := range c.k.entries {
			c.k.entries[i].brkUntil.Store(^uint64(0)) // every caller's breaker open
		}
		gate(t, "shed call", 1, n, func(int) { expect(t, c.Infer(req, 20*time.Millisecond), Fallback, Overloaded) })
	})
	t.Run("abandon", func(t *testing.T) {
		c, ip := gateClient(t)
		ip.mode.Store(behaveHoldOdd)
		held, answered := make([]byte, 8), make([]byte, 8)
		held[0] = 1
		// Three calls abandoned at their deadline, then one answered, so that the breaker
		// never opens.
		gate(t, "abandon at the deadline", 1, n, func(i int) {
			if i%4 == 3 {
				expect(t, c.Infer(answered, 50*time.Millisecond), Model, 0)
			} else {
				expect(t, c.Infer(held, 20*time.Microsecond), Fallback, Deadline)
			}
		})
	})
	t.Run("polling", func(t *testing.T) {
		c, ip := gateClient(t)
		c.SetSpinCeiling(0)
		c.k.parkedCap.Store(0)
		ip.mode.Store(behaveDelay)
		ip.delay.Store(20_000)
		gate(t, "polling wait (beyond the cap)", 16, max(n/16, 20), func(int) {
			expect(t, c.Infer(make8, 50*time.Millisecond), Model, 0)
		})
	})
	// Every call timed, with a hook (sdk-contract §8): the sampled path and the hook's call
	// allocate nothing either. (Every other path times one call in 64.)
	t.Run("model-timed", func(t *testing.T) {
		c, _ := gateClient(t, WithSampleEvery(1))
		gate(t, "model answer, every call timed", 1, n, func(int) { expect(t, c.Infer(req, 20*time.Millisecond), Model, 0) })
		if s := c.Stats(); s.LatencyModel.Count != s.Model || s.Model != uint64(64+n) {
			t.Errorf("every call timed: %d of %d", s.LatencyModel.Count, s.Model)
		}
	})
	t.Run("session-timed", func(t *testing.T) {
		c, _ := gateClient(t, WithSampleEvery(1))
		s := c.Session()
		defer s.Close()
		gate(t, "model answer, Session, every call timed", 1, n, func(int) { expect(t, s.Infer(req, 20*time.Millisecond), Model, 0) })
		if st := c.Stats(); st.LatencyModel.Count != uint64(64+n) {
			t.Errorf("every call timed: %+v", st.LatencyModel)
		}
	})
	t.Run("too-large-timed-hook", func(t *testing.T) {
		var hooked atomic.Uint64
		c, _ := gateClient(t, WithSampleEvery(1), WithOnFallback(func(FallbackReason) { hooked.Add(1) }))
		big := make([]byte, 4000)
		gate(t, "TOO_LARGE, every call timed, hook", 1, n, func(int) { expect(t, c.Infer(big, 20*time.Millisecond), Fallback, TooLarge) })
		if s := c.Stats(); s.FallbackTime.Count != uint64(64+n) || s.LatencyFallback.Count != uint64(64+n) || hooked.Load() != uint64(64+n) {
			t.Errorf("every call timed and hooked: %d, %d, %d", s.FallbackTime.Count, s.LatencyFallback.Count, hooked.Load())
		}
	})
	t.Run("not-connected-timed-hook", func(t *testing.T) {
		var hooked atomic.Uint64
		c, _ := gateClient(t, WithSampleEvery(1), WithOnFallback(func(FallbackReason) { hooked.Add(1) }))
		c.Close()
		gate(t, "not connected, every call timed, hook", 1, n, func(int) { expect(t, c.Infer(req, 20*time.Millisecond), Fallback, NotConnected) })
		if s := c.Stats(); s.FallbackTime.Count != uint64(64+n) || hooked.Load() != uint64(64+n) {
			t.Errorf("every call timed and hooked: %d, %d", s.FallbackTime.Count, hooked.Load())
		}
	})
	t.Run("new-goroutine", func(t *testing.T) {
		// The first call from a goroutine that has never called: a new goroutine per call,
		// measured against the same loop calling nothing, since starting a goroutine
		// allocates its closure.
		c, _ := gateClient(t)
		// The test waits by polling a counter: a blocking wait could allocate the runtime's
		// waiter record, which would be the harness's allocation, not the SDK's.
		var finished atomic.Int64
		run := func(k int, f func()) (uint64, uint64) {
			spawn := func() {
				want := finished.Load() + 1
				go func() { f(); finished.Add(1) }()
				for finished.Load() != want {
					runtime.Gosched()
				}
			}
			for i := 0; i < 64; i++ {
				spawn()
			}
			runtime.GC()
			time.Sleep(50 * time.Millisecond)
			m0, b0 := mallocs()
			for i := 0; i < k; i++ {
				spawn()
			}
			m1, b1 := mallocs()
			return m1 - m0, b1 - b0
		}
		k := max(n/10, 1000)
		bm, bb := run(k, func() {})
		cm, cb := run(k, func() { expect(t, c.Infer(req, 20*time.Millisecond), Model, 0) })
		t.Logf("%-28s %9d calls  %d allocations, %d bytes (a goroutine that calls nothing: %d, %d)", "first call, new goroutine", k, cm, cb, bm, bb)
		if cm > bm || cb > bb {
			t.Errorf("a new goroutine's call: %d allocations, %d bytes; calling nothing: %d, %d", cm, cb, bm, bb)
		}
	})
}

var make8 = make([]byte, 8)
