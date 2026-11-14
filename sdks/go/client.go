package sidecarzero

import (
	"errors"
	"math"
	"runtime"
	"time"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/shm"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/sys"
)

// Endpoint is where the session socket lives: a path, or "@name" for a name in the abstract
// namespace.
type Endpoint string

// UnixPath is the endpoint of a socket file, for example in a shared emptyDir.
func UnixPath(p string) Endpoint { return Endpoint(p) }

// AbstractName is the endpoint of a socket in the abstract namespace.
func AbstractName(n string) Endpoint { return Endpoint("@" + n) }

// Client calls the sidecar's model through shared memory, and falls back to the caller's
// own function whenever the model cannot answer in time (sdk-contract §1). It is safe for
// concurrent use by any number of goroutines.
type Client[In, Out any] struct {
	k        *core
	codec    Codec[In, Out]
	fallback FallbackFunc[In, Out]
	reserve  uint64
}

// ErrNoFallback is New's error for a nil fallback: no client exists without one.
var ErrNoFallback = errors.New("sidecarzero: New needs a fallback function, and it was nil")

// ErrNoCodec is New's error for a nil codec.
var ErrNoCodec = errors.New("sidecarzero: New needs a codec, and it was nil")

// New builds a client. The fallback is a required positional parameter (sdk-contract §3.1):
// leaving it out does not compile, and nil is refused here. New returns at once; a background
// goroutine connects, and calls fall back with NOT_CONNECTED until it has (sdk-contract §6,
// "Startup"). Off Linux the client never connects and every call falls back.
func New[In, Out any](endpoint Endpoint, codec Codec[In, Out], fallback FallbackFunc[In, Out], opts ...Option) (*Client[In, Out], error) {
	if fallback == nil {
		return nil, ErrNoFallback
	}
	if codec == nil {
		return nil, ErrNoCodec
	}
	k := newCore(string(endpoint), codec.SchemaFingerprint(), opts)
	c := &Client[In, Out]{k: k, codec: codec, fallback: fallback, reserve: uint64(k.opts.fallbackReserve)}
	go k.monitor()
	go k.background()
	return c, nil
}

// State is the client's connection state.
func (c *Client[In, Out]) State() ClientState { return ClientState(c.k.state.Load()) }

// Infer always returns an output: the model's if it answers within budget, otherwise the
// fallback's (sdk-contract §1). Nothing the SDK does makes it fail; only a fallback (or an
// OnFallback hook) that panics does, after the SDK has released everything it held.
func (c *Client[In, Out]) Infer(in In, budget time.Duration) Outcome[Out] {
	return c.infer(in, budget, nil)
}

// infer is Infer on an entry the caller holds (a Session's) or, with held nil, on one claimed
// for the call; with the sampled timing and the hook of sdk-contract §8 around it. An untimed
// call reads no clock here.
func (c *Client[In, Out]) infer(in In, budget time.Duration, held *entry) Outcome[Out] {
	k := c.k
	hint := sys.StackHint()
	t0 := k.sampleStart(hint)
	o, r := c.try(in, budget, held, hint)
	if r == reasonNone {
		if t0 != untimed {
			k.timedModel(hint, t0)
		}
		return o
	}
	// The slot and the caller entry have already been released.
	if h := k.opts.onFallback; h != nil {
		h(r)
	}
	if t0 == untimed {
		return Outcome[Out]{Value: c.fallback(in, r), Source: Fallback, Reason: r}
	}
	t1 := k.clk.elapsed()
	v := c.fallback(in, r)
	k.timedFallback(hint, t0, t1)
	return Outcome[Out]{Value: v, Source: Fallback, Reason: r}
}

// InferUntil is Infer with an absolute deadline, as a time.Time.
func (c *Client[In, Out]) InferUntil(in In, deadline time.Time) Outcome[Out] {
	return c.Infer(in, time.Until(deadline))
}

func budgetNs(d time.Duration) uint64 {
	if d < 0 {
		return 0
	}
	return uint64(d)
}

