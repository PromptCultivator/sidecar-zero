// scz-agent-go: the Go SDK's conformance agent (testing §3, "Agent protocol", v2), after the
// Rust reference agent (sdks/rust/conformance-agent), whose header documents the protocol.
//
// It builds one client for --endpoint=PATH and obeys one command per line on stdin,
// answering one line on stdout; diagnostics go to standard error.
//
//	(on start)                          HELLO sdk=go version=V caps=...
//	CONFIG threads=N fallback_us=N kind=K fallback=F spin=S codec=C inputs=PATH   OK | ERR unsupported
//	BOUND ms=N                          BOUND true|false
//	CALL count=N budget_us=N rows=N op=O arg=N seed=N   R lines, then END calls=N
//	CODEC_FAULT mode=off|encode|decode  OK
//	UNWIND                              OK: the next call unwinds between claiming a ring cell and publishing it
//	CLOCK_SKEW ns=N                     OK: N added to the SDK's cheap clock
//	FORK                                ERR unsupported (Go cannot fork without exec)
//	SPAWN                               SPAWNED pid=N
//	STATS                               STATS model=N sessions=N retired=N late_wakes=N fb.REASON=N ...
//	PROC                                PROC fds=N maps=N threads=N
//	CLOSE                               CLOSED
//	EXIT                                BYE
//
// kind is goroutine (the default: one goroutine per calling thread), per-call (a new
// goroutine for every call), or platform (goroutines locked to OS threads); virtual is
// unsupported.
package main

import (
	"bufio"
	"encoding/binary"
	"fmt"
	"math"
	"os"
	"os/exec"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	sz "github.com/PromptCultivator/sidecar-zero/sdks/go"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/shm"
	"github.com/PromptCultivator/sidecar-zero/sdks/go/internal/sys"
)

const (
	version = "0.1.0"
	caps    = "threads,goroutines,per-call,frames,unwind,throws,nospin,cheap-clock,spawn"
	nFeat   = 256
	maxRows = 64

	faultEncode = 1
	faultDecode = 2
)

var fault atomic.Uint32

// ── the keyed codec: the request bytes in, the 8-byte keyed answer out ────────────────

type keyed struct{}

func (keyed) SchemaFingerprint() uint64 { return 0 }
func (keyed) Size(in []byte) int        { return len(in) }
func (keyed) Encode(in []byte, w sz.SlotWriter) error {
	if fault.Load() == faultEncode {
		return sz.ErrShape
	}
	return w.Write(in)
}
func (keyed) Decode(_ []byte, r sz.SlotReader) (uint64, error) {
	if fault.Load() == faultDecode || r.Len() != 8 {
		return 0, sz.ErrShape
	}
	return r.Uint64(0)
}

// ── the features codec: the prebuilt dense float32 codec over the fraud model's columns ─

type scores struct {
	n int
	v [maxRows]float32
}

type dense struct{ d sz.DenseF32Batch }

func (c dense) SchemaFingerprint() uint64 { return c.d.SchemaFingerprint() }
func (c dense) Size(in sz.F32Batch) int   { return c.d.Size(in) }
func (c dense) Encode(in sz.F32Batch, w sz.SlotWriter) error {
	if fault.Load() == faultEncode {
		return sz.ErrShape
	}
	return c.d.Encode(in, w)
}
func (c dense) Decode(in sz.F32Batch, r sz.SlotReader) (scores, error) {
	var s scores
	if fault.Load() == faultDecode || r.Rows() > maxRows {
		return s, sz.ErrShape
	}
	out, err := c.d.Decode(in, r)
	s.n = copy(s.v[:], out)
	return s, err
}

// ── the fallback: takes fallback_us, then answers 0; or records its reason and panics ──

var (
	fallbackUs atomic.Int64
	throws     atomic.Bool
)

type thrown struct{ reason sz.FallbackReason }

func slow(reason sz.FallbackReason) {
	if us := fallbackUs.Load(); us > 0 {
		time.Sleep(time.Duration(us) * time.Microsecond)
	}
	if throws.Load() {
		panic(thrown{reason})
	}
}

type agentClient interface {
	call(r req, t, s uint32) string
	awaitBound(time.Duration) bool
	stats() sz.Stats
	setSpin(off bool)
	skew(ns int64)
	close()
}

type req struct {
	op, arg, seed uint64
	rows          int
	budget        time.Duration
}

type keyedClient struct{ c *sz.Client[[]byte, uint64] }
type denseClient struct {
	c      *sz.Client[sz.F32Batch, scores]
	inputs [][]float32
}

