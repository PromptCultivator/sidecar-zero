package sidecarzero

import (
	"errors"
	"sync/atomic"
	"time"

	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/shm"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/sys"
)

// Status is a completed slot's status, as an in-process model reports it.
type Status uint32

const (
	StatusOK                Status = abi.StatusOk
	StatusDeadlineExceeded  Status = abi.StatusDeadlineExceeded
	StatusBadRequest        Status = abi.StatusBadRequest
	StatusModelError        Status = abi.StatusModelError
	StatusResourceExhausted Status = abi.StatusResourceExhausted
	StatusInternal          Status = abi.StatusInternal
)

// ServeFunc is an in-process model: it reads the request bytes and writes the response into
// resp, returning its length and status. It runs on the in-process worker's goroutine.
type ServeFunc func(req []byte, resp []byte) (n int, status Status)

// InProcess is the test double of sdk-contract §9: a Client whose arena is ordinary memory,
// served by a goroutine that runs a model function in-process, so that application tests can
// exercise model answers and every fallback on any platform. It runs the real call algorithm
// over the real protocol, against opaque-bytes codecs.
type InProcess struct {
	k     *core
	a     *arena
	local [2]*abi.Local
	serve ServeFunc

	stop   atomic.Bool
	done   chan struct{}
	mode   atomic.Uint32 // the worker's behavior: tests of the SDK itself
	delay  atomic.Int64
	served atomic.Uint64
	head   uint64
	req    []byte
	resp   []byte
}

// Worker behaviors, for the SDK's own tests.
const (
	behaveAnswer    = iota // answer at once
	behaveAfterPark        // answer once the caller has announced it parked
	behaveHoldOdd          // never answer a request whose first byte is odd; free it once abandoned
	behaveDelay            // answer after delay
)

// NewInProcess builds a client over an in-process arena served by serve. The codec must
// carry opaque bytes (fingerprint 0): the in-process arena has an empty registry.
func NewInProcess[In, Out any](codec Codec[In, Out], fallback FallbackFunc[In, Out], serve ServeFunc, opts ...Option) (*Client[In, Out], *InProcess, error) {
	if fallback == nil {
		return nil, nil, ErrNoFallback
	}
	if codec == nil || serve == nil {
		return nil, nil, ErrNoCodec
	}
	if codec.SchemaFingerprint() != 0 {
		return nil, nil, errors.New("sidecarzero: the in-process double carries opaque bytes only")
	}
	return newInProcessClient(codec, fallback, serve, true, opts...)
}

// newInProcessClient is NewInProcess, optionally without the monitor goroutine (the clock
// check and the spin ceiling), whose once-a-second file reads allocate: the allocation gate
// measures the call path alone.
func newInProcessClient[In, Out any](codec Codec[In, Out], fallback FallbackFunc[In, Out], serve ServeFunc, monitor bool, opts ...Option) (*Client[In, Out], *InProcess, error) {
	k := newCore("in-process", 0, opts)
	ip, err := newInProcess(k, serve)
	if err != nil {
		return nil, nil, err
	}
	if monitor {
		go k.monitor()
	} else {
		close(k.monDone)
	}
	return &Client[In, Out]{k: k, codec: codec, fallback: fallback, reserve: uint64(k.opts.fallbackReserve)}, ip, nil
}

func newInProcess(k *core, serve ServeFunc) (*InProcess, error) {
	g := shm.Geometry{Rings: []uint64{1024}, SlabClasses: []shm.SlabClass{{SlotSize: 4096, SlotCount: 256, ReqCap: 2048, RespCap: 2048}}, StagingLen: 4096}
	l, err := shm.ComputeLayout(g)
	if err != nil {
		return nil, err
	}
	ctl, data := abi.NewLocal(shm.CtlLen), abi.NewLocal(uintptr(l.DataLen))
	cfg := shm.DefaultConfig(0x1A, sys.MonotonicNs())
	shm.WriteControl(ctl.Seg(), l, cfg)
	shm.WriteData(data.Seg(), l, cfg)
	dir, err := shm.ReadDirectory(ctl.Seg(), l.DataLen)
	if err != nil {
		return nil, err
	}
	ip := &InProcess{k: k, local: [2]*abi.Local{ctl, data}, serve: serve, done: make(chan struct{}), req: make([]byte, 2048), resp: make([]byte, 2048)}
	ip.a = &arena{ctl: ctl.Seg(), data: data.Seg(), dir: dir}
	sv := abi.Supervisor(ip.a.ctl)
	// The model block: generation 1, an empty registry under epoch 1. The supervisor's
	// heartbeat is set a century ahead: nothing here beats.
	sv.SetControlHeaderModelGen(0, 1)
	sv.SetControlHeaderRegistryEpoch(0, 1)
	sv.SetControlHeaderRegistryOff(0, shm.RegistryOff)
	sv.SetControlHeaderSupervisorHbMonoNs(0, sys.MonotonicNs()+100*365*24*3600*1e9)
	ip.SetHealthy(true)
	k.publish(ip.a)
	k.sessions.Add(1)
	close(k.done) // no connection goroutine
	k.onClose = func() {
		k.setState(Disconnected)
		if !k.retire(k.current.Swap(nil)) {
			k.closeErr = errStillInUse
		}
	}
	go ip.work()
	return ip, nil
}

