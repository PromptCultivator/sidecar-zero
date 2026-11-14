// S6b worker, built against glibc. Plays a worker with several consume threads.
//
//   worker <segment fd> <ready fd> <consume threads> <runtime threads> <private MiB> <mode>
//
// Each consume thread takes its own robust, process-shared mutex in the worker segment
// and holds it until it dies, incrementing a counter of its own the whole time. "Runtime"
// threads, standing in for an inference runtime's thread pool, increment counters too
// but hold no lock. The supervisor watches the locks' words and compares the counters.
//
// mode "normal": just run until killed.
// mode "forge":  consume thread 0 sets the owner-died bit on its own live lock and wakes
//                the watcher, then keeps running. This is a worker lying about a death.
#define _GNU_SOURCE
#include <gnu/libc-version.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#include "segment.h"

_Static_assert(sizeof(pthread_mutex_t) <= CELL_SIZE, "a mutex must fit in a 64-byte cell");
_Static_assert(offsetof(pthread_mutex_t, __data.__lock) == 0, "the futex word must be word 0 of the cell");

static struct segment *seg;
static int forge, sleepy;

// A crash from inside the process, the way a model runtime would crash.
static void maybe_crash(int consume_thread, int index) {
    uint32_t c = __atomic_load_n(&seg->crash, __ATOMIC_RELAXED);
    if (!c || index != 0) return;
    if ((c == 1 && consume_thread) || (c == 2 && !consume_thread)) *(volatile int *)0 = 1;
    if (c == 3 && consume_thread) abort();
}

static void *consume(void *arg) {
    int i = (int)(intptr_t)arg;
    pthread_mutex_t *m = (pthread_mutex_t *)seg->cell[i].bytes;
    if (pthread_mutex_lock(m) != 0) abort();
    seg->tid[i] = gettid();
    __atomic_fetch_add(&seg->locked, 1, __ATOMIC_SEQ_CST);
    if (forge && i == 0) {
        while (__atomic_load_n(&seg->forge_now, __ATOMIC_SEQ_CST) == 0) ;
        uint32_t *w = (uint32_t *)seg->cell[0].bytes;
        __atomic_fetch_or(w, FUTEX_OWNER_DIED, __ATOMIC_SEQ_CST);
        syscall(SYS_futex, w, FUTEX_WAKE, 1, NULL, NULL, 0);
    }
    if (sleepy) for (;;) pause(); // a parked standby's consume thread
    for (;;) { __atomic_fetch_add(&seg->counter[i].v, 1, __ATOMIC_RELAXED); maybe_crash(1, i); }
    return NULL;
}

static void *runtime(void *arg) {
    int j = (int)(intptr_t)arg;
    for (;;) { __atomic_fetch_add(&seg->counter[MAX_CELLS + j].v, 1, __ATOMIC_RELAXED); maybe_crash(0, j); }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 7 && argc != 8) { fprintf(stderr, "usage: worker fd readyfd consume runtime mib mode [spin|sleep]\n"); return 2; }
    sleepy = argc == 8 && strcmp(argv[7], "sleep") == 0;
    int fd = atoi(argv[1]), ready = atoi(argv[2]), nc = atoi(argv[3]), nr = atoi(argv[4]);
    size_t mib = strtoul(argv[5], NULL, 10);
    forge = strcmp(argv[6], "forge") == 0;
    seg = mmap(NULL, sizeof *seg, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (seg == MAP_FAILED) { perror("mmap"); return 2; }
    seg->worker_libc_major = 0;
    snprintf(seg->worker_libc, sizeof seg->worker_libc, "glibc %s", gnu_get_libc_version());

    if (mib) { // private memory, as a runtime's heap and weights would be
        unsigned char *p = mmap(NULL, mib << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { perror("mmap private"); return 2; }
        for (size_t o = 0; o < (mib << 20); o += 4096) p[o] = 1;
    }

    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    pthread_mutexattr_setpshared(&a, PTHREAD_PROCESS_SHARED);
    pthread_mutexattr_setrobust(&a, PTHREAD_MUTEX_ROBUST);
    for (int i = 0; i < nc; i++) pthread_mutex_init((pthread_mutex_t *)seg->cell[i].bytes, &a);

    pthread_t t;
    for (int i = 0; i < nc; i++) pthread_create(&t, NULL, consume, (void *)(intptr_t)i);
    for (int j = 0; j < nr; j++) pthread_create(&t, NULL, runtime, (void *)(intptr_t)j);
    while (__atomic_load_n(&seg->locked, __ATOMIC_SEQ_CST) < (uint32_t)nc) usleep(100);
    if (write(ready, "R", 1) != 1) return 2; // "WARM": every consume thread holds its lock
    for (;;) pause();
}