func (k keyedClient) awaitBound(d time.Duration) bool { return k.c.AwaitBound(d) }
func (k keyedClient) stats() sz.Stats                 { return k.c.Stats() }
func (k keyedClient) setSpin(off bool)                { k.c.SetSpinCeiling(spin(off)) }
func (k keyedClient) skew(ns int64)                   { k.c.SkewClock(ns) }
func (k keyedClient) close()                          { _ = k.c.Close() }
func (d denseClient) awaitBound(t time.Duration) bool { return d.c.AwaitBound(t) }
func (d denseClient) stats() sz.Stats                 { return d.c.Stats() }
func (d denseClient) setSpin(off bool)                { d.c.SetSpinCeiling(spin(off)) }
func (d denseClient) skew(ns int64)                   { d.c.SkewClock(ns) }
func (d denseClient) close()                          { _ = d.c.Close() }

func spin(off bool) time.Duration {
	if off {
		return 0
	}
	return -1
}

// guarded runs one call and turns its panics into the protocol's sources.
func guarded(t, s uint32, f func() (sz.Source, sz.FallbackReason, uint32, string)) (line string) {
	t0 := sys.MonotonicNs()
	defer func() {
		p := recover()
		if p == nil {
			return
		}
		t1 := sys.MonotonicNs()
		if th, ok := p.(thrown); ok {
			line = fmt.Sprintf("R t=%d s=%d src=thrown reason=%s t0=%d t1=%d", t, s, th.reason, t0, t1)
			return
		}
		if e, ok := p.(error); ok && e == error(shm.UnwindPanic) {
			line = fmt.Sprintf("R t=%d s=%d src=unwound t0=%d t1=%d", t, s, t0, t1)
			return
		}
		panic(p)
	}()
	src, reason, gen, value := f()
	t1 := sys.MonotonicNs()
	if src == sz.Model {
		return fmt.Sprintf("R t=%d s=%d src=model gen=%d %s t0=%d t1=%d", t, s, gen, value, t0, t1)
	}
	return fmt.Sprintf("R t=%d s=%d src=fallback reason=%s t0=%d t1=%d", t, s, reason, t0, t1)
}

func (k keyedClient) call(r req, t, s uint32) string {
	in := request(r.op, r.arg, t, s, r.seed, r.rows)
	return guarded(t, s, func() (sz.Source, sz.FallbackReason, uint32, string) {
		o := k.c.Infer(in, r.budget)
		return o.Source, o.Reason, o.ModelGen, fmt.Sprintf("d=%#x", o.Value)
	})
}

func (d denseClient) call(r req, t, s uint32) string {
	rows := max(r.rows, 1)
	in := sz.F32Batch{Features: make([]float32, 0, rows*nFeat), Scores: make([]float32, rows)}
	for i := 0; i < rows; i++ {
		in.Features = append(in.Features, d.inputs[(int(s)+i)%len(d.inputs)]...)
	}
	return guarded(t, s, func() (sz.Source, sz.FallbackReason, uint32, string) {
		o := d.c.Infer(in, r.budget)
		var b strings.Builder
		b.WriteString("v=")
		for i := 0; i < o.Value.n; i++ {
			if i > 0 {
				b.WriteByte(',')
			}
			fmt.Fprintf(&b, "%#x", math.Float32bits(o.Value.v[i]))
		}
		return o.Source, o.Reason, o.ModelGen, b.String()
	})
}

// ── the request format of conformance/proto ──────────────────────────────────────────

const magic = 0x534f414843_5a4353 // "SCZCHAOS", little-endian

func splitmix64(x uint64) uint64 {
	x += 0x9e3779b97f4a7c15
	x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9
	x = (x ^ (x >> 27)) * 0x94d049bb133111eb
	return x ^ (x >> 31)
}

func request(op, arg uint64, t, s uint32, seed uint64, rows int) []byte {
	nonce := uint64(t)<<32 | uint64(s)
	b := make([]byte, 0, (5+rows)*8)
	for _, w := range []uint64{magic, op, arg, nonce, seed} {
		b = binary.LittleEndian.AppendUint64(b, w)
	}
	for i := 0; i < rows; i++ {
		b = binary.LittleEndian.AppendUint64(b, splitmix64(seed^nonce^uint64(i)))
	}
	return b
}

var ops = map[string]uint64{"normal": 0, "delay": 1, "crash": 2, "hang": 3, "status": 4, "bad_len": 5, "stall_wake": 6}

func parseOp(s string) uint64 {
	if v, ok := ops[s]; ok {
		return v
	}
	v, _ := strconv.ParseUint(s, 10, 64)
	return v
}

