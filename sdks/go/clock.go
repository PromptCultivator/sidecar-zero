package sidecarzero

import (
	"sync"
	"sync/atomic"
	"time"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/sys"
)

// clock is the SDK's cheap CLOCK_MONOTONIC (sdk-go §4): offset + time.Since(base). A
// time.Time from time.Now carries a monotonic reading, and time.Since on it reads only the
// monotonic clock, through the vDSO; unix.ClockGettime would be a real system call, hundreds
// of nanoseconds, too slow for admission. The offset is measured against a real
// clock_gettime(CLOCK_MONOTONIC) at start and re-verified four times a second by the monitor
// goroutine. From the check that finds drift beyond the tolerance, calls fall back with
// CLOCK_FAULT until a later check finds the re-established offset holding.
type clock struct {
	base time.Time
	// adj is CLOCK_MONOTONIC at base, in nanoseconds, plus any skew the test hook added.
	adj   atomic.Int64
	fault atomic.Bool
	mu    sync.Mutex // serializes checks
	logf  func(string, ...any)
}

const (
	// clockTolerance is the drift the check accepts, beyond half its measuring window.
	clockTolerance = 50_000
	// clockWindow: a measurement whose two reference reads are further apart than this
	// (a preempted thread) proves nothing either way, and is not acted on.
	clockWindow     = 200_000
	clockCheckEvery = 250 * time.Millisecond
)

func newClock(logf func(string, ...any)) *clock {
	c := &clock{base: time.Now(), logf: logf}
	for i := 0; i < 10; i++ {
		if off, _, win := c.measure(); win <= clockWindow || i == 9 {
			c.adj.Store(off)
			break
		}
	}
	return c
}

// now is CLOCK_MONOTONIC nanoseconds, as cheaply as Go can read them.
func (c *clock) now() uint64 { return uint64(c.adj.Load() + int64(time.Since(c.base))) }

// elapsed is the time since the clock was made, in nanoseconds: the same one monotonic read
// as now, without the offset, which the checks may move (and the CLOCK_SKEW hook does). The
// sampled histograms time calls with it (sdk-contract §8).
func (c *clock) elapsed() int64 { return int64(time.Since(c.base)) }

// measure reads the reference clock and the monotonic clock behind time.Since back to back
// several times and keeps the tightest pair: the offset it implies, the current clock's drift
// from the reference, and the measuring window.
func (c *clock) measure() (offset, drift, window int64) {
	window = 1 << 62
	adj := c.adj.Load()
	for i := 0; i < 7; i++ {
		r1 := int64(sys.MonotonicNs())
		since := int64(time.Since(c.base))
		r2 := int64(sys.MonotonicNs())
		if w := r2 - r1; w < window {
			mid := r1 + w/2
			window, offset, drift = w, mid-since, adj+since-mid
		}
	}
	return offset, drift, window
}

// check is one verification (the monitor's, or the test hook's right after it skews).
func (c *clock) check() {
	c.mu.Lock()
	defer c.mu.Unlock()
	off, drift, win := c.measure()
	if win > clockWindow {
		return
	}
	if drift < 0 {
		drift = -drift
	}
	if drift > clockTolerance+win/2 {
		if !c.fault.Load() {
			c.logf("sidecarzero: the cheap clock is %d ns away from CLOCK_MONOTONIC; calls fall back with CLOCK_FAULT until it is re-established", drift)
			c.fault.Store(true)
		}
		c.adj.Store(off)
		return
	}
	if c.fault.Load() {
		c.fault.Store(false)
		c.logf("sidecarzero: the cheap clock agrees with CLOCK_MONOTONIC again")
	}
}

// skew shifts the cheap clock by ns, as a clock anomaly would, and runs the check that finds
// it (the CLOCK_SKEW test hook of the conformance agent).
func (c *clock) skew(ns int64) {
	c.mu.Lock()
	c.adj.Add(ns)
	c.mu.Unlock()
	c.check()
}
