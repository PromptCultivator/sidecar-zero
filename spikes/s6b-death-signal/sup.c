// S6b supervisor, built statically against musl. Spawns the glibc worker, watches each
// consume thread's lock with raw futex calls (it never takes the lock), kills the worker,
// and records when each lock reports and when the pidfd does.
//
//   sup <sizes in MiB, comma separated> <kills per size> <consume threads> <runtime threads>
//   sup forge
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "segment.h"

#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif

static uint64_t now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec; }
static void nap_ms(int ms) { struct timespec t = {ms / 1000, (ms % 1000) * 1000000L}; nanosleep(&t, NULL); }

static struct segment *seg;
static int efd, nconsume, nruntime;

struct report {
    uint64_t t;                     // when the watcher woke with the owner-died bit set
    uint32_t word;                  // the word it saw
    uint64_t own_counter;           // this consume thread's counter at that moment
    uint64_t runtime[MAX_RUNTIME];  // the runtime threads' counters at that moment
};
static struct report rep[MAX_CELLS];
static char worker_libc[32];

static void *watcher(void *arg) {
    int i = (int)(intptr_t)arg;
    uint32_t *w = (uint32_t *)seg->cell[i].bytes;
    for (;;) {
        uint32_t v = __atomic_load_n(w, __ATOMIC_SEQ_CST);
        if (v & FUTEX_OWNER_DIED) {
            rep[i].t = now_ns();
            rep[i].word = v;
            rep[i].own_counter = __atomic_load_n(&seg->counter[i].v, __ATOMIC_SEQ_CST);
            for (int j = 0; j < nruntime; j++) rep[i].runtime[j] = __atomic_load_n(&seg->counter[MAX_CELLS + j].v, __ATOMIC_SEQ_CST);
            uint64_t one = 1;
            if (write(efd, &one, sizeof one) != sizeof one) abort();
            return NULL;
        }
        if (!(v & FUTEX_WAITERS)) { // announce a waiter, so that the kernel wakes us
            if (!__atomic_compare_exchange_n(w, &v, v | FUTEX_WAITERS, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST)) continue;
            v |= FUTEX_WAITERS;
        }
        syscall(SYS_futex, w, FUTEX_WAIT, v, NULL, NULL, 0); // shared futex: no PRIVATE flag
    }
}

struct trial {
    double first_ms, last_ms, pidfd_ms;
    int reports, consume_moved_after_own_report;
    uint64_t runtime_steps_after_first_report;
    int ok;
};

static struct trial run_one(size_t mib, int forge, int verbose) {
    struct trial r = {0};
    memset(rep, 0, sizeof rep);
    int fd = memfd_create("scz_wrk", 0);
    size_t len = (sizeof(struct segment) + 4095) & ~4095ul;
    if (ftruncate(fd, len)) abort();
    seg = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    int ready[2];
    if (pipe(ready)) abort();
    efd = eventfd(0, EFD_CLOEXEC);

    pid_t pid = fork();
    if (pid == 0) {
        dup2(fd, 3); dup2(ready[1], 4);
        char a[5][24];
        snprintf(a[0], 24, "%d", nconsume); snprintf(a[1], 24, "%d", nruntime); snprintf(a[2], 24, "%zu", mib);
        execl("/spike/worker", "worker", "3", "4", a[0], a[1], a[2], forge ? "forge" : "normal", (char *)NULL);
        _exit(127);
    }
    close(ready[1]);
    struct pollfd p = {.fd = ready[0], .events = POLLIN};
    char c;
    if (poll(&p, 1, 60000) != 1 || read(ready[0], &c, 1) != 1) { fprintf(stderr, "worker never became ready\n"); exit(1); }

    memcpy(worker_libc, seg->worker_libc, sizeof worker_libc);
    worker_libc[sizeof worker_libc - 1] = 0;

