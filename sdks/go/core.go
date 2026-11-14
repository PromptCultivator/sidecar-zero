package sidecarzero

import (
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/shm"
)

// ClientState is the connection state of sdk-contract §6.
type ClientState uint32

const (
	Disconnected ClientState = iota
	Connecting
	Bound
	// Rejected: the sidecar does not speak this SDK's ABI; calls fall back with ABI_MISMATCH.
	Rejected
	// SchemaRejected: the model's schema is not the codec's; calls fall back with SCHEMA_MISMATCH.
	SchemaRejected
)

func (s ClientState) String() string {
	return [...]string{"DISCONNECTED", "CONNECTING", "BOUND", "REJECTED", "SCHEMA_REJECTED"}[s]
}

// How a snapshot's binding meets the codec (arrow-data-plane §9).
const (
	modeRaw      uint8 = iota // an empty registry and a codec without columns: opaque bytes
	modeFrames                // the registry's schema is the codec's
	modeMismatch              // they differ: SCHEMA_MISMATCH
	modeUnbound               // the registry could not be read or parsed; asked again
)

// arena is one mapped arena (or, in the in-process double, ordinary memory standing in for
// one), with the session socket that came with it. Only the background goroutine changes it.
type arena struct {
	ctl, data       abi.Seg
	ctlMap, dataMap *abi.Mapping
	dir             *shm.Directory
	conn            io.Closer
	sessionID       uint64
	snaps           int // snapshots of this arena not yet reclaimed
}

// snapshot is what request goroutines see: one arena and the binding to its registry, loaded
// once per call with one atomic load (sdk-contract §4). A rebind publishes a new snapshot of
// the same arena.
type snapshot struct {
	id    uint32
	a     *arena
	mode  uint8
	reg   *shm.Registry
	epoch uint32
	// confirmed is the gate generation the binding was last confirmed under; request
	// goroutines move it on when a new generation shows the same registry epoch.
	confirmed atomic.Uint32
}

// entry is one caller-state entry (sdk-contract §7; sdk-go §6, "Caller state"): goroutines are
// not OS threads, so per-caller state lives in a fixed table of entries that a call claims
// for its length (or a Session holds). Fields above the counters are written and read only
// by the holder; the owner word's compare-and-swap orders one holder's writes before the
// next's reads. The counters are atomics because Stats sums them while calls run.
type entry struct {
	owner atomic.Uint32 // 0 free, 1 a call, 2 a Session
	_     uint32
	// inCall is the in-call marker: snapshot id << 32, thread id 0 (a goroutine is not tied
	// to one thread). The background goroutine unmaps an old arena only once no marker names
	// one of its snapshots.
	inCall atomic.Uint64

	cachedSlot  [abi.MaxSlabClasses]uint32
	ringCursor  uint32
	reqLen      uint32 // the opaque-bytes writer's high-water mark
	nextRequest uint64
	ewma        uint64 // service time, ns: the spin budget follows it
	budget      uint64 // the current call's budget, clamped
	brkMisses   uint32
	_           uint32
	// brkUntil: the breaker sheds until then; 0: closed. Written only by the holder, but an
	// atomic, because Stats counts the callers shedding now (scz_client_breaker_open).
	brkUntil    atomic.Uint64
	brkInterval uint64

	lastCall  atomic.Uint64
	model     atomic.Uint64
	lateWakes atomic.Uint64
	// clamped counts calls whose budget was above the sidecar's max_deadline_ns; modelGen is
	// the last model generation that answered this caller (sdk-contract §8).
	clamped   atomic.Uint64
	modelGen  atomic.Uint64
	fallbacks [numReasons]atomic.Uint64
	_         [8]byte // to a whole number of cache lines (TestEntryLayout)
}

func bump(c *atomic.Uint64) { c.Store(c.Load() + 1) }

// shard counts fallbacks taken with no entry to count them in (no snapshot, or no entry
// free), sharded by stack hint so goroutines rarely share a line.
type shard struct {
	c [numReasons]atomic.Uint64
	_ [24]byte
}

const shards = 64

// entryProbe is how many entries a call tries before falling back with NO_SLOT (sdk-go §6:
// "probing a few neighbours").
const entryProbe = 8

const (
	breakerAfter = 8           // consecutive DEADLINE or RING_FULL outcomes that open it
	breakerMaxNs = 100_000_000 // its longest interval
)

const reasonNone FallbackReason = 255

