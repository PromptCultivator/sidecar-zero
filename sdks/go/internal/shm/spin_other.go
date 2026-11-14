//go:build !amd64 && !arm64

package shm

// CPURelax is one turn of a spin loop. No hint on this architecture.
func CPURelax() {}
