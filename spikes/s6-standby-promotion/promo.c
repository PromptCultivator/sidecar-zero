// S6 spike: how long from kill -9 of the active worker to the gate reading HEALTHY again
// on a warm standby, and where does that time go?
//
// It times four moments after the kill:
//   robust   a watcher blocked on a robust lock the worker held is told the owner died
//   pidfd    the worker's pidfd becomes readable (the signal the design uses today)
//   swept    the supervisor has flipped the gate and failed the worker's slots
//   healthy  the standby has reported ACTIVE and the gate reads HEALTHY
//
// The question behind "robust": the kernel reports an exit through the pidfd only after
// it has freed the process's memory. A robust futex is handled earlier in the exit path,
// so it may tell us sooner, and the gap should grow with the worker's memory.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define SLOTS 1088 // the default arena: 1,024 small slots and 64 batch slots
enum { FREE, READY, PROCESSING, FAILED };
enum { G_HEALTHY = 1, G_RELOADING = 3 };

struct shm {
    _Atomic uint64_t gate;           // [generation:32 | state:8]
    _Atomic uint32_t standby_wait;   // event counter the standby sleeps on
    _Atomic uint64_t standby_active; // when the standby saw its generation, by its own clock
    pthread_mutex_t robust;          // process-shared robust lock, held by the active worker
    _Atomic uint64_t slot[SLOTS];    // [owner:8 | state:8]
};
static struct shm *S;

static uint64_t now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec; }
static void nap_ms(int ms) { struct timespec t = {ms / 1000, (ms % 1000) * 1000000L}; nanosleep(&t, NULL); }
static void *idle_thread(void *a) { (void)a; for (;;) pause(); return NULL; }

// ── the active worker: holds memory, holds the robust lock, holds some slots ──
static void worker_active(int ready_fd, size_t mb, int threads, int shared_fd) {
    pthread_mutex_lock(&S->robust);
    size_t len = mb << 20;
    if (len) {
        unsigned char *p;
        if (shared_fd >= 0) { // weights as a shared file mapping: read, never copied
            p = mmap(NULL, len, PROT_READ, MAP_SHARED, shared_fd, 0);
            volatile unsigned char sink = 0;
            for (size_t i = 0; i < len; i += 4096) sink += p[i];
        } else { // private memory: what a runtime's heap looks like
            p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
            for (size_t i = 0; i < len; i += 4096) p[i] = 1;
        }
    }
    for (int i = 1; i < threads; i++) { pthread_t t; pthread_create(&t, NULL, idle_thread, NULL); }
    for (int i = 0; i < 32; i++) atomic_store(&S->slot[i], (1ull << 8) | PROCESSING);
    for (int i = 32; i < 48; i++) atomic_store(&S->slot[i], READY);
    (void)!write(ready_fd, "r", 1);
    for (;;) pause();
}

// ── the warm standby: asleep until the gate carries its generation ──
static void worker_standby(int status_fd, uint32_t my_gen) {
    for (;;) {
        uint32_t v = atomic_load(&S->standby_wait);
        if ((uint32_t)(atomic_load(&S->gate) >> 32) == my_gen) break;
        syscall(SYS_futex, &S->standby_wait, FUTEX_WAIT, v, NULL, NULL, 0);
    }
    atomic_store(&S->standby_active, now_ns());
    (void)!write(status_fd, "A", 1);
    for (;;) pause();
}

// ── a watcher thread in the supervisor, blocked on the worker's robust lock ──
static sem_t arm, fired;
static _Atomic uint64_t t_robust;
static _Atomic int robust_result;
static void *watcher(void *a) {
    (void)a;
    for (;;) {
        sem_wait(&arm);
        int r = pthread_mutex_lock(&S->robust);
        atomic_store(&t_robust, now_ns());
        atomic_store(&robust_result, r);
        if (r == EOWNERDEAD) pthread_mutex_consistent(&S->robust);
        pthread_mutex_unlock(&S->robust);
        sem_post(&fired);
    }
    return NULL;
}

struct sample { double robust, pidfd, swept, healthy; };