// try is the model path. Every way out of it has released the slot (or handed it to the
// worker) and the caller entry; the fallback then runs with nothing held. hint is the
// caller's stack hint.
func (c *Client[In, Out]) try(in In, budget time.Duration, held *entry, hint uintptr) (o Outcome[Out], r FallbackReason) {
	k := c.k
	snap := k.current.Load()
	if snap == nil {
		r = k.unattachedReason()
		k.countUnattached(hint, r)
		return o, r
	}
	e := held
	if e == nil {
		if e = k.claim(hint); e == nil {
			k.countUnattached(hint, NoSlot)
			return o, NoSlot
		}
		defer e.owner.Store(0)
	} else if !e.inCall.CompareAndSwap(0, 1) {
		// A Session used by two goroutines at once: its marker doubles as the guard.
		k.countUnattached(hint, NoSlot)
		return o, NoSlot
	}
	defer e.inCall.Store(0)
	if snap = k.enter(e, snap); snap == nil {
		r = k.unattachedReason()
		bump(&e.fallbacks[r])
		return o, r
	}
	value, gen, r := c.call(snap, e, in, budgetNs(budget))
	if r == reasonNone {
		bump(&e.model)
		if e.modelGen.Load() != uint64(gen) {
			e.modelGen.Store(uint64(gen))
		}
		e.record(k, reasonNone)
		return Outcome[Out]{Value: value, Source: Model, ModelGen: gen}, reasonNone
	}
	bump(&e.fallbacks[r])
	e.record(k, r)
	return o, r
}

// record is the breaker's view of a finished call (sdk-contract §4, "Shedding under
// overload"). Plain per-caller state; nothing shared is written.
func (e *entry) record(k *core, r FallbackReason) {
	switch r {
	case Overloaded:
	case Deadline, RingFull:
		if e.brkUntil.Load() != 0 {
			// The probe missed: shed again, for twice as long.
			e.open(k, min(e.brkInterval*2, breakerMaxNs))
		} else if e.brkMisses++; e.brkMisses >= breakerAfter {
			e.brkMisses = 0
			e.open(k, min(max(e.budget*2, 1), breakerMaxNs))
		}
	default:
		e.brkMisses = 0
		if e.brkUntil.Load() != 0 {
			e.brkUntil.Store(0)
		}
	}
}

// open sheds for a jittered interval: between half and one and a half times interval, so
// callers do not probe together.
func (e *entry) open(k *core, interval uint64) {
	now := k.clk.now()
	j := mix(now^uint64(sys.StackHint())) % max(interval, 1)
	e.brkInterval = interval
	e.brkUntil.Store(now + interval/2 + j)
}

// shedding: the breaker is open and its interval not yet over at now.
func (e *entry) shedding(now uint64) bool {
	u := e.brkUntil.Load()
	return u != 0 && now < u
}

func mix(x uint64) uint64 {
	x += 0x9e3779b97f4a7c15
	x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9
	x = (x ^ (x >> 27)) * 0x94d049bb133111eb
	return x ^ (x >> 31)
}

func gateReason(g uint64) FallbackReason {
	switch shm.GateState(g) {
	case abi.GateStateInit:
		return GateInit
	case abi.GateStatePsiShedding:
		return GateShedding
	case abi.GateStateReloading:
		return GateReloading
	case abi.GateStateDraining:
		return GateDraining
	}
	return GateDead
}

func statusReason(s uint32) FallbackReason {
	switch s {
	case abi.StatusDeadlineExceeded:
		return Deadline
	case abi.StatusBadRequest:
		return BadRequest
	case abi.StatusModelError:
		return ModelError
	case abi.StatusResourceExhausted:
		return ResourceExhausted
	case abi.StatusStaleRegistry:
		return Rebinding
	}
	return WorkerFailed
}

func safeSize[In, Out any](c Codec[In, Out], in In) (n int) {
	defer func() {
		if recover() != nil {
			n = -1
		}
	}()
	return c.Size(in)
}

func safeEncode[In, Out any](c Codec[In, Out], in In, w SlotWriter) (err error) {
	defer func() {
		if recover() != nil {
			err = ErrCodecPanic
		}
	}()
	return c.Encode(in, w)
}

func safeDecode[In, Out any](c Codec[In, Out], in In, r SlotReader) (out Out, err error) {
	defer func() {
		if recover() != nil {
			err = ErrCodecPanic
		}
	}()
	return c.Decode(in, r)
}

