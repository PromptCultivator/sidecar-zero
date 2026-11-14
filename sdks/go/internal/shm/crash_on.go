//go:build crashpoints && linux

package shm

import (
	"os"
	"syscall"
)

// CrashPoints reports whether this binary was built with client-side crash points.
const CrashPoints = true

var crashAt = os.Getenv("SCZ_CRASH_AT")

// crashPoint aborts the process with SIGKILL when SCZ_CRASH_AT names tag (testing §2.4).
func crashPoint(tag string) {
	if crashAt == tag {
		_ = syscall.Kill(os.Getpid(), syscall.SIGKILL)
	}
}