// ── the agent ─────────────────────────────────────────────────────────────────────────

type config struct {
	threads  int
	kind     string
	spinOff  bool
	features string // inputs file; empty: the keyed codec
}

func readInputs(path string) ([][]float32, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	if len(b) == 0 || len(b)%(nFeat*4) != 0 {
		return nil, fmt.Errorf("%s: not rows of %d float32", path, nFeat)
	}
	var rows [][]float32
	for off := 0; off < len(b); off += nFeat * 4 {
		r := make([]float32, nFeat)
		for j := range r {
			r[j] = math.Float32frombits(binary.LittleEndian.Uint32(b[off+4*j:]))
		}
		rows = append(rows, r)
	}
	return rows, nil
}

func build(endpoint string, cfg *config) (agentClient, error) {
	fb := func(_ []byte, r sz.FallbackReason) uint64 { slow(r); return 0 }
	var c agentClient
	if cfg.features == "" {
		cl, err := sz.New[[]byte, uint64](sz.Endpoint(endpoint), keyed{}, fb)
		if err != nil {
			return nil, err
		}
		c = keyedClient{cl}
	} else {
		inputs, err := readInputs(cfg.features)
		if err != nil {
			return nil, err
		}
		dfb := func(in sz.F32Batch, r sz.FallbackReason) scores {
			slow(r)
			return scores{n: min(len(in.Scores), maxRows)}
		}
		cl, err := sz.New[sz.F32Batch, scores](sz.Endpoint(endpoint), dense{sz.NewDenseF32Batch(nFeat)}, dfb)
		if err != nil {
			return nil, err
		}
		c = denseClient{cl, inputs}
	}
	c.setSpin(cfg.spinOff)
	return c, nil
}

func kv(words []string) map[string]string {
	m := map[string]string{}
	for _, w := range words {
		if k, v, ok := strings.Cut(w, "="); ok {
			m[k] = v
		}
	}
	return m
}

func num(m map[string]string, k string, def uint64) uint64 {
	if v, err := strconv.ParseUint(m[k], 10, 64); err == nil {
		return v
	}
	return def
}

func procCounts() string {
	count := func(dir string) int {
		d, err := os.ReadDir(dir)
		if err != nil {
			return 0
		}
		return len(d)
	}
	maps := 0
	if b, err := os.ReadFile("/proc/self/maps"); err == nil {
		for _, l := range strings.Split(string(b), "\n") {
			if strings.Contains(l, "memfd:scz-") {
				maps++
			}
		}
	}
	return fmt.Sprintf("PROC fds=%d maps=%d threads=%d", count("/proc/self/fd"), maps, count("/proc/self/task"))
}