func (k *core) ringFault(a *arena) { a.data.AddStatusPageClientRingFaults(a.dir.StatusOff, 1) }
func (k *core) reject(a *arena)    { a.data.AddStatusPageClientRejects(a.dir.StatusOff, 1) }

// call is sdk-contract §4 from admission to resolution, step by step. Every path out of it
// has released the slot, handed it to the worker, or (after a bounded loop gave up)
// forgotten it.
func (c *Client[In, Out]) call(snap *snapshot, e *entry, in In, budget uint64) (out Out, gen uint32, r FallbackReason) {
	k := c.k
	a := snap.a
	ctl, data, dir := a.ctl, a.data, a.dir

	t0 := k.clk.now()
	if budget > dir.MaxDeadlineNs {
		bump(&e.clamped)
	}
	if k.clk.fault.Load() {
		return out, 0, ClockFault
	}
	e.budget = min(budget, dir.MaxDeadlineNs)
	e.lastCall.Store(t0)
	deadline := t0 + e.budget
	waitUntil := deadline - min(c.reserve, deadline)

	// ── Admission: one clock read and two loads from one cache line.
	g := ctl.ControlHeaderGate(0)
	if !shm.Healthy(g) {
		return out, 0, gateReason(g)
	}
	// Written so that a heartbeat newer than t0 cannot wrap (sdk-contract §4, "Time").
	if ctl.ControlHeaderSupervisorHbMonoNs(0)+dir.HbStaleNs < t0 {
		return out, 0, StaleHeartbeat
	}
	if !k.confirm(snap, shm.GateGen(g)) {
		return out, 0, Rebinding
	}
	switch snap.mode {
	case modeMismatch:
		return out, 0, SchemaMismatch
	case modeUnbound:
		k.rebind.Store(true)
		return out, 0, Rebinding
	}
	if e.shedding(t0) {
		return out, 0, Overloaded // claims no slot, publishes nothing
	}

	// ── Claim a slot in the smallest class that holds the request: the cached one, then a
	//    bounded probe.
	size := safeSize(c.codec, in)
	if size < 0 {
		return out, 0, CodecError
	}
	class := -1
	var rows uint32
	if snap.mode == modeRaw {
		for i := range dir.Slabs {
			if uint64(size) <= uint64(dir.Slabs[i].Class.ReqCap) {
				class = i
				break
			}
		}
	} else {
		if size == 0 || size > math.MaxUint32 {
			return out, 0, CodecError
		}
		rows = uint32(size)
		class = snap.reg.ClassFor(rows)
	}
	if class < 0 {
		return out, 0, TooLarge
	}
	count := dir.Slabs[class].Class.SlotCount
	start := e.cachedSlot[class] % count
	var s shm.Slot
	var idx, ep uint32
	claimed := false
	for i := uint32(0); i < abi.SlotProbe && i < count; i++ {
		idx = (start + i) % count
		s = dir.SlotAt(class, idx)
		if ep, claimed = shm.TryClaim(data, s.Rec); claimed {
			break
		}
	}
	if !claimed {
		return out, 0, NoSlot
	}
	e.cachedSlot[class] = idx
	forget := func() { e.cachedSlot[class] = (idx + 1) % count }

	// ── Encode. A codec error or panic releases the slot.
	var w SlotWriter
	var tmpl *shm.Template
	if snap.mode == modeRaw {
		e.reqLen = 0
		w = SlotWriter{seg: data, off: s.Data, cap: s.ReqCap, n: &e.reqLen}
	} else {
		tmpl = snap.reg.Request(class)
		// Stamp the region with the template's prefix if it holds another, and set the row
		// count's length fields if it differs (arrow-data-plane §7.1).
		shm.Prepare(data, s, tmpl, tmpl.Stamp(snap.epoch), rows)
		w = SlotWriter{seg: data, off: s.Data, cap: s.ReqCap, tmpl: tmpl, rows: rows}
	}
	if err := safeEncode(c.codec, in, w); err != nil {
		shm.Release(data, s.Rec, ep)
		return out, 0, CodecError
	}
	reqLen := e.reqLen
	if tmpl != nil {
		reqLen = tmpl.FrameCap
	}
	e.nextRequest++
	tEnq := k.clk.now()
	data.SetSlotRecordRequestId(s.Rec, e.nextRequest)
	data.SetSlotRecordReqLen(s.Rec, reqLen)
	data.SetSlotRecordTEnqueueNs(s.Rec, tEnq)
	shm.SetReady(data, s.Rec, ep) // the request region is the worker's from here

	// ── Enqueue: round robin per caller, skipping a full ring.
	d := shm.Descriptor{RequestID: e.nextRequest, SlotIndex: idx, DeadlineNs: waitUntil, SlotEpoch: ep, SlabClass: uint16(class), EnqueueNs: tEnq}
	cur := e.ringCursor
	e.ringCursor = cur + 1
	if !k.enqueue(a, s, ep, cur, &d) {
		// No descriptor exists, but the sweep may have failed the slot meanwhile.
		switch shm.CancelIfReady(data, s.Rec, ep) {
		case shm.NotReady:
			shm.Release(data, s.Rec, ep)
		case shm.GaveUp:
			k.ringFault(a)
			forget()
		}
		return out, 0, RingFull
	}

	// ── Wait: gate re-check, spin, then futex (or poll, beyond the parked-caller cap).
	res, gv, late := k.wait(e, a, s, ep, waitUntil)
	switch res {
	case waitDone:
		if late {
			bump(&e.lateWakes)
		}
		return c.resolve(snap, e, s, ep, class, rows, in, tEnq)
	case waitFailed:
		shm.Release(data, s.Rec, ep)
		return out, 0, WorkerFailed
	case waitCancelled:
		return out, 0, gateReason(gv) // the cancel freed the slot
	case waitTimeout:
		switch shm.AbandonSlot(data, s.Rec, ep) {
		case shm.Freed:
			return out, 0, Deadline
		case shm.Abandoned:
			forget() // the worker, or the sweep, frees it
			return out, 0, Deadline
		case shm.AbandonDone:
			// The worker won the race; the answer is valid. A wake-up more than 1 ms late is
			// how a lost one shows (signaling §7).
			if k.lateDone(data, s) {
				bump(&e.lateWakes)
			}
			return c.resolve(snap, e, s, ep, class, rows, in, tEnq)
		case shm.AbandonFailed:
			shm.Release(data, s.Rec, ep)
			return out, 0, WorkerFailed
		}
		k.ringFault(a)
		forget()
		return out, 0, WorkerFailed
	}
	k.ringFault(a) // waitGaveUp: the word kept changing under the waiter
	forget()
	return out, 0, WorkerFailed
}

