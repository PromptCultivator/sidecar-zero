//go:build !linux

package sys

import "time"

var start = time.Now()

// MonotonicNs off Linux: a monotonic count from process start. Nothing compares it with
// another process's clock there, because the client never connects.
func MonotonicNs() uint64 { return uint64(time.Since(start)) }