    // Arm the watchers, as the supervisor does when a worker reports WARM.
    pthread_t th[MAX_CELLS];
    for (int i = 0; i < nconsume; i++) pthread_create(&th[i], NULL, watcher, (void *)(intptr_t)i);
    for (int armed = 0, tries = 0; armed < nconsume && tries < 10000; tries++) {
        armed = 0;
        for (int i = 0; i < nconsume; i++) armed += (__atomic_load_n((uint32_t *)seg->cell[i].bytes, __ATOMIC_SEQ_CST) & FUTEX_WAITERS) != 0;
        usleep(100);
    }
    nap_ms(20); // let every watcher reach the kernel

    int pidfd = syscall(SYS_pidfd_open, pid, 0);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event e1 = {.events = EPOLLIN, .data.u32 = 1}, e2 = {.events = EPOLLIN, .data.u32 = 2};
    epoll_ctl(ep, EPOLL_CTL_ADD, pidfd, &e1);
    epoll_ctl(ep, EPOLL_CTL_ADD, efd, &e2);

    if (forge) {
        uint64_t t0 = now_ns();
        __atomic_store_n(&seg->forge_now, 1, __ATOMIC_SEQ_CST);
        struct epoll_event got;
        epoll_wait(ep, &got, 1, 2000);
        uint64_t n;
        if (read(efd, &n, sizeof n) != sizeof n) n = 0;
        uint64_t at = rep[0].own_counter;
        nap_ms(20);
        uint64_t later = __atomic_load_n(&seg->counter[0].v, __ATOMIC_SEQ_CST);
        printf("forge: lock 0 reported owner-died %.3f ms after the worker set the bit itself; word 0x%08x\n", (rep[0].t - t0) / 1e6, rep[0].word);
        printf("forge: that consume thread's counter was %llu at the report and %llu 20 ms later: the thread is %s\n",
               (unsigned long long)at, (unsigned long long)later, later > at ? "STILL RUNNING" : "stopped");
        printf("forge: kernel-visible check, tgkill(worker, tid of thread 0, 0) = %ld (0 means the thread exists)\n",
               syscall(SYS_tgkill, pid, seg->tid[0], 0));
        r.ok = later > at;
    }

    uint64_t t_kill = now_ns();
    kill(pid, SIGKILL);
    uint64_t t_pidfd = 0;
    int reports = forge ? 1 : 0;
    while ((t_pidfd == 0 || reports < nconsume) && now_ns() - t_kill < 10000000000ull) {
        struct epoll_event got[2];
        int n = epoll_wait(ep, got, 2, 1000);
        for (int k = 0; k < n; k++) {
            if (got[k].data.u32 == 1 && !t_pidfd) { t_pidfd = now_ns(); epoll_ctl(ep, EPOLL_CTL_DEL, pidfd, NULL); }
            if (got[k].data.u32 == 2) { uint64_t m; if (read(efd, &m, sizeof m) == sizeof m) reports += (int)m; }
        }
    }
    for (int i = 0; i < nconsume; i++) pthread_join(th[i], NULL);
    waitpid(pid, NULL, 0);

    if (!forge) {
        uint64_t first = UINT64_MAX, last = 0;
        int fi = 0;
        for (int i = 0; i < nconsume; i++) {
            if (rep[i].t && rep[i].t < first) { first = rep[i].t; fi = i; }
            if (rep[i].t > last) last = rep[i].t;
            if (rep[i].t && __atomic_load_n(&seg->counter[i].v, __ATOMIC_SEQ_CST) != rep[i].own_counter) r.consume_moved_after_own_report++;
        }
        for (int j = 0; j < nruntime; j++) r.runtime_steps_after_first_report += __atomic_load_n(&seg->counter[MAX_CELLS + j].v, __ATOMIC_SEQ_CST) - rep[fi].runtime[j];
        r.reports = reports;
        r.first_ms = (first - t_kill) / 1e6;
        r.last_ms = (last - t_kill) / 1e6;
        r.pidfd_ms = (t_pidfd - t_kill) / 1e6;
        r.ok = reports == nconsume && t_pidfd != 0 && first < t_pidfd;
        if (verbose) {
            printf("  one kill in detail (%zu MiB):\n", mib);
            for (int i = 0; i < nconsume; i++)
                printf("    lock %d (tid %d): reported %.3f ms after the kill, word 0x%08x, its counter afterwards %s\n", i, seg->tid[i],
                       (rep[i].t - t_kill) / 1e6, rep[i].word, __atomic_load_n(&seg->counter[i].v, __ATOMIC_SEQ_CST) == rep[i].own_counter ? "unchanged" : "MOVED");
            printf("    pidfd readable %.3f ms after the kill; runtime threads took %llu more steps after the first report\n", r.pidfd_ms,
                   (unsigned long long)r.runtime_steps_after_first_report);
        }
    }
    close(pidfd); close(ep); close(efd); close(ready[0]); munmap(seg, len); close(fd);
    return r;
}

