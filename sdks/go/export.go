package sidecarzero

import (
	"math"
	"math/big"
	"strconv"
	"strings"
)

// Label is one label of a metric series: a name and its value.
type Label struct {
	Name, Value string
}

// MetricsSink is where Export writes each metric of sdk-contract §8. Adapters for a metrics
// library implement it; PrometheusText is the one in the core, which has no dependency.
//
// labels are in order ({source, fallback}, {reason, DEADLINE}), nil for none. Each call gets
// slices of its own, which the sink may keep.
type MetricsSink interface {
	// Counter is a counter's value: exact, never decreasing.
	Counter(name string, labels []Label, value uint64)
	Gauge(name string, labels []Label, value float64)
	// Histogram is a sampled histogram. bounds are BucketsSeconds(); cumulative has one more
	// entry, for +Inf, which equals count. sum is in seconds.
	Histogram(name string, labels []Label, bounds []float64, cumulative []uint64, sum float64, count uint64)
}

// Export writes every metric of sdk-contract §8 to sink in one pass, in the order every SDK
// uses: the calls by source (every reason, zeros included), the two latency histograms, the
// fallback histogram, then the unlabelled counters and gauges. Not on the call path: it
// allocates the slices it hands the sink.
func (c *Client[In, Out]) Export(sink MetricsSink) { c.k.export(sink) }

// PrometheusText is Export rendered in the Prometheus text exposition format, for a service
// that serves /metrics itself.
func (c *Client[In, Out]) PrometheusText() string {
	var t PrometheusText
	c.k.export(&t)
	return t.String()
}

func (k *core) export(sink MetricsSink) {
	s := k.stats()
	sink.Counter("scz_client_calls_total", []Label{{"source", "model"}}, s.Model)
	for r := FallbackReason(0); r < numReasons; r++ {
		sink.Counter("scz_client_calls_total", []Label{{"source", "fallback"}, {"reason", r.String()}}, s.Fallbacks[r])
	}
	histogram(sink, "scz_client_latency_seconds", []Label{{"source", "model"}}, &s.LatencyModel)
	histogram(sink, "scz_client_latency_seconds", []Label{{"source", "fallback"}}, &s.LatencyFallback)
	histogram(sink, "scz_client_fallback_seconds", nil, &s.FallbackTime)
	sink.Counter("scz_client_budget_clamped_total", nil, s.Clamped)
	sink.Gauge("scz_client_breaker_open", nil, float64(s.BreakerOpen))
	sink.Counter("scz_client_late_wakes_total", nil, s.LateWakes)
	sink.Counter("scz_client_reconnects_total", nil, max(s.Sessions, 1)-1)
	sink.Gauge("scz_client_state", nil, float64(k.state.Load()))
	sink.Gauge("scz_client_model_gen", nil, float64(s.ModelGen))
}

func histogram(sink MetricsSink, name string, labels []Label, h *Histogram) {
	b, c := BucketsSeconds(), h.Cumulative
	sink.Histogram(name, labels, b[:], c[:], h.SumSeconds, h.Count)
}

// PrometheusText is a MetricsSink that renders the Prometheus text exposition format
// (sdk-contract §8, "A text adapter in the core"): one # TYPE line per metric family, then
// its series; histograms as _bucket lines (with le), _sum, and _count. Numbers are written as
// the Rust SDK writes them (the shortest decimal that reads back as the same float64, never
// an exponent: 0.000001, 1.5, 2), so every SDK renders the same numbers as the same text.
//
//	var t sidecarzero.PrometheusText
//	client.Export(&t)
//	body := t.String() // or client.PrometheusText()
//
// The zero value is ready to use.
type PrometheusText struct {
	b    strings.Builder
	last string
}

// String is the text written so far.
func (t *PrometheusText) String() string { return t.b.String() }

func (t *PrometheusText) kind(name, kind string) {
	if t.last != name {
		t.b.WriteString("# TYPE ")
		t.b.WriteString(name)
		t.b.WriteByte(' ')
		t.b.WriteString(kind)
		t.b.WriteByte('\n')
		t.last = name
	}
}

// labelSet writes {name="value",...}, the labels then le if it is set; nothing if there are
// none.
func (t *PrometheusText) labelSet(labels []Label, le string) {
	if len(labels) == 0 && le == "" {
		return
	}
	t.b.WriteByte('{')
	for i, l := range labels {
		if i > 0 {
			t.b.WriteByte(',')
		}
		t.pair(l.Name, l.Value)
	}
	if le != "" {
		if len(labels) > 0 {
			t.b.WriteByte(',')
		}
		t.pair("le", le)
	}
	t.b.WriteByte('}')
}

