package shm

import "github.com/PromptCultivator/sidecar-zero/sdks/go/internal/abi"

// RegistryOff is where the supervisor places the registry in the control segment.
const RegistryOff = abi.CtlHeaderLen

// ModelBlock is the control header's model block (shm-abi §3).
type ModelBlock struct {
	Sha256            [32]byte
	ModelGen          uint32
	RegistryEpoch     uint32
	SchemaFingerprint uint64
	RegistryLen       uint32
}

// ReadModel reads the model block and a copy of the registry under the model seqlock, or
// fails if the writer stayed busy for attempts tries or the block names a registry outside
// the segment. A reader that gives up stays unbound and tries again later. Background only:
// it allocates the copy.
func ReadModel(ctl abi.Seg, attempts int) (ModelBlock, []byte, bool) {
	for i := 0; i < attempts; i++ {
		s1 := ctl.ControlHeaderModelSeq(0)
		if s1&1 == 1 {
			CPURelax()
			continue
		}
		b := ModelBlock{
			Sha256:            ctl.ControlHeaderModelSha256(0),
			ModelGen:          ctl.ControlHeaderModelGen(0),
			RegistryEpoch:     ctl.ControlHeaderRegistryEpoch(0),
			SchemaFingerprint: ctl.ControlHeaderSchemaFingerprint(0),
			RegistryLen:       ctl.ControlHeaderRegistryLen(0),
		}
		off := uintptr(ctl.ControlHeaderRegistryOff(0))
		n := uintptr(b.RegistryLen)
		fits := n == 0 || (off == RegistryOff && n <= abi.CtlRegistryLen && off+n <= ctl.Len())
		var bytes []byte
		if fits && n > 0 {
			words := (n + 7) / 8
			bytes = make([]byte, words*8)
			for w := uintptr(0); w < words; w++ {
				v := ctl.Word64(off + w*8)
				for j := uintptr(0); j < 8; j++ {
					bytes[w*8+j] = byte(v >> (8 * j))
				}
			}
			bytes = bytes[:n]
		}
		if ctl.ControlHeaderModelSeq(0) == s1 {
			return b, bytes, fits
		}
	}
	return ModelBlock{}, nil, false
}

// ModelEpoch is the registry epoch alone, under the seqlock: what a client re-reads when the
// gate shows a worker generation its binding was not confirmed under (arrow-data-plane §9).
func ModelEpoch(ctl abi.Seg, attempts int) (uint32, bool) {
	for i := 0; i < attempts; i++ {
		s1 := ctl.ControlHeaderModelSeq(0)
		if s1&1 == 0 {
			e := ctl.ControlHeaderRegistryEpoch(0)
			if ctl.ControlHeaderModelSeq(0) == s1 {
				return e, true
			}
		}
		CPURelax()
	}
	return 0, false
}
