package sidecarzero

import "sync/atomic"

// Metrics on the call path (sdk-contract §8): which calls are timed, and the sampled
// histograms they are recorded in. Nothing here allocates; an untimed call reads no clock.
// The export, the sink, and the text format are in export.go.

// DefaultSampleEvery is how many calls of a goroutine make one timed call, by default
// (sdk-contract §8: one call in 64).
const DefaultSampleEvery = 64

// The bucket bounds of every histogram, in nanoseconds (BucketsSeconds in seconds); a last
// bucket, +Inf, follows them.
var boundsNs = [16]uint64{1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000,
	1_000_000, 2_000_000, 5_000_000, 10_000_000, 20_000_000, 50_000_000, 100_000_000}

// BucketsSeconds are the upper bounds of the histogram buckets in seconds, fixed by
// sdk-contract §8 and the same in every SDK; a last bucket, +Inf, follows them. A duration
// lands in the first bucket whose bound is at least as large.
func BucketsSeconds() [16]float64 {
	return [16]float64{1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4, 2e-4, 5e-4, 1e-3, 2e-3, 5e-3, 1e-2, 2e-2, 5e-2, 0.1}
}

// hist is one histogram's buckets, +Inf included, and its sum in nanoseconds: added to by
// timed calls only, so rarely, from any goroutine.
type hist struct {
	buckets [17]atomic.Uint64
	sumNs   atomic.Uint64
}

func (h *hist) record(ns uint64) {
	i := 0
	for i < len(boundsNs) && ns > boundsNs[i] {
		i++
	}
	h.buckets[i].Add(1)
	h.sumNs.Add(ns)
}

// The three histograms of a shard: end-to-end Infer time by source, and the fallback
// function alone.
const (
	hModel = iota
	hFallback
	hFallbackFn
)

// histShard is one of a client's histShards shards, chosen by the caller's stack hint so
// that goroutines rarely share a line.
type histShard struct {
	h [3]hist
	_ [16]byte // to a whole number of cache lines (TestEntryLayout)
}

const histShards = 16

// The sampling counts (sdk-contract §8, "Sampling"): each calling goroutine's calls, over
// every client, in the stripe of its stack hint, 128 bytes apart so that goroutines in
// different stripes never write one line. Go has neither goroutine-local storage nor a
// goroutine id, so a goroutine is known by the page its stack is on: two goroutines whose
// stacks hash to one stripe share a count, which shifts only which of their calls are timed.
const sampleStripes = 1024

type sampleCount struct {
	n atomic.Uint32
	_ [124]byte
}

var sampleCounts struct {
	_ [128]byte // off the line of whatever the linker puts before
	c [sampleStripes]sampleCount
}

// untimed is what sampleStart returns for a call it does not time.
const untimed = -1

// sampleStart is the start of a call: whether this goroutine times it, those whose number in
// its stripe is a multiple of sampleEvery, the first among them. It returns the call's start
// on the clock of elapsed (never negative), or untimed; an untimed call reads no clock.
func (k *core) sampleStart(hint uintptr) int64 {
	every := k.sampleEvery
	if every == 0 {
		return untimed
	}
	n := sampleCounts.c[(hash(hint)>>32)%sampleStripes].n.Add(1) - 1
	if k.samplePow2 {
		if n&(every-1) != 0 {
			return untimed
		}
	} else if n%every != 0 {
		return untimed
	}
	return k.clk.elapsed()
}

func (k *core) shard(hint uintptr) *histShard {
	return &k.hists[(hash(hint)>>32)%histShards]
}

func since(t0, t1 int64) uint64 { return uint64(max(t1-t0, 0)) }

// timedModel records a timed call the model answered: its end-to-end time.
func (k *core) timedModel(hint uintptr, t0 int64) {
	k.shard(hint).h[hModel].record(since(t0, k.clk.elapsed()))
}

// timedFallback records, after the fallback function of a timed call that started at t0 and
// whose fallback started at t1, the fallback's time and the call's end to end.
func (k *core) timedFallback(hint uintptr, t0, t1 int64) {
	t2 := k.clk.elapsed()
	s := k.shard(hint)
	s.h[hFallbackFn].record(since(t1, t2))
	s.h[hFallback].record(since(t0, t2))
}

// Histogram is one sampled histogram as read (sdk-contract §8): its bucket counts, cumulative
// as the Prometheus format has them, its sum in seconds, and its count.
type Histogram struct {
	// Cumulative[i] counts the durations of at most BucketsSeconds()[i]; the last, +Inf,
	// every duration, so it equals Count.
	Cumulative [17]uint64
	SumSeconds float64
	Count      uint64
}

// sumHist sums histogram which of every shard.
func sumHist(shards []histShard, which int) Histogram {
	var raw [17]uint64
	var sum uint64
	for s := range shards {
		h := &shards[s].h[which]
		for i := range raw {
			raw[i] += h.buckets[i].Load()
		}
		sum += h.sumNs.Load()
	}
	var out Histogram
	var acc uint64
	for i, r := range raw {
		acc += r
		out.Cumulative[i] = acc
	}
	out.SumSeconds, out.Count = float64(sum)/1e9, acc
	return out
}