// core is what every Client shares with its background goroutines, whatever its types.
type core struct {
	opts     options
	endpoint string
	// want is the codec's schema fingerprint; 0 for opaque bytes.
	want uint64

	current    atomic.Pointer[snapshot]
	entries    []entry
	unattached []shard
	state      atomic.Uint32
	stop       atomic.Bool
	stopCh     chan struct{}
	done       chan struct{}
	monDone    chan struct{}
	rebind     atomic.Bool
	sessions   atomic.Uint64
	retired    atomic.Uint64
	clk        *clock

	// The spin ceiling (sdk-contract §7): spinFixed is the application's (-1: none), and
	// spinScaled the monitor's, lowered when calling goroutines outnumber the CPUs available
	// or the host container is throttled. Request goroutines only read them.
	spinFixed  atomic.Int64
	spinScaled atomic.Int64
	// parked counts callers blocked in futex, each holding an OS thread; beyond parkedCap a
	// caller polls instead (sdk-go §5, "Parked callers are capped").
	parked    atomic.Int32
	parkedCap atomic.Int32

	// Sampling (sdk-contract §8): one call in sampleEvery is timed (0: none), counted with a
	// mask when it is a power of two; and the sampled histograms, in shards chosen by the
	// caller's stack hint. Set by newCore, read-only after.
	sampleEvery uint32
	samplePow2  bool
	hists       []histShard

	nextID   uint32 // background only
	closeErr error
	onClose  func() // the in-process double's part of Close
	once     sync.Once
}

type options struct {
	fallbackReserve time.Duration
	connectTimeout  time.Duration
	callers         int
	spinCeiling     time.Duration
	parkedCap       int
	sampleEvery     int
	onFallback      func(FallbackReason)
	logf            func(string, ...any)
}

// Option configures New.
type Option func(*options)

// WithFallbackReserve stops waiting that long before the deadline, to leave the fallback its
// time (sdk-contract §1).
func WithFallbackReserve(d time.Duration) Option { return func(o *options) { o.fallbackReserve = d } }

// WithConnectTimeout is applied to the connect and to each handshake message (default 1 s).
func WithConnectTimeout(d time.Duration) Option { return func(o *options) { o.connectTimeout = d } }

// WithCallers sets the number of caller-state entries: at most that many calls can be in
// flight at once (default 1,024).
func WithCallers(n int) Option { return func(o *options) { o.callers = n } }

// WithSpinCeiling fixes the spin before a caller parks, below the sidecar's spin_ns; zero
// parks at once.
func WithSpinCeiling(d time.Duration) Option { return func(o *options) { o.spinCeiling = d } }

// WithParkedCap sets how many callers may block an OS thread in futex at once; the rest poll
// (default four times the CPUs available, at least 16).
func WithParkedCap(n int) Option { return func(o *options) { o.parkedCap = n } }

// WithLogger sends the SDK's few log lines (an ABI mismatch, a clock fault) somewhere other
// than standard error.
func WithLogger(f func(format string, args ...any)) Option { return func(o *options) { o.logf = f } }

// WithSampleEvery times one call in n on each calling goroutine for the latency histograms
// (sdk-contract §8; default DefaultSampleEvery, 64). Zero, or a negative n, times none.
func WithSampleEvery(n int) Option { return func(o *options) { o.sampleEvery = n } }

// WithOnFallback sets a hook called with the reason once per fallback, on the calling
// goroutine, just before the fallback function and after the slot and the caller entry have
// been released (sdk-contract §8): for sampled logging. Off by default. It is not on the
// zero-allocation path: whatever it does costs every fallback. A panic in it propagates as
// the fallback's would, and the fallback then does not run.
func WithOnFallback(hook func(FallbackReason)) Option {
	return func(o *options) { o.onFallback = hook }
}

func newCore(endpoint string, want uint64, opts []Option) *core {
	o := options{connectTimeout: time.Second, callers: 1024, spinCeiling: -1, sampleEvery: DefaultSampleEvery}
	for _, f := range opts {
		f(&o)
	}
	if o.callers <= 0 {
		o.callers = 1024
	}
	every := uint64(max(o.sampleEvery, 0))
	every = min(every, math.MaxUint32)
	if o.logf == nil {
		o.logf = func(f string, a ...any) { logStderr(f, a...) }
	}
	k := &core{
		opts:        o,
		endpoint:    endpoint,
		want:        want,
		entries:     make([]entry, o.callers),
		unattached:  make([]shard, shards),
		stopCh:      make(chan struct{}),
		done:        make(chan struct{}),
		monDone:     make(chan struct{}),
		clk:         newClock(o.logf),
		sampleEvery: uint32(every),
		samplePow2:  every&(every-1) == 0,
		hists:       make([]histShard, histShards),
	}
	// Each entry's first probe starts at a hash of the entry, so that callers spread over
	// the slot table (shm-abi §5.3: "starting at a hash of its thread id").
	for i := range k.entries {
		for c := range k.entries[i].cachedSlot {
			k.entries[i].cachedSlot[c] = uint32(mix(uint64(i)<<3 | uint64(c)))
		}
	}
	k.spinFixed.Store(-1)
	if o.spinCeiling >= 0 {
		k.spinFixed.Store(int64(o.spinCeiling))
	}
	k.spinScaled.Store(abi.DefaultSpinNs)
	cap := o.parkedCap
	if cap <= 0 {
		cap = 4 * cpusAvailable()
		if cap < 16 {
			cap = 16
		}
	}
	k.parkedCap.Store(int32(cap))
	return k
}

