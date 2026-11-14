// The worker segment as the design specifies it (shm-abi §2), plus counters for the spike.
#include <stdint.h>

#define MAX_CELLS 8
#define MAX_RUNTIME 8
#define CELL_SIZE 64

#ifndef FUTEX_WAITERS
#define FUTEX_WAITERS 0x80000000u
#endif
#ifndef FUTEX_OWNER_DIED
#define FUTEX_OWNER_DIED 0x40000000u
#endif
#ifndef FUTEX_TID_MASK
#define FUTEX_TID_MASK 0x3fffffffu
#endif

struct segment {
    // death_lock[8]: word 0 of each cell is the futex word; the rest is the worker's C library's.
    struct { _Alignas(64) unsigned char bytes[CELL_SIZE]; } cell[MAX_CELLS];
    // Spike-only fields below this line.
    _Alignas(64) uint32_t locked;     // consume threads that hold their lock
    uint32_t forge_now;               // set by the supervisor to trigger a forged report
    uint32_t crash;                   // set by the supervisor: 1 segfault in consume thread 0,
                                      // 2 segfault in runtime thread 0, 3 abort() in consume thread 0
    int32_t tid[MAX_CELLS];           // each consume thread's kernel thread id
    uint32_t worker_libc_major;
    char worker_libc[32];
    struct { _Alignas(64) uint64_t v; } counter[MAX_CELLS + MAX_RUNTIME]; // consume, then runtime
};