// lateDone: the slot record says the worker completed more than 1 ms before now.
func (k *core) lateDone(data abi.Seg, s shm.Slot) bool {
	done, now := data.SlotRecordTDoneNs(s.Rec), k.clk.now()
	return done != 0 && done < now && now-done > 1_000_000
}

// enqueue publishes d on the caller's next ring, skipping a full one. If the call unwinds
// inside the enqueue, the ring's guard publishes its cell as void and this one takes the
// slot back (sdk-contract §4, "Every claimed ring cell is published").
func (k *core) enqueue(a *arena, s shm.Slot, ep uint32, cur uint32, d *shm.Descriptor) bool {
	armed := true
	defer func() {
		if armed {
			if shm.CancelIfReady(a.data, s.Rec, ep) == shm.NotReady {
				shm.Release(a.data, s.Rec, ep) // the sweep failed it meanwhile
			}
		}
	}()
	nr := uint32(len(a.dir.Rings))
	for i := uint32(0); i < nr; i++ {
		switch shm.Enqueue(a.data, a.dir.Rings[(cur+i)%nr], d) {
		case shm.Enqueued:
			armed = false
			return true
		case shm.Exhausted:
			k.ringFault(a)
		}
	}
	armed = false
	return false
}

const (
	waitDone = iota
	waitFailed
	waitCancelled
	waitTimeout
	waitGaveUp
)