// prewarm starts the OS threads the process can ever need and leaves them idle. The Go
// runtime never ends an idle thread, so without this the thread count would follow the peak
// number of callers blocked in futex at once (bounded by the SDK's parked-caller cap) instead
// of returning to where it was, which is what PROC is compared on. Locked to a thread, each
// goroutine pins one; released together, the threads stay in the runtime's idle pool.
func prewarm(n int) {
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

func main() {
	var endpoint string
	for _, a := range os.Args[1:] {
		if p, ok := strings.CutPrefix(a, "--endpoint="); ok {
			endpoint = p
		}
	}
	if endpoint == "" {
		fmt.Fprintln(os.Stderr, "usage: scz-agent-go --endpoint=PATH")
		os.Exit(2)
	}
	procs := runtime.GOMAXPROCS(0)
	prewarm(max(16, 4*procs) + 2*procs + 16)

	out := bufio.NewWriter(os.Stdout)
	var outMu sync.Mutex
	say := func(s string) {
		outMu.Lock()
		out.WriteString(s)
		out.WriteByte('\n')
		out.Flush()
		outMu.Unlock()
	}
	cfg := &config{threads: 1, kind: "goroutine"}
	client, err := build(endpoint, cfg)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	agentCaps := caps
	if shm.CrashPoints {
		agentCaps += ",crashpoints"
	}
	say(fmt.Sprintf("HELLO sdk=go version=%s caps=%s", version, agentCaps))

	in := bufio.NewScanner(os.Stdin)
	in.Buffer(make([]byte, 1<<16), 1<<20)
	for in.Scan() {
		words := strings.Fields(in.Text())
		if len(words) == 0 {
			continue
		}
		cmd, m := words[0], kv(words[1:])
		switch cmd {
		case "CONFIG":
			kind, hasKind := m["kind"]
			if hasKind && kind != "goroutine" && kind != "per-call" && kind != "platform" {
				say("ERR unsupported")
				continue
			}
			cfg.threads = int(max(num(m, "threads", uint64(cfg.threads)), 1))
			if hasKind {
				cfg.kind = kind
			}
			if _, ok := m["fallback_us"]; ok {
				fallbackUs.Store(int64(num(m, "fallback_us", 0)))
			}
			if f, ok := m["fallback"]; ok {
				throws.Store(f == "throws")
			}
			if s, ok := m["spin"]; ok {
				cfg.spinOff = s == "off"
			}
			features := cfg.features
			if c, ok := m["codec"]; ok {
				features = ""
				if c == "features" {
					features = m["inputs"]
					if features == "" {
						say("ERR codec=features needs inputs")
						continue
					}
				}
			}
			if features != cfg.features && client != nil {
				// Another codec is another client: the old session ends first.
				cfg.features = features
				client.close()
				if client, err = build(endpoint, cfg); err != nil {
					say("ERR " + err.Error())
					continue
				}
			}
			if client != nil {
				client.setSpin(cfg.spinOff)
			}
			say("OK")
		case "CODEC_FAULT":
			switch m["mode"] {
			case "encode":
				fault.Store(faultEncode)
			case "decode":
				fault.Store(faultDecode)
			default:
				fault.Store(0)
			}
			say("OK")
		case "UNWIND":
			shm.UnwindOnce.Store(true)
			say("OK")
		case "FORK":
			say("ERR unsupported")
		case "SPAWN":
			c := exec.Command("sleep", "600")
			if err := c.Start(); err != nil {
				say("ERR " + err.Error())
			} else {
				say(fmt.Sprintf("SPAWNED pid=%d", c.Process.Pid))
				_ = c.Process.Release()
			}
		case "PROC":
			say(procCounts())
		case "EXIT":
			say("BYE")
			if client != nil {
				client.close()
			}
			os.Exit(0)
		default:
			if client == nil {
				say("ERR closed")
				continue
			}
			switch cmd {
			case "BOUND":
				say(fmt.Sprintf("BOUND %t", client.awaitBound(time.Duration(num(m, "ms", 5000))*time.Millisecond)))
			case "CLOCK_SKEW":
				ns, err := strconv.ParseInt(m["ns"], 10, 64)
				if err != nil {
					say("ERR ns")
					continue
				}
				client.skew(ns)
				say("OK")
			case "CALL":
				r := req{op: parseOp(m["op"]), arg: num(m, "arg", 0), seed: num(m, "seed", 1), rows: int(num(m, "rows", 0)), budget: time.Duration(num(m, "budget_us", 20_000)) * time.Microsecond}
				calls(client, cfg, r, int(num(m, "count", 1)), say, &outMu, out)
			case "STATS":
				s := client.stats()
				var b strings.Builder
				fmt.Fprintf(&b, "STATS model=%d sessions=%d retired=%d late_wakes=%d", s.Model, s.Sessions, s.Retired, s.LateWakes)
				for _, r := range sz.Reasons() {
					if n := s.Fallback(r); n > 0 {
						fmt.Fprintf(&b, " fb.%s=%d", r, n)
					}
				}
				say(b.String())
			case "CLOSE":
				client.close()
				client = nil
				say("CLOSED")
			default:
				say("ERR unknown command " + cmd)
			}
		}
	}
	if client != nil {
		client.close()
	}
}

// calls runs count calls on each of cfg.threads callers and prints their lines, then END.
func calls(c agentClient, cfg *config, r req, count int, say func(string), mu *sync.Mutex, out *bufio.Writer) {
	lines := make([][]string, cfg.threads)
	var wg sync.WaitGroup
	for t := 0; t < cfg.threads; t++ {
		wg.Add(1)
		go func(t int) {
			defer wg.Done()
			if cfg.kind == "platform" {
				runtime.LockOSThread()
				defer runtime.UnlockOSThread()
			}
			ls := make([]string, 0, count)
			for s := 0; s < count; s++ {
				if cfg.kind == "per-call" {
					ch := make(chan string)
					go func() { ch <- c.call(r, uint32(t), uint32(s)) }()
					ls = append(ls, <-ch)
				} else {
					ls = append(ls, c.call(r, uint32(t), uint32(s)))
				}
			}
			lines[t] = ls
		}(t)
	}
	wg.Wait()
	mu.Lock()
	n := 0
	for _, ls := range lines {
		for _, l := range ls {
			out.WriteString(l)
			out.WriteByte('\n')
			n++
		}
	}
	fmt.Fprintf(out, "END calls=%d\n", n)
	out.Flush()
	mu.Unlock()
}