static int trial(size_t mb, int threads, int shared_fd, uint32_t gen, struct sample *out) {
    int ready[2], status[2];
    if (pipe(ready) || pipe(status)) return -1;
    for (int i = 0; i < SLOTS; i++) atomic_store(&S->slot[i], FREE);
    atomic_store(&S->gate, ((uint64_t)gen << 32) | G_HEALTHY);
    atomic_store(&S->standby_active, 0);

    pid_t a = fork();
    if (a == 0) { close(ready[0]); worker_active(ready[1], mb, threads, shared_fd); _exit(0); }
    char c;
    if (read(ready[0], &c, 1) != 1) return -1;
    pid_t b = fork();
    if (b == 0) { close(status[0]); worker_standby(status[1], gen + 1); _exit(0); }

    int pidfd = syscall(SYS_pidfd_open, a, 0);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN, .data.fd = pidfd};
    epoll_ctl(ep, EPOLL_CTL_ADD, pidfd, &ev);
    sem_post(&arm); // the watcher now blocks on the lock the worker holds
    nap_ms(30);     // let the standby park and the watcher reach the kernel

    uint64_t t_kill = now_ns();
    kill(a, SIGKILL);

    // 1. Today's signal: the pidfd.
    epoll_wait(ep, &ev, 1, -1);
    uint64_t t_pidfd = now_ns();

    // 2. Flip the gate, then sweep: fail what the dead worker held and what was queued.
    atomic_store(&S->gate, ((uint64_t)gen << 32) | G_RELOADING);
    int failed = 0;
    for (int i = 0; i < SLOTS; i++) {
        uint64_t w = atomic_load(&S->slot[i]);
        if ((w & 0xff) == PROCESSING || (w & 0xff) == READY) { atomic_store(&S->slot[i], FAILED); failed++; }
    }
    uint64_t t_swept = now_ns();

    // 3. Promote: new generation in the gate, bump the counter, wake the standby.
    atomic_store(&S->gate, ((uint64_t)(gen + 1) << 32) | G_RELOADING);
    atomic_fetch_add(&S->standby_wait, 1);
    syscall(SYS_futex, &S->standby_wait, FUTEX_WAKE, 0x7fffffff, NULL, NULL, 0);
    struct epoll_event ev2 = {.events = EPOLLIN, .data.fd = status[0]};
    epoll_ctl(ep, EPOLL_CTL_ADD, status[0], &ev2);
    for (;;) { // wait for the standby's ACTIVE report
        struct epoll_event got;
        epoll_wait(ep, &got, 1, -1);
        if (got.data.fd == status[0]) break;
    }
    atomic_store(&S->gate, ((uint64_t)(gen + 1) << 32) | G_HEALTHY);
    uint64_t t_healthy = now_ns();

    sem_wait(&fired);
    out->robust = (atomic_load(&t_robust) - (double)t_kill) / 1e6;
    out->pidfd = (t_pidfd - (double)t_kill) / 1e6;
    out->swept = (t_swept - (double)t_kill) / 1e6;
    out->healthy = (t_healthy - (double)t_kill) / 1e6;
    int ok = failed == 48 && atomic_load(&robust_result) == EOWNERDEAD;

    kill(b, SIGKILL);
    waitpid(a, NULL, 0);
    waitpid(b, NULL, 0);
    close(pidfd); close(ep); close(ready[0]); close(ready[1]); close(status[0]); close(status[1]);
    return ok ? 0 : -2;
}

static int cmp(const void *x, const void *y) { double a = *(const double *)x, b = *(const double *)y; return (a > b) - (a < b); }
static void stats(double *v, int n, double *med, double *p90, double *max) {
    qsort(v, n, sizeof *v, cmp);
    *med = v[n / 2]; *p90 = v[(n * 9) / 10 < n ? (n * 9) / 10 : n - 1]; *max = v[n - 1];
}

int main(int argc, char **argv) {
    int trials = argc > 1 ? atoi(argv[1]) : 20;
    S = mmap(NULL, sizeof *S, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    pthread_mutexattr_t at;
    pthread_mutexattr_init(&at);
    pthread_mutexattr_setpshared(&at, PTHREAD_PROCESS_SHARED);
    pthread_mutexattr_setrobust(&at, PTHREAD_MUTEX_ROBUST);
    pthread_mutex_init(&S->robust, &at);
    sem_init(&arm, 0, 0);
    sem_init(&fired, 0, 0);
    pthread_t w;
    pthread_create(&w, NULL, watcher, NULL);

    // A file of "weights" for the shared-mapping case, populated once.
    size_t shared_mb = 1024;
    int sfd = syscall(SYS_memfd_create, "weights", 0);
    if (ftruncate(sfd, shared_mb << 20)) perror("ftruncate");
    unsigned char *sp = mmap(NULL, shared_mb << 20, PROT_READ | PROT_WRITE, MAP_SHARED, sfd, 0);
    for (size_t i = 0; i < shared_mb << 20; i += 4096) sp[i] = 1;

    struct { size_t mb; int threads; int shared; } cases[] = {
        {0, 1, 0}, {16, 1, 0}, {128, 1, 0}, {512, 1, 0}, {1024, 1, 0}, {2048, 1, 0},
        {1024, 8, 0}, {1024, 1, 1},
    };
    printf("All times are milliseconds after the kill; %d kills per row. median / p90 / max\n\n", trials);
    printf("%-34s %-22s %-22s %-22s %-22s\n", "worker", "robust lock reports", "pidfd readable", "gate flipped + swept", "standby HEALTHY");
    uint32_t gen = 1;
    for (size_t ci = 0; ci < sizeof cases / sizeof cases[0]; ci++) {
        double r[256], p[256], s[256], h[256];
        int n = 0, bad = 0;
        for (int t = 0; t < trials && n < 256; t++, gen += 2) {
            struct sample sm;
            int rc = trial(cases[ci].mb, cases[ci].threads, cases[ci].shared ? sfd : -1, gen, &sm);
            if (rc == -2) bad++;
            if (rc == -1) continue;
            r[n] = sm.robust; p[n] = sm.pidfd; s[n] = sm.swept; h[n] = sm.healthy; n++;
        }
        char name[64];
        snprintf(name, sizeof name, "%4zu MiB %s, %d thread%s", cases[ci].mb, cases[ci].shared ? "shared file" : "private", cases[ci].threads,
                 cases[ci].threads > 1 ? "s" : "");
        double m1, q1, x1, m2, q2, x2, m3, q3, x3, m4, q4, x4;
        stats(r, n, &m1, &q1, &x1); stats(p, n, &m2, &q2, &x2); stats(s, n, &m3, &q3, &x3); stats(h, n, &m4, &q4, &x4);
        printf("%-34s %6.3f/%6.3f/%7.3f  %6.3f/%6.3f/%7.3f  %6.3f/%6.3f/%7.3f  %6.3f/%6.3f/%7.3f%s\n", name, m1, q1, x1, m2, q2, x2, m3, q3, x3, m4,
               q4, x4, bad ? "   (some trials failed their checks)" : "");
        fflush(stdout);
    }
    return 0;
}
