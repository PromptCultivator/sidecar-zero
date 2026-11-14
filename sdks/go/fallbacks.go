package sidecarzero

import "time"

// Reference fallbacks (sdk-contract §3.2): pure, allocation-free, and well inside the 400 ns
// target, for callers who have no better answer of their own. Each accepts any input,
// including one the codec rejected.

// ConstantFallback always answers v.
func ConstantFallback[In, Out any](v Out) FallbackFunc[In, Out] {
	return func(In, FallbackReason) Out { return v }
}

// ThresholdFallback answers above when feature i of the row exceeds threshold, else below
// (and below for a row too short to have feature i).
func ThresholdFallback(i int, threshold, above, below float32) FallbackFunc[[]float32, float32] {
	return func(x []float32, _ FallbackReason) float32 {
		if i >= 0 && i < len(x) && x[i] > threshold {
			return above
		}
		return below
	}
}

// LinearFallback answers bias + Σ weights[j]·x[j], over the features both have.
func LinearFallback(weights []float32, bias float32) FallbackFunc[[]float32, float32] {
	w := append([]float32(nil), weights...)
	return func(x []float32, _ FallbackReason) float32 {
		s := bias
		for j := 0; j < len(w) && j < len(x); j++ {
			s += w[j] * x[j]
		}
		return s
	}
}

// MeasureFallback is the micro-benchmark harness of sdk-contract §3.2: the mean time of n
// calls of f on in, to hold against the fallback budget (target: under 400 ns).
func MeasureFallback[In, Out any](f FallbackFunc[In, Out], in In, n int) time.Duration {
	if n <= 0 {
		n = 1
	}
	var sink Out
	t0 := time.Now()
	for i := 0; i < n; i++ {
		sink = f(in, Deadline)
	}
	el := time.Since(t0)
	_ = sink
	return el / time.Duration(n)
}
