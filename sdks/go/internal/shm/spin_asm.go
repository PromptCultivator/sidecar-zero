//go:build amd64 || arm64

package shm

// CPURelax is one turn of a spin loop: PAUSE on amd64, ISB on arm64.
//
//go:noescape
func CPURelax()