// spinFor is the caller's spin budget (signaling §4): up to the ceiling, following twice
// the caller's typical service time, and none when that is far above the ceiling. It also
// returns the ceiling.
func (k *core) spinFor(e *entry, a *arena) (spin, ceil uint64) {
	ceil = a.dir.SpinNs
	if f := k.spinFixed.Load(); f >= 0 {
		ceil = min(ceil, uint64(f))
	} else {
		ceil = min(ceil, uint64(max(k.spinScaled.Load(), 0)))
	}
	switch {
	case ceil == 0 || e.ewma == 0:
		return ceil, ceil
	case e.ewma > 4*ceil:
		return 0, ceil
	}
	return min(ceil, 2*e.ewma), ceil
}

// wait is the client side of signaling §3.2. It is called right after the publish, which
// was sequentially consistent, so its first gate load is the re-check that pairs with the
// supervisor's store-gate-then-sweep (shm-abi §6.3).
func (k *core) wait(e *entry, a *arena, s shm.Slot, ep uint32, until uint64) (res int, gate uint64, late bool) {
	data, ctl := a.data, a.ctl
	if g := ctl.ControlHeaderGate(0); !shm.Healthy(g) && shm.CancelIfReady(data, s.Rec, ep) == shm.Cancelled {
		return waitCancelled, g, false
	}
	// Spin.
	now := k.clk.now()
	spin, ceil := k.spinFor(e, a)
	spinEnd := min(now+spin, until)
	if until-spinEnd < ceil {
		// Less than a ceiling's worth left after the spin: a futex sleep that short would
		// overrun by the timer slack, so spin to the deadline instead.
		spinEnd = until
	}
	for now < spinEnd {
		w := data.SlotRecordWord(s.Rec)
		if shm.Epoch(w) != ep {
			return waitGaveUp, 0, false
		}
		switch shm.State(w) {
		case abi.SlotStateDone:
			return waitDone, 0, false
		case abi.SlotStateFailed:
			return waitFailed, 0, false
		case abi.SlotStateReady:
			// Still queued, and the worker gone or going: take it back.
			if g := ctl.ControlHeaderGate(0); !shm.Healthy(g) && shm.CancelIfReady(data, s.Rec, ep) == shm.Cancelled {
				return waitCancelled, g, false
			}
		}
		shm.CPURelax()
		now = k.clk.now()
	}
	// Park: announce with the WAITER bit and sleep in futex, holding an OS thread; or, with
	// the parked-caller cap reached, poll at a short, growing interval, holding none.
	parked, polling := false, false
	defer func() {
		if parked {
			k.parked.Add(-1)
		}
	}()
	ival := 20 * time.Microsecond
	attempts := 0
	for {
		w := data.SlotRecordWord(s.Rec)
		if shm.Epoch(w) != ep {
			return waitGaveUp, 0, false
		}
		now = k.clk.now()
		switch shm.State(w) {
		case abi.SlotStateDone:
			// Found only once the deadline had passed, after parking: the wake-up was late
			// if the worker completed more than 1 ms ago (signaling §7).
			return waitDone, 0, parked && now >= until && k.lateDone(data, s)
		case abi.SlotStateFailed:
			return waitFailed, 0, false
		}
		if now >= until {
			return waitTimeout, 0, false
		}
		if !parked && !polling {
			if k.parked.Add(1) > k.parkedCap.Load() {
				k.parked.Add(-1)
				polling = true
			} else {
				parked = true
			}
		}
		if polling {
			time.Sleep(min(ival, time.Duration(until-now)))
			ival = min(2*ival, time.Millisecond)
			continue
		}
		if !shm.Waiter(w) {
			if !data.CasSlotRecordWord(s.Rec, w, shm.WithWaiter(w)) {
				if attempts++; attempts >= abi.CasAttempts {
					return waitGaveUp, 0, false
				}
				continue
			}
			w = shm.WithWaiter(w)
		}
		data.FutexWaitLow32(s.Rec+abi.SlotRecordWord, shm.Low32(w), until)
	}
}

