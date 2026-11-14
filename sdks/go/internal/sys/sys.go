// Package sys holds the few system calls the SDK makes outside the arena that need an
// unsafe.Pointer: reading CLOCK_MONOTONIC directly, and the stack-address hint that spreads
// goroutines over the caller table. With internal/abi, it is the only package that imports
// unsafe (lint_test.go).
package sys

import "unsafe"

// StackHint is an address on the calling goroutine's stack. Goroutines run on different
// stacks, so it differs between them (sdk-go §6: an entry is found "starting at an index
// derived from the address of a local variable").
func StackHint() uintptr {
	var x byte
	return uintptr(unsafe.Pointer(&x))
}
