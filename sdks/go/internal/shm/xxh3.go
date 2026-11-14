package shm

import (
	"encoding/binary"
	"math/bits"
)

// XXH3 is XXH3-64 with seed 0 and the default secret, written from the xxHash specification
// (the schema fingerprint of arrow-data-plane §9). Checked against abi/golden/fingerprints.txt
// and against vectors for every length up to 2,048 bytes (testdata/xxh3.txt).
func XXH3(b []byte) uint64 {
	n := len(b)
	switch {
	case n <= 16:
		return xxh3Len0To16(b)
	case n <= 128:
		return xxh3Len17To128(b)
	case n <= 240:
		return xxh3Len129To240(b)
	default:
		return xxh3Long(b)
	}
}

const (
	prime32_1 = 0x9E3779B1
	prime32_2 = 0x85EBCA77
	prime32_3 = 0xC2B2AE3D
	prime64_1 = 0x9E3779B185EBCA87
	prime64_2 = 0xC2B2AE3D27D4EB4F
	prime64_3 = 0x165667B19E3779F9
	prime64_4 = 0x85EBCA77C2B2AE63
	prime64_5 = 0x27D4EB2F165667C5
	primeMx1  = 0x165667919E3779F9
	primeMx2  = 0x9FB21C651E98DF25
)

var kSecret = [192]byte{
	0xb8, 0xfe, 0x6c, 0x39, 0x23, 0xa4, 0x4b, 0xbe, 0x7c, 0x01, 0x81, 0x2c, 0xf7, 0x21, 0xad, 0x1c,
	0xde, 0xd4, 0x6d, 0xe9, 0x83, 0x90, 0x97, 0xdb, 0x72, 0x40, 0xa4, 0xa4, 0xb7, 0xb3, 0x67, 0x1f,
	0xcb, 0x79, 0xe6, 0x4e, 0xcc, 0xc0, 0xe5, 0x78, 0x82, 0x5a, 0xd0, 0x7d, 0xcc, 0xff, 0x72, 0x21,
	0xb8, 0x08, 0x46, 0x74, 0xf7, 0x43, 0x24, 0x8e, 0xe0, 0x35, 0x90, 0xe6, 0x81, 0x3a, 0x26, 0x4c,
	0x3c, 0x28, 0x52, 0xbb, 0x91, 0xc3, 0x00, 0xcb, 0x88, 0xd0, 0x65, 0x8b, 0x1b, 0x53, 0x2e, 0xa3,
	0x71, 0x64, 0x48, 0x97, 0xa2, 0x0d, 0xf9, 0x4e, 0x38, 0x19, 0xef, 0x46, 0xa9, 0xde, 0xac, 0xd8,
	0xa8, 0xfa, 0x76, 0x3f, 0xe3, 0x9c, 0x34, 0x3f, 0xf9, 0xdc, 0xbb, 0xc7, 0xc7, 0x0b, 0x4f, 0x1d,
	0x8a, 0x51, 0xe0, 0x4b, 0xcd, 0xb4, 0x59, 0x31, 0xc8, 0x9f, 0x7e, 0xc9, 0xd9, 0x78, 0x73, 0x64,
	0xea, 0xc5, 0xac, 0x83, 0x34, 0xd3, 0xeb, 0xc3, 0xc5, 0x81, 0xa0, 0xff, 0xfa, 0x13, 0x63, 0xeb,
	0x17, 0x0d, 0xdd, 0x51, 0xb7, 0xf0, 0xda, 0x49, 0xd3, 0x16, 0x55, 0x26, 0x29, 0xd4, 0x68, 0x9e,
	0x2b, 0x16, 0xbe, 0x58, 0x7d, 0x47, 0xa1, 0xfc, 0x8f, 0xf8, 0xb8, 0xd1, 0x7a, 0xd0, 0x31, 0xce,
	0x45, 0xcb, 0x3a, 0x8f, 0x95, 0x16, 0x04, 0x28, 0xaf, 0xd7, 0xfb, 0xca, 0xbb, 0x4b, 0x40, 0x7e,
}

func r64(b []byte, i int) uint64 { return binary.LittleEndian.Uint64(b[i:]) }
func r32(b []byte, i int) uint32 { return binary.LittleEndian.Uint32(b[i:]) }

func mulFold64(a, b uint64) uint64 {
	hi, lo := bits.Mul64(a, b)
	return hi ^ lo
}

func xxh64Avalanche(h uint64) uint64 {
	h ^= h >> 33
	h *= prime64_2
	h ^= h >> 29
	h *= prime64_3
	h ^= h >> 32
	return h
}

func xxh3Avalanche(h uint64) uint64 {
	h ^= h >> 37
	h *= primeMx1
	h ^= h >> 32
	return h
}