static int cmp(const void *a, const void *b) { double x = *(const double *)a, y = *(const double *)b; return (x > y) - (x < y); }
static void q(double *v, int n, double *m, double *p, double *x) { qsort(v, n, sizeof *v, cmp); *m = v[n / 2]; *p = v[(n * 9) / 10]; *x = v[n - 1]; }

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
#ifdef __GLIBC__
    const char *mylibc = "glibc";
#else
    const char *mylibc = "musl (static)";
#endif
    if (argc == 2 && !strcmp(argv[1], "forge")) {
        nconsume = 2; nruntime = 1;
        printf("supervisor C library: %s\n", mylibc);
        struct trial t = run_one(0, 1, 0);
        printf("forge: VERDICT a worker %s fake its own death\n", t.ok ? "CAN" : "could not");
        return 0;
    }
    if (argc != 5) { fprintf(stderr, "usage: sup <MiB,MiB,...> <kills> <consume> <runtime> | sup forge\n"); return 2; }
    int kills = atoi(argv[2]);
    nconsume = atoi(argv[3]);
    nruntime = atoi(argv[4]);

    // Print both C libraries once, from a throwaway worker.
    {
        struct trial t = run_one(0, 0, 1);
        (void)t;
    }
    printf("supervisor C library: %s; worker C library: %s; %d consume threads with a lock each, %d runtime threads without\n\n",
           mylibc, worker_libc, nconsume, nruntime);

    printf("All times in ms after the kill; %d kills per row; median / p90 / max\n", kills);
    printf("%-10s %-24s %-24s %-24s %s\n", "worker", "first lock reports", "last lock reports", "pidfd readable", "checks");
    char *list = strdup(argv[1]);
    for (char *s = strtok(list, ","); s; s = strtok(NULL, ",")) {
        size_t mib = strtoul(s, NULL, 10);
        double f[512], l[512], d[512];
        int n = 0, bad = 0, moved = 0, rt_after = 0;
        uint64_t rt_max = 0;
        for (int k = 0; k < kills && n < 512; k++) {
            struct trial t = run_one(mib, 0, 0);
            if (!t.ok) { bad++; continue; }
            f[n] = t.first_ms; l[n] = t.last_ms; d[n] = t.pidfd_ms; n++;
            moved += t.consume_moved_after_own_report;
            if (t.runtime_steps_after_first_report) rt_after++;
            if (t.runtime_steps_after_first_report > rt_max) rt_max = t.runtime_steps_after_first_report;
        }
        double a, b, c, a2, b2, c2, a3, b3, c3;
        q(f, n, &a, &b, &c); q(l, n, &a2, &b2, &c2); q(d, n, &a3, &b3, &c3);
        printf("%5zu MiB  %6.3f/%6.3f/%7.3f   %6.3f/%6.3f/%7.3f   %6.3f/%6.3f/%7.3f   consume thread ran after its own report: %d; runtime threads ran after the first report in %d of %d kills (up to %llu steps)%s\n",
               mib, a, b, c, a2, b2, c2, a3, b3, c3, moved, rt_after, n, (unsigned long long)rt_max, bad ? "; SOME KILLS FAILED THEIR CHECKS" : "");
    }
    return 0;
}