func logStderr(f string, a ...any) {
	s := f
	if len(a) > 0 {
		s = fmt.Sprintf(f, a...)
	}
	os.Stderr.WriteString(s + "\n")
}

func (k *core) setState(s ClientState) { k.state.Store(uint32(s)) }

func (k *core) unattachedReason() FallbackReason {
	if ClientState(k.state.Load()) == Rejected {
		return AbiMismatch
	}
	return NotConnected
}

func hash(h uintptr) uint64 { return uint64(h>>12) * 0x9E3779B97F4A7C15 }

func (k *core) countUnattached(hint uintptr, r FallbackReason) {
	k.unattached[(hash(hint)>>32)%shards].c[r].Add(1)
}

// claim takes a free entry, starting at an index derived from the caller's stack address.
func (k *core) claim(hint uintptr) *entry {
	n := uint64(len(k.entries))
	start := (hash(hint) >> 32) % n
	for i := uint64(0); i < entryProbe && i < n; i++ {
		e := &k.entries[(start+i)%n]
		if e.owner.Load() == 0 && e.owner.CompareAndSwap(0, 1) {
			return e
		}
	}
	return nil
}

// enter marks e as in a call inside the current snapshot, and re-checks that the snapshot is
// still current: either the call sees a replacement and backs out, or the background
// goroutine's scan sees the marker and waits (both write, then read; Go's atomics are
// sequentially consistent). Nil: no snapshot, or it kept changing.
func (k *core) enter(e *entry, snap *snapshot) *snapshot {
	for i := 0; i < 4 && snap != nil; i++ {
		e.inCall.Store(uint64(snap.id) << 32)
		if cur := k.current.Load(); cur == snap {
			return snap
		} else {
			e.inCall.Store(0)
			snap = cur
		}
	}
	return nil
}

// inUse reports whether any marker names snapshot id.
func (k *core) inUse(id uint32) bool {
	for i := range k.entries {
		if uint32(k.entries[i].inCall.Load()>>32) == id {
			return true
		}
	}
	return false
}

// confirm is the admission check of arena-data-plane §9: when the gate shows a generation
// the binding was not confirmed under, the registry epoch is read under the seqlock.
// Unchanged: confirmed. Changed: REBINDING, and the background goroutine rebinds.
func (k *core) confirm(s *snapshot, gen uint32) bool {
	if s.confirmed.Load() == gen {
		return true
	}
	if e, ok := shm.ModelEpoch(s.a.ctl, 64); ok && e == s.epoch && s.mode != modeUnbound {
		s.confirmed.Store(gen)
		return true
	}
	k.rebind.Store(true)
	return false
}

// bind reads the registry under the model seqlock and binds the codec to it (arrow-data-plane
// §9, strict binding). The gate generation is read first: the supervisor writes the model
// block before the gate shows a new generation, so the block read after it is at least as new.
func (k *core) bind(a *arena) *snapshot {
	s := &snapshot{a: a, mode: modeUnbound}
	gen := shm.GateGen(a.ctl.ControlHeaderGate(0))
	s.confirmed.Store(gen)
	block, bytes, ok := shm.ReadModel(a.ctl, 1000)
	if !ok {
		return s
	}
	s.epoch = block.RegistryEpoch
	caps := make([]shm.Caps, len(a.dir.Slabs))
	for i, g := range a.dir.Slabs {
		caps[i] = shm.Caps{Req: g.Class.ReqCap, Resp: g.Class.RespCap}
	}
	reg, err := shm.ParseRegistry(bytes, caps)
	switch {
	case err == nil && reg == nil && k.want == 0:
		s.mode = modeRaw
	case err == nil && reg != nil && k.want != 0 && reg.Fingerprint() == k.want && block.SchemaFingerprint == k.want:
		s.mode, s.reg = modeFrames, reg
	case err != nil, block.RegistryEpoch == 0:
		s.mode = modeUnbound
	default:
		s.mode = modeMismatch
	}
	return s
}