// resolve handles a DONE slot: each result field read once, the response checked, decoded,
// and the slot released. A response that fails its checks is refused: counted for the
// supervisor (client_rejects), and the call falls back.
func (c *Client[In, Out]) resolve(snap *snapshot, e *entry, s shm.Slot, ep uint32, class int, rows uint32, in In, tEnq uint64) (out Out, gen uint32, r FallbackReason) {
	k := c.k
	a := snap.a
	data := a.data
	status := data.SlotRecordStatus(s.Rec)
	gen = data.SlotRecordModelGen(s.Rec)
	respLen := uintptr(data.SlotRecordRespLen(s.Rec))
	if status != abi.StatusOk {
		shm.Release(data, s.Rec, ep)
		if status == abi.StatusStaleRegistry {
			k.rebind.Store(true)
		}
		return out, 0, statusReason(status)
	}
	var rd SlotReader
	if snap.mode == modeRaw {
		if respLen > s.RespCap {
			k.reject(a)
			shm.Release(data, s.Rec, ep)
			return out, 0, WorkerFailed
		}
		rd = SlotReader{seg: data, off: s.RespOff(), len: respLen}
	} else {
		// arrow-data-plane §7.3: the response must be this template's, under this registry
		// epoch, the template's frame length, and as many rows as were asked.
		t := snap.reg.Response(class)
		if t == nil || data.SlotRecordRespStamp(s.Rec) != t.Stamp(snap.epoch) || respLen != uintptr(t.FrameCap) || shm.Rows(data, s.RespOff(), t) != uint64(rows) {
			k.reject(a)
			shm.Release(data, s.Rec, ep)
			return out, 0, WorkerFailed
		}
		rd = SlotReader{seg: data, off: s.RespOff(), len: respLen, tmpl: t, rows: rows}
	}
	out, err := safeDecode(c.codec, in, rd)
	shm.Release(data, s.Rec, ep)
	if err != nil {
		var zero Out
		return zero, 0, CodecError
	}
	// The service time is the worker's, from enqueue to its completion (slot record
	// t_done_ns): the caller's own wake-up latency left out, so that parking cannot feed
	// itself by making calls look slower.
	if done := data.SlotRecordTDoneNs(s.Rec); done > tEnq && done-tEnq < 1e9 {
		svc := done - tEnq
		if e.ewma == 0 {
			e.ewma = svc
		} else {
			e.ewma = e.ewma - e.ewma/8 + svc/8
		}
	}
	return out, gen, reasonNone
}

// ── sessions ───────────────────────────────────────────────────────────────────────────

// Session holds one caller-state entry until it is closed, so its calls claim none (sdk-go
// §6): for worker-pool servers, one per worker goroutine. It must not be used by two
// goroutines at once; a second concurrent call finds the entry's in-call marker set and falls
// back with NO_SLOT.
type Session[In, Out any] struct {
	c *Client[In, Out]
	e *entry
}

// Session takes an entry for the caller. With every entry held, the session's calls claim
// one per call, as Client.Infer does.
func (c *Client[In, Out]) Session() *Session[In, Out] {
	s := &Session[In, Out]{c: c}
	for i := range c.k.entries {
		if c.k.entries[i].owner.CompareAndSwap(0, 2) {
			s.e = &c.k.entries[i]
			break
		}
	}
	// A safety net for a session that is dropped without Close, not a substitute for it.
	runtime.SetFinalizer(s, (*Session[In, Out]).Close)
	return s
}

// Infer is Client.Infer on the session's entry.
func (s *Session[In, Out]) Infer(in In, budget time.Duration) Outcome[Out] {
	return s.c.infer(in, budget, s.e)
}

// Close gives the entry back.
func (s *Session[In, Out]) Close() {
	if s.e != nil {
		s.e.owner.Store(0)
		s.e = nil
	}
}

// ── state, statistics, closing ─────────────────────────────────────────────────────────

// AwaitBound waits until an arena is mapped and its gate is open, for callers that prefer to
// start warm. Mapped alone is not enough: the handshake can finish before the worker is
// active, and calls would then fall back with GATE_INIT.
func (c *Client[In, Out]) AwaitBound(timeout time.Duration) bool {
	end := time.Now().Add(timeout)
	for {
		if c.k.gateOpen() {
			return true
		}
		if time.Now().After(end) {
			return false
		}
		time.Sleep(time.Millisecond)
	}
}