// SetHealthy opens or closes the in-process gate: closed, calls fall back with GATE_DEAD.
func (ip *InProcess) SetHealthy(ok bool) {
	st := uint32(abi.GateStateHealthy)
	if !ok {
		st = abi.GateStateDead
	}
	abi.Supervisor(ip.a.ctl).SetControlHeaderGate(0, shm.Gate(1, 0, st))
}

// Served is how many requests the in-process worker completed.
func (ip *InProcess) Served() uint64 { return ip.served.Load() }

// Close stops the worker. Close the client first.
func (ip *InProcess) Close() {
	if ip.stop.Swap(true) {
		return
	}
	r := ip.a.dir.Rings[0]
	ip.a.data.FutexWake32(r.Off+abi.RingHeaderConsumerWait, 1)
	<-ip.done
}

// work is the in-process worker's consume loop: dequeue, claim, serve, complete, and park on
// the ring's wake word when idle (signaling §3.1). It allocates nothing per request.
func (ip *InProcess) work() {
	defer close(ip.done)
	data, dir := ip.a.data, ip.a.dir
	r := dir.Rings[0]
	w := abi.Worker(data)
	lastWork := sys.MonotonicNs()
	for !ip.stop.Load() {
		d, ok := shm.Dequeue(data, r, &ip.head)
		if !ok {
			// Spin for 200 µs after the last request, then park.
			if sys.MonotonicNs() < lastWork+200_000 {
				shm.CPURelax()
				continue
			}
			// Announce, re-check, sleep (a timed sleep, so that Close is seen).
			w.SetRingHeaderConsumerWait(r.Off, abi.ConsumerWaitParked)
			if data.CellSeq(r.Cell(ip.head)) != ip.head+1 && !ip.stop.Load() {
				data.FutexWait32(r.Off+abi.RingHeaderConsumerWait, abi.ConsumerWaitParked, sys.MonotonicNs()+20e6)
			}
			w.SetRingHeaderConsumerWait(r.Off, abi.ConsumerWaitAwake)
			continue
		}
		lastWork = sys.MonotonicNs()
		if d.Flags&abi.CellFlagVoid != 0 || int(d.SlabClass) >= len(dir.Slabs) || d.SlotIndex >= dir.Slabs[d.SlabClass].Class.SlotCount {
			continue
		}
		s := dir.SlotAt(int(d.SlabClass), d.SlotIndex)
		if !shm.WorkerClaim(data, s.Rec, d.SlotEpoch, 1) {
			continue
		}
		n := min(uintptr(data.SlotRecordReqLen(s.Rec)), s.ReqCap)
		req := ip.req[:n]
		data.CopyOut(s.Data, req)
		switch ip.mode.Load() {
		case behaveAfterPark:
			for i := 0; i < 1e7 && !shm.Waiter(data.SlotRecordWord(s.Rec)); i++ {
				shm.CPURelax()
			}
		case behaveHoldOdd:
			if len(req) > 0 && req[0]&1 == 1 {
				for i := 0; i < 1e8 && shm.State(data.SlotRecordWord(s.Rec)) == abi.SlotStateProcessing; i++ {
					shm.CPURelax()
				}
				shm.Complete(data, s.Rec, d.SlotEpoch) // frees it: ABANDONED → FREE
				continue
			}
		case behaveDelay:
			for end := sys.MonotonicNs() + uint64(ip.delay.Load()); sys.MonotonicNs() < end; {
				shm.CPURelax()
			}
		}
		rn, st := ip.serve(req, ip.resp)
		// The length is recorded as the model gave it; the client refuses one out of range.
		data.CopyIn(s.RespOff(), ip.resp[:max(0, min(rn, int(s.RespCap), len(ip.resp)))])
		w.SetSlotRecordRespLen(s.Rec, uint32(max(rn, 0)))
		w.SetSlotRecordStatus(s.Rec, uint32(st))
		w.SetSlotRecordModelGen(s.Rec, 1)
		w.SetSlotRecordRespStamp(s.Rec, 0)
		w.SetSlotRecordTDoneNs(s.Rec, sys.MonotonicNs())
		if shm.Complete(data, s.Rec, d.SlotEpoch) {
			ip.served.Add(1)
		}
	}
}

// Wait until the in-process worker has served n requests, or timeout.
func (ip *InProcess) waitServed(n uint64, timeout time.Duration) bool {
	end := time.Now().Add(timeout)
	for ip.served.Load() < n {
		if time.Now().After(end) {
			return false
		}
		time.Sleep(time.Millisecond)
	}
	return true
}