// publish binds a and makes it current, returning the snapshot it replaced. Background only.
func (k *core) publish(a *arena) (*snapshot, *snapshot) {
	s := k.bind(a)
	k.nextID++
	if k.nextID == 0 {
		k.nextID = 1
	}
	s.id = k.nextID
	a.snaps++
	old := k.current.Swap(s)
	if s.mode == modeMismatch {
		k.setState(SchemaRejected)
	} else {
		k.setState(Bound)
	}
	return s, old
}

// retire waits until no call is inside snapshot s, then drops it, unmapping its arena and
// closing the arena's socket if it was the arena's last snapshot (sdk-contract §6). Request
// goroutines never unmap. False: still in use after 5 s while closing, and left mapped.
func (k *core) retire(s *snapshot) bool {
	if s == nil {
		return true
	}
	t0 := time.Now()
	for k.inUse(s.id) {
		if k.stop.Load() && time.Since(t0) > 5*time.Second {
			k.opts.logf("sidecarzero: an arena is still in use after 5 s; leaving it mapped")
			return false
		}
		time.Sleep(time.Millisecond)
	}
	a := s.a
	a.snaps--
	if a.snaps == 0 {
		if a.ctlMap != nil {
			_ = a.ctlMap.Unmap()
		}
		if a.dataMap != nil {
			_ = a.dataMap.Unmap()
		}
		// Last: its closing tells the supervisor nothing maps the arena any more.
		if a.conn != nil {
			_ = a.conn.Close()
		}
		k.retired.Add(1)
	}
	return true
}

// nap sleeps for d, or until the client closes.
func (k *core) nap(d time.Duration) {
	t := time.NewTimer(d)
	select {
	case <-t.C:
	case <-k.stopCh:
		t.Stop()
	}
}

// ── the monitor: clock checks, the spin ceiling ───────────────────────────────────────

func (k *core) monitor() {
	defer close(k.monDone)
	t := time.NewTicker(clockCheckEvery)
	defer t.Stop()
	var throttled uint64
	shift := uint(0)
	for n := 0; ; n++ {
		select {
		case <-k.stopCh:
			return
		case <-t.C:
		}
		k.clk.check()
		if n%4 != 3 {
			continue
		}
		// Once a second: the spin ceiling (signaling §4). Scaled down in proportion when the
		// goroutines that called in the last second outnumber the CPUs available, and halved
		// each time this container's nr_throttled advanced, restored step by step after.
		cpus := cpusAvailable()
		now := k.clk.now()
		active := 0
		for i := range k.entries {
			if lc := k.entries[i].lastCall.Load(); lc != 0 && lc+1e9 > now {
				active++
			}
		}
		base := int64(abi.DefaultSpinNs)
		if s := k.current.Load(); s != nil {
			base = int64(s.a.dir.SpinNs)
		}
		if active > cpus {
			base = base * int64(cpus) / int64(active)
		}
		if nt, ok := readThrottled(); ok {
			if nt > throttled && throttled != 0 && shift < 8 {
				shift++
			} else if nt == throttled && shift > 0 {
				shift--
			}
			throttled = nt
		}
		k.spinScaled.Store(base >> shift)
	}
}

// cpusAvailable is the smaller of GOMAXPROCS and the container's CPU quota (sdk-go §5: Go
// before 1.25 sizes GOMAXPROCS from the node's cores and ignores the quota).
func cpusAvailable() int {
	n := runtime.GOMAXPROCS(0)
	if b, err := os.ReadFile("/sys/fs/cgroup/cpu.max"); err == nil {
		f := strings.Fields(string(b))
		if len(f) == 2 && f[0] != "max" {
			q, e1 := strconv.ParseFloat(f[0], 64)
			p, e2 := strconv.ParseFloat(f[1], 64)
			if e1 == nil && e2 == nil && p > 0 {
				c := int(q/p + 0.999)
				if c < 1 {
					c = 1
				}
				if c < n {
					n = c
				}
			}
		}
	}
	return n
}

func readThrottled() (uint64, bool) {
	b, err := os.ReadFile("/sys/fs/cgroup/cpu.stat")
	if err != nil {
		return 0, false
	}
	for _, l := range strings.Split(string(b), "\n") {
		if v, ok := strings.CutPrefix(l, "nr_throttled "); ok {
			n, err := strconv.ParseUint(strings.TrimSpace(v), 10, 64)
			return n, err == nil
		}
	}
	return 0, false
}

var errStillInUse = errors.New("sidecarzero: a call was still inside the arena after 5 s; the arena and its socket are left in place")