func (k *core) gateOpen() bool {
	snap := k.current.Load()
	if snap == nil {
		return false
	}
	e := k.claim(sys.StackHint())
	if e == nil {
		return false
	}
	defer e.owner.Store(0)
	defer e.inCall.Store(0)
	if snap = k.enter(e, snap); snap == nil {
		return false
	}
	g := snap.a.ctl.ControlHeaderGate(0)
	return shm.Healthy(g) && k.confirm(snap, shm.GateGen(g)) && (snap.mode == modeRaw || snap.mode == modeFrames)
}

// Stats are the client's counters, summed over every caller entry, and the sampled
// histograms (sdk-contract §8).
type Stats struct {
	Model     uint64
	Fallbacks [numReasons]uint64
	// Sessions bound so far, and arenas unmapped after theirs ended.
	Sessions uint64
	Retired  uint64
	// LateWakes counts waits that found the slot done more than 1 ms after the worker
	// completed it, once their deadline had passed (signaling §7). It should read zero.
	LateWakes uint64
	// Clamped counts calls whose budget exceeded the sidecar's max_deadline_ns.
	Clamped uint64
	// BreakerOpen is how many callers' breakers are open now (shedding).
	BreakerOpen uint64
	// ModelGen is the highest model generation that has answered any caller.
	ModelGen uint64
	// LatencyModel and LatencyFallback are the end-to-end Infer times of the sampled calls
	// the model answered and of those that fell back (the fallback included); FallbackTime is
	// the time the sampled fallbacks spent inside the fallback function.
	LatencyModel    Histogram
	LatencyFallback Histogram
	FallbackTime    Histogram
}

// Fallback is the count for one reason.
func (s Stats) Fallback(r FallbackReason) uint64 {
	if r < numReasons {
		return s.Fallbacks[r]
	}
	return 0
}

// Stats sums the counters and the histograms. It allocates nothing and takes no lock.
func (c *Client[In, Out]) Stats() Stats { return c.k.stats() }

func (k *core) stats() Stats {
	s := Stats{Sessions: k.sessions.Load(), Retired: k.retired.Load()}
	now := k.clk.now()
	for i := range k.entries {
		e := &k.entries[i]
		s.Model += e.model.Load()
		s.LateWakes += e.lateWakes.Load()
		s.Clamped += e.clamped.Load()
		s.ModelGen = max(s.ModelGen, e.modelGen.Load())
		if e.shedding(now) {
			s.BreakerOpen++
		}
		for r := range e.fallbacks {
			s.Fallbacks[r] += e.fallbacks[r].Load()
		}
	}
	for i := range k.unattached {
		for r := range k.unattached[i].c {
			s.Fallbacks[r] += k.unattached[i].c[r].Load()
		}
	}
	s.LatencyModel = sumHist(k.hists, hModel)
	s.LatencyFallback = sumHist(k.hists, hFallback)
	s.FallbackTime = sumHist(k.hists, hFallbackFn)
	return s
}

// SetSpinCeiling fixes the process-wide spin ceiling (sdk-contract §7); zero parks at once,
// a negative duration hands it back to the SDK.
func (c *Client[In, Out]) SetSpinCeiling(d time.Duration) {
	if d < 0 {
		d = -1
	}
	c.k.spinFixed.Store(int64(d))
}

// ClockFault reports whether calls currently fall back with CLOCK_FAULT.
func (c *Client[In, Out]) ClockFault() bool { return c.k.clk.fault.Load() }

// SkewClock shifts the SDK's cheap clock by ns, as a clock anomaly would, and runs the
// check that finds it. A test hook: the conformance agent's CLOCK_SKEW.
func (c *Client[In, Out]) SkewClock(ns int64) { c.k.clk.skew(ns) }

// Close is sdk-contract §6 close(): the snapshot reference cleared (calls fall back with
// NOT_CONNECTED), a wait until no call is still inside an arena, the arenas unmapped, the
// session socket closed last. If a call stays inside for 5 s, the mapping and the socket are
// left in place and Close reports it. A client that is never closed keeps its goroutines and
// mappings for the life of the process.
func (c *Client[In, Out]) Close() error {
	k := c.k
	k.once.Do(func() {
		k.stop.Store(true)
		close(k.stopCh)
		<-k.done
		<-k.monDone
		if k.onClose != nil {
			k.onClose()
		}
	})
	return k.closeErr
}
