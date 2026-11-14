//go:build !crashpoints || !linux

package shm

// CrashPoints reports whether this binary was built with client-side crash points.
const CrashPoints = false

func crashPoint(string) {}