func (t *PrometheusText) pair(name, value string) {
	t.b.WriteString(name)
	t.b.WriteString(`="`)
	for i := 0; i < len(value); i++ {
		switch ch := value[i]; ch {
		case '\\':
			t.b.WriteString(`\\`)
		case '"':
			t.b.WriteString(`\"`)
		case '\n':
			t.b.WriteString(`\n`)
		default:
			t.b.WriteByte(ch)
		}
	}
	t.b.WriteByte('"')
}

func (t *PrometheusText) line(name, suffix string, labels []Label, le, value string) {
	t.b.WriteString(name)
	t.b.WriteString(suffix)
	t.labelSet(labels, le)
	t.b.WriteByte(' ')
	t.b.WriteString(value)
	t.b.WriteByte('\n')
}

// Counter implements MetricsSink.
func (t *PrometheusText) Counter(name string, labels []Label, value uint64) {
	t.kind(name, "counter")
	t.line(name, "", labels, "", strconv.FormatUint(value, 10))
}

// Gauge implements MetricsSink.
func (t *PrometheusText) Gauge(name string, labels []Label, value float64) {
	t.kind(name, "gauge")
	t.line(name, "", labels, "", formatFloat(value))
}

// Histogram implements MetricsSink.
func (t *PrometheusText) Histogram(name string, labels []Label, bounds []float64, cumulative []uint64, sum float64, count uint64) {
	t.kind(name, "histogram")
	for i, c := range cumulative {
		le := "+Inf"
		if i < len(bounds) {
			le = formatFloat(bounds[i])
		}
		t.line(name, "_bucket", labels, le, strconv.FormatUint(c, 10))
	}
	t.line(name, "_sum", labels, "", formatFloat(sum))
	t.line(name, "_count", labels, "", strconv.FormatUint(count, 10))
}

// formatFloat writes a float64 as Rust's Display for f64 does: the shortest decimal that reads
// back as the same float64, the closest to it of those, in positional notation, without a
// trailing ".0"; NaN, inf, -inf, and -0 as Rust spells them. Where the float64 lies exactly
// halfway between two shortest decimals, strconv takes the even one and Rust the one further
// from zero: Rust's here.
func formatFloat(v float64) string {
	switch {
	case math.IsNaN(v):
		return "NaN"
	case math.IsInf(v, 1):
		return "inf"
	case math.IsInf(v, -1):
		return "-inf"
	}
	s := strconv.FormatFloat(v, 'f', -1, 64)
	if v == 0 {
		return s // "0" or "-0"
	}
	// The shortest digits D and exponent q, |v| ≈ D·10^q; then whether |v| is exactly
	// D·10^q + 10^q/2, a tie strconv broke towards D.
	a := math.Abs(v)
	mant, exp, _ := strings.Cut(strconv.FormatFloat(a, 'e', -1, 64), "e")
	digits := strings.Replace(mant, ".", "", 1)
	e, _ := strconv.Atoi(exp)
	q := e - (len(digits) - 1)
	d, _ := new(big.Int).SetString(digits, 10)
	unit := pow10(q)
	off := new(big.Rat).Sub(new(big.Rat).SetFloat64(a), new(big.Rat).Mul(new(big.Rat).SetInt(d), unit))
	if off.Sign() <= 0 || new(big.Rat).Add(off, off).Cmp(unit) != 0 {
		return s
	}
	up := new(big.Rat).Mul(new(big.Rat).SetInt(d.Add(d, big.NewInt(1))), unit)
	u := up.FloatString(max(0, -q))
	if strings.Contains(u, ".") {
		u = strings.TrimSuffix(strings.TrimRight(u, "0"), ".")
	}
	if f, err := strconv.ParseFloat(u, 64); err != nil || f != a {
		return s
	}
	if v < 0 {
		return "-" + u
	}
	return u
}

// pow10 is 10^q as an exact rational.
func pow10(q int) *big.Rat {
	p := new(big.Int).Exp(big.NewInt(10), big.NewInt(int64(max(q, -q))), nil)
	if q >= 0 {
		return new(big.Rat).SetInt(p)
	}
	return new(big.Rat).SetFrac(big.NewInt(1), p)
}