func rrmxmx(h uint64, n uint64) uint64 {
	h ^= bits.RotateLeft64(h, 49) ^ bits.RotateLeft64(h, 24)
	h *= primeMx2
	h ^= (h >> 35) + n
	h *= primeMx2
	h ^= h >> 28
	return h
}

func xxh3Len0To16(b []byte) uint64 {
	s := kSecret[:]
	n := len(b)
	switch {
	case n > 8:
		bitflip1 := r64(s, 24) ^ r64(s, 32)
		bitflip2 := r64(s, 40) ^ r64(s, 48)
		lo := r64(b, 0) ^ bitflip1
		hi := r64(b, n-8) ^ bitflip2
		acc := uint64(n) + bits.ReverseBytes64(lo) + hi + mulFold64(lo, hi)
		return xxh3Avalanche(acc)
	case n >= 4:
		in1 := r32(b, 0)
		in2 := r32(b, n-4)
		bitflip := r64(s, 8) ^ r64(s, 16)
		in64 := uint64(in2) + uint64(in1)<<32
		return rrmxmx(in64^bitflip, uint64(n))
	case n > 0:
		c1, c2, c3 := uint32(b[0]), uint32(b[n>>1]), uint32(b[n-1])
		combined := c1<<16 | c2<<24 | c3 | uint32(n)<<8
		bitflip := uint64(r32(s, 0) ^ r32(s, 4))
		return xxh64Avalanche(uint64(combined) ^ bitflip)
	default:
		return xxh64Avalanche(r64(s, 56) ^ r64(s, 64))
	}
}

func mix16(b []byte, i int, s []byte, j int) uint64 {
	return mulFold64(r64(b, i)^r64(s, j), r64(b, i+8)^r64(s, j+8))
}

func xxh3Len17To128(b []byte) uint64 {
	s := kSecret[:]
	n := len(b)
	acc := uint64(n) * prime64_1
	if n > 32 {
		if n > 64 {
			if n > 96 {
				acc += mix16(b, 48, s, 96)
				acc += mix16(b, n-64, s, 112)
			}
			acc += mix16(b, 32, s, 64)
			acc += mix16(b, n-48, s, 80)
		}
		acc += mix16(b, 16, s, 32)
		acc += mix16(b, n-32, s, 48)
	}
	acc += mix16(b, 0, s, 0)
	acc += mix16(b, n-16, s, 16)
	return xxh3Avalanche(acc)
}

func xxh3Len129To240(b []byte) uint64 {
	s := kSecret[:]
	n := len(b)
	acc := uint64(n) * prime64_1
	rounds := n / 16
	for i := 0; i < 8; i++ {
		acc += mix16(b, 16*i, s, 16*i)
	}
	acc = xxh3Avalanche(acc)
	for i := 8; i < rounds; i++ {
		acc += mix16(b, 16*i, s, 16*(i-8)+3)
	}
	acc += mix16(b, n-16, s, 136-17)
	return xxh3Avalanche(acc)
}

func accumulate512(acc *[8]uint64, b []byte, i int, s []byte, j int) {
	for k := 0; k < 8; k++ {
		v := r64(b, i+8*k)
		key := v ^ r64(s, j+8*k)
		acc[k^1] += v
		acc[k] += uint64(uint32(key)) * (key >> 32)
	}
}

func xxh3Long(b []byte) uint64 {
	s := kSecret[:]
	n := len(b)
	acc := [8]uint64{prime32_3, prime64_1, prime64_2, prime64_3, prime64_4, prime32_2, prime64_5, prime32_1}
	const stripe = 64
	stripesPerBlock := (len(s) - stripe) / 8
	blockLen := stripe * stripesPerBlock
	blocks := (n - 1) / blockLen
	for blk := 0; blk < blocks; blk++ {
		for st := 0; st < stripesPerBlock; st++ {
			accumulate512(&acc, b, blk*blockLen+st*stripe, s, st*8)
		}
		for k := 0; k < 8; k++ {
			a := acc[k]
			a ^= a >> 47
			a ^= r64(s, len(s)-stripe+8*k)
			a *= prime32_1
			acc[k] = a
		}
	}
	last := ((n - 1) - blockLen*blocks) / stripe
	for st := 0; st < last; st++ {
		accumulate512(&acc, b, blocks*blockLen+st*stripe, s, st*8)
	}
	accumulate512(&acc, b, n-stripe, s, len(s)-stripe-7)
	result := uint64(n) * prime64_1
	for k := 0; k < 4; k++ {
		result += mulFold64(acc[2*k]^r64(s, 11+16*k), acc[2*k+1]^r64(s, 11+16*k+8))
	}
	return xxh3Avalanche(result)
}
