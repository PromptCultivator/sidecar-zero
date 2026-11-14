package sidecarzero

// FallbackReason says why a call returned the fallback's answer (sdk-contract §3.3). The
// set is open: a minor version may add reasons, so switch statements over it need a default.
type FallbackReason uint8

const (
	NotConnected FallbackReason = iota
	AbiMismatch
	SchemaMismatch
	Rebinding
	GateInit
	GateShedding
	GateReloading
	GateDead
	GateDraining
	StaleHeartbeat
	ClockFault
	Overloaded
	TooLarge
	NoSlot
	CodecError
	RingFull
	Deadline
	WorkerFailed
	ModelError
	BadRequest
	ResourceExhausted
	numReasons
)

var reasonNames = [numReasons]string{
	"NOT_CONNECTED", "ABI_MISMATCH", "SCHEMA_MISMATCH", "REBINDING", "GATE_INIT", "GATE_SHEDDING",
	"GATE_RELOADING", "GATE_DEAD", "GATE_DRAINING", "STALE_HEARTBEAT", "CLOCK_FAULT", "OVERLOADED",
	"TOO_LARGE", "NO_SLOT", "CODEC_ERROR", "RING_FULL", "DEADLINE", "WORKER_FAILED", "MODEL_ERROR",
	"BAD_REQUEST", "RESOURCE_EXHAUSTED",
}

// String is the reason's metric label.
func (r FallbackReason) String() string {
	if r < numReasons {
		return reasonNames[r]
	}
	return "UNKNOWN"
}

// Reasons lists every reason this version defines, in order.
func Reasons() []FallbackReason {
	out := make([]FallbackReason, numReasons)
	for i := range out {
		out[i] = FallbackReason(i)
	}
	return out
}

// Source says where an answer came from.
type Source uint8

const (
	// Model: the sidecar's model answered within the budget.
	Model Source = iota + 1
	// Fallback: the caller's fallback answered; Outcome.Reason says why.
	Fallback
)

func (s Source) String() string {
	switch s {
	case Model:
		return "model"
	case Fallback:
		return "fallback"
	}
	return "?"
}

// Outcome is what Infer returns: always a value (sdk-contract §1).
type Outcome[Out any] struct {
	Value  Out
	Source Source
	// Reason is meaningful when Source is Fallback.
	Reason FallbackReason
	// ModelGen is the model generation that answered, when Source is Model.
	ModelGen uint32
}

// IsModel reports whether the model answered.
func (o Outcome[Out]) IsModel() bool { return o.Source == Model }

// FallbackFunc is the caller's own answer, used whenever the model's is not available in
// time. It must be pure, fast, must not block, allocate, or panic, and must accept any input
// the caller can pass, including inputs the codec rejected (sdk-contract §3.2). It is a
// required parameter of New; nil is rejected.
type FallbackFunc[In, Out any] func(in In, reason FallbackReason) Out
