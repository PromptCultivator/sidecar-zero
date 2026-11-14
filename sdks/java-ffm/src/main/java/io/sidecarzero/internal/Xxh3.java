package io.sidecarzero.internal;

/**
 * XXH3-64 with seed 0 and the default secret: the schema fingerprint's hash (arrow-data-plane §9).
 * A direct transcription of the reference algorithm (xxHash 0.8), scalar, for every input length.
 * Checked against {@code abi/golden/fingerprints.txt} and longer vectors in the SDK's tests.
 */
public final class Xxh3 {
    private Xxh3() {}

    private static final long P32_1 = 0x9E3779B1L, P32_2 = 0x85EBCA77L, P32_3 = 0xC2B2AE3DL;
    private static final long P64_1 = 0x9E3779B185EBCA87L, P64_2 = 0xC2B2AE3D27D4EB4FL, P64_3 = 0x165667B19E3779F9L;
    private static final long P64_4 = 0x85EBCA77C2B2AE63L, P64_5 = 0x27D4EB2F165667C5L;
    private static final long PMX1 = 0x165667919E3779F9L, PMX2 = 0x9FB21C651E98DF25L;

    private static final byte[] SECRET = bytes(
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
            0x45, 0xcb, 0x3a, 0x8f, 0x95, 0x16, 0x04, 0x28, 0xaf, 0xd7, 0xfb, 0xca, 0xbb, 0x4b, 0x40, 0x7e);

    private static byte[] bytes(int... v) {
        byte[] b = new byte[v.length];
        for (int i = 0; i < v.length; i++) b[i] = (byte) v[i];
        return b;
    }

    private static long r64(byte[] b, int i) {
        return (b[i] & 0xFFL) | (b[i + 1] & 0xFFL) << 8 | (b[i + 2] & 0xFFL) << 16 | (b[i + 3] & 0xFFL) << 24
                | (b[i + 4] & 0xFFL) << 32 | (b[i + 5] & 0xFFL) << 40 | (b[i + 6] & 0xFFL) << 48 | (b[i + 7] & 0xFFL) << 56;
    }

    private static long r32(byte[] b, int i) {
        return (b[i] & 0xFFL) | (b[i + 1] & 0xFFL) << 8 | (b[i + 2] & 0xFFL) << 16 | (b[i + 3] & 0xFFL) << 24;
    }

    private static long fold(long a, long b) {
        return (a * b) ^ Math.unsignedMultiplyHigh(a, b);
    }

    private static long avalanche(long h) {
        h ^= h >>> 37;
        h *= PMX1;
        return h ^ (h >>> 32);
    }

    private static long avalanche64(long h) {
        h ^= h >>> 33;
        h *= P64_2;
        h ^= h >>> 29;
        h *= P64_3;
        return h ^ (h >>> 32);
    }

    private static long rrmxmx(long h, long len) {
        h ^= Long.rotateLeft(h, 49) ^ Long.rotateLeft(h, 24);
        h *= PMX2;
        h ^= (h >>> 35) + len;
        h *= PMX2;
        return h ^ (h >>> 28);
    }

    private static long mix16(byte[] in, int i, int s) {
        return fold(r64(in, i) ^ r64(SECRET, s), r64(in, i + 8) ^ r64(SECRET, s + 8));
    }

    public static long hash(byte[] in) {
        int len = in.length;
        if (len == 0) return avalanche64(r64(SECRET, 56) ^ r64(SECRET, 64));
        if (len <= 3) {
            long c1 = in[0] & 0xFF, c2 = in[len >> 1] & 0xFF, c3 = in[len - 1] & 0xFF;
            long combined = (c1 << 16) | (c2 << 24) | c3 | ((long) len << 8);
            long bitflip = (r32(SECRET, 0) ^ r32(SECRET, 4));
            return avalanche64(combined ^ bitflip);
        }
        if (len <= 8) {
            long in1 = r32(in, 0), in2 = r32(in, len - 4);
            long bitflip = r64(SECRET, 8) ^ r64(SECRET, 16);
            long in64 = in2 + (in1 << 32);
            return rrmxmx(in64 ^ bitflip, len);
        }
        if (len <= 16) {
            long bf1 = r64(SECRET, 24) ^ r64(SECRET, 32);
            long bf2 = r64(SECRET, 40) ^ r64(SECRET, 48);
            long lo = r64(in, 0) ^ bf1;
            long hi = r64(in, len - 8) ^ bf2;
            long acc = len + Long.reverseBytes(lo) + hi + fold(lo, hi);
            return avalanche(acc);
        }
        if (len <= 128) {
            long acc = len * P64_1;
            if (len > 32) {
                if (len > 64) {
                    if (len > 96) {
                        acc += mix16(in, 48, 96);
                        acc += mix16(in, len - 64, 112);
                    }
                    acc += mix16(in, 32, 64);
                    acc += mix16(in, len - 48, 80);
                }
                acc += mix16(in, 16, 32);
                acc += mix16(in, len - 32, 48);
            }
            acc += mix16(in, 0, 0);
            acc += mix16(in, len - 16, 16);
            return avalanche(acc);
        }
        if (len <= 240) {
            long acc = len * P64_1;
            int rounds = len / 16;
            for (int i = 0; i < 8; i++) acc += mix16(in, 16 * i, 16 * i);
            acc = avalanche(acc);
            for (int i = 8; i < rounds; i++) acc += mix16(in, 16 * i, 16 * (i - 8) + 3);
            acc += mix16(in, len - 16, 136 - 17);
            return avalanche(acc);
        }
        return hashLong(in);
    }

    private static void accumulate512(long[] acc, byte[] in, int i, int s) {
        for (int k = 0; k < 8; k++) {
            long v = r64(in, i + 8 * k);
            long key = v ^ r64(SECRET, s + 8 * k);
            acc[k ^ 1] += v;
            acc[k] += (key & 0xFFFF_FFFFL) * (key >>> 32);
        }
    }

    private static void scramble(long[] acc, int s) {
        for (int k = 0; k < 8; k++) {
            long a = acc[k];
            a ^= a >>> 47;
            a ^= r64(SECRET, s + 8 * k);
            acc[k] = a * P32_1;
        }
    }

    private static long hashLong(byte[] in) {
        int len = in.length;
        long[] acc = {P32_3, P64_1, P64_2, P64_3, P64_4, P32_2, P64_5, P32_1};
        int stripesPerBlock = (SECRET.length - 64) / 8;
        int blockLen = 64 * stripesPerBlock;
        int blocks = (len - 1) / blockLen;
        for (int n = 0; n < blocks; n++) {
            for (int s = 0; s < stripesPerBlock; s++) accumulate512(acc, in, n * blockLen + s * 64, s * 8);
            scramble(acc, SECRET.length - 64);
        }
        int stripes = ((len - 1) - blockLen * blocks) / 64;
        for (int s = 0; s < stripes; s++) accumulate512(acc, in, blocks * blockLen + s * 64, s * 8);
        accumulate512(acc, in, len - 64, SECRET.length - 64 - 7);
        long result = len * P64_1;
        for (int k = 0; k < 4; k++) result += fold(acc[2 * k] ^ r64(SECRET, 11 + 16 * k), acc[2 * k + 1] ^ r64(SECRET, 11 + 16 * k + 8));
        return avalanche(result);
    }
}
