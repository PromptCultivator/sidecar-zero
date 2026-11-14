// S6b, second part: cases the first run left out. Built statically against musl, like sup.c.
//
//   sup2 crash <how> <kills> <MiB>   how: kill | segv-consume | segv-runtime | abort-consume
//       One worker, four consume threads and two runtime threads. Instead of a SIGKILL from
//       outside, a thread inside the worker crashes. Times are from the moment the
//       supervisor asks for the crash.
//   sup2 sixteen <kills>
//       Two workers with eight consume threads each, so sixteen watchers: an active worker
//       whose threads run, and a standby whose threads are parked. The active one is killed,
//       then the standby. Only the killed worker's locks may report.
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

struct worker;
struct watch { struct worker *w; int i; };
struct worker {
    pid_t pid;
    int fd, pidfd, efd, nc, nr, status;
    size_t len;
    struct segment *seg;
    pthread_t th[MAX_CELLS];
    struct watch arg[MAX_CELLS];
    uint64_t t_report[MAX_CELLS];
    uint32_t word[MAX_CELLS];
    uint64_t own_counter[MAX_CELLS];
    uint64_t runtime_at[MAX_CELLS][MAX_RUNTIME];
    int reports;
    uint64_t t_pidfd;
};

static void *watcher(void *p) {
    struct watch *a = p;
    struct worker *w = a->w;
    int i = a->i;
    uint32_t *word = (uint32_t *)w->seg->cell[i].bytes;
    for (;;) {
        uint32_t v = __atomic_load_n(word, __ATOMIC_SEQ_CST);
        if (v & FUTEX_OWNER_DIED) {
            w->t_report[i] = now_ns();
            w->word[i] = v;
            w->own_counter[i] = __atomic_load_n(&w->seg->counter[i].v, __ATOMIC_SEQ_CST);
            for (int j = 0; j < w->nr; j++) w->runtime_at[i][j] = __atomic_load_n(&w->seg->counter[MAX_CELLS + j].v, __ATOMIC_SEQ_CST);
            uint64_t one = 1;
            if (write(w->efd, &one, sizeof one) != sizeof one) abort();
            return NULL;
        }
        if (!(v & FUTEX_WAITERS)) {
            if (!__atomic_compare_exchange_n(word, &v, v | FUTEX_WAITERS, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST)) continue;
            v |= FUTEX_WAITERS;
        }
        syscall(SYS_futex, word, FUTEX_WAIT, v, NULL, NULL, 0);
    }
}

static void start(struct worker *w, int nc, int nr, size_t mib, const char *behaviour) {
    memset(w, 0, sizeof *w);
    w->nc = nc; w->nr = nr;
    w->fd = memfd_create("scz_wrk", 0);
    w->len = (sizeof(struct segment) + 4095) & ~4095ul;
    if (ftruncate(w->fd, w->len)) abort();
    w->seg = mmap(NULL, w->len, PROT_READ | PROT_WRITE, MAP_SHARED, w->fd, 0);
    w->efd = eventfd(0, EFD_CLOEXEC);
    int ready[2];
    if (pipe(ready)) abort();
    w->pid = fork();
    if (w->pid == 0) {
        dup2(w->fd, 3); dup2(ready[1], 4);
        char a[3][24];
        snprintf(a[0], 24, "%d", nc); snprintf(a[1], 24, "%d", nr); snprintf(a[2], 24, "%zu", mib);
        chdir("/tmp"); // where a core file would go, if core dumps are on
        execl("/spike/worker", "worker", "3", "4", a[0], a[1], a[2], "normal", behaviour, (char *)NULL);
        _exit(127);
    }
    close(ready[1]);
    struct pollfd p = {.fd = ready[0], .events = POLLIN};
    char c;
    if (poll(&p, 1, 60000) != 1 || read(ready[0], &c, 1) != 1) { fprintf(stderr, "worker never became ready\n"); exit(1); }
    close(ready[0]);
    for (int i = 0; i < nc; i++) {
        w->arg[i] = (struct watch){w, i};
        pthread_create(&w->th[i], NULL, watcher, &w->arg[i]);
    }
    for (int armed = 0, tries = 0; armed < nc && tries < 20000; tries++) {
        armed = 0;
        for (int i = 0; i < nc; i++) armed += (__atomic_load_n((uint32_t *)w->seg->cell[i].bytes, __ATOMIC_SEQ_CST) & FUTEX_WAITERS) != 0;
        usleep(100);
    }
    w->pidfd = syscall(SYS_pidfd_open, w->pid, 0);
}

// Wait until the worker has exited and every lock has reported, or the time runs out.
static void await(struct worker *w, uint64_t t0, uint64_t limit_ns) {
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event e1 = {.events = EPOLLIN, .data.u32 = 1}, e2 = {.events = EPOLLIN, .data.u32 = 2};
    epoll_ctl(ep, EPOLL_CTL_ADD, w->pidfd, &e1);
    epoll_ctl(ep, EPOLL_CTL_ADD, w->efd, &e2);
    while ((w->t_pidfd == 0 || w->reports < w->nc) && now_ns() - t0 < limit_ns) {
        struct epoll_event got[2];
        int n = epoll_wait(ep, got, 2, 200);
        for (int k = 0; k < n; k++) {
            if (got[k].data.u32 == 1 && !w->t_pidfd) { w->t_pidfd = now_ns(); epoll_ctl(ep, EPOLL_CTL_DEL, w->pidfd, NULL); }
            if (got[k].data.u32 == 2) { uint64_t m; if (read(w->efd, &m, sizeof m) == sizeof m) w->reports += (int)m; }
        }
    }
    close(ep);
}

static void finish(struct worker *w) {
    for (int i = 0; i < w->nc; i++) {
        if (!w->t_report[i]) pthread_cancel(w->th[i]);
        pthread_join(w->th[i], NULL);
    }
    waitpid(w->pid, &w->status, 0);
    close(w->pidfd); close(w->efd); munmap(w->seg, w->len); close(w->fd);
}

static int cmp(const void *a, const void *b) { double x = *(const double *)a, y = *(const double *)b; return (x > y) - (x < y); }
static void q(double *v, int n, char *out, size_t sz) {
    if (!n) { snprintf(out, sz, "%-22s", "none"); return; }
    qsort(v, n, sizeof *v, cmp);
    snprintf(out, sz, "%6.3f/%6.3f/%7.3f", v[n / 2], v[(n * 9) / 10], v[n - 1]);
}

static int crash_mode(const char *how, int kills, size_t mib) {
    int code = !strcmp(how, "segv-consume") ? 1 : !strcmp(how, "segv-runtime") ? 2 : !strcmp(how, "abort-consume") ? 3 : 0;
    double first[256], last[256], pid[256];
    int n = 0, incomplete = 0, first_was_0 = 0, moved = 0, rt_after = 0;
    char sig[64] = "";
    for (int k = 0; k < kills && n < 256; k++) {
        struct worker w;
        start(&w, 4, 2, mib, "spin");
        nap_ms(20);
        uint64_t t0 = now_ns();
        if (code) __atomic_store_n(&w.seg->crash, code, __ATOMIC_SEQ_CST);
        else kill(w.pid, SIGKILL);
        await(&w, t0, 30000000000ull);
        uint64_t f = UINT64_MAX, l = 0;
        int fi = -1;
        for (int i = 0; i < w.nc; i++) {
            if (w.t_report[i] && w.t_report[i] < f) { f = w.t_report[i]; fi = i; }
            if (w.t_report[i] > l) l = w.t_report[i];
            if (w.t_report[i] && __atomic_load_n(&w.seg->counter[i].v, __ATOMIC_SEQ_CST) != w.own_counter[i]) moved++;
        }
        if (fi >= 0) {
            uint64_t after = 0;
            for (int j = 0; j < w.nr; j++) after += __atomic_load_n(&w.seg->counter[MAX_CELLS + j].v, __ATOMIC_SEQ_CST) - w.runtime_at[fi][j];
            if (after) rt_after++;
        }
        int complete = w.reports == w.nc && w.t_pidfd;
        finish(&w);
        if (WIFSIGNALED(w.status)) snprintf(sig, sizeof sig, "signal %d%s", WTERMSIG(w.status), WCOREDUMP(w.status) ? ", core dumped" : "");
        if (!complete) { incomplete++; continue; }
        if (fi == 0) first_was_0++;
        first[n] = (f - t0) / 1e6; last[n] = (l - t0) / 1e6; pid[n] = (w.t_pidfd - t0) / 1e6; n++;
    }
    char a[64], b[64], c[64];
    q(first, n, a, sizeof a); q(last, n, b, sizeof b); q(pid, n, c, sizeof c);
    printf("%-14s %5zu MiB  %s  %s  %s  worker ended by %s; first report from lock 0 in %d of %d; consume thread ran after its report: %d; runtime threads ran after the first report in %d%s\n",
           how, mib, a, b, c, sig, first_was_0, n, moved, rt_after, incomplete ? "; SOME RUNS INCOMPLETE" : "");
    return 0;
}

static int sixteen_mode(int kills) {
    double a_first[256], a_last[256], a_pid[256], b_last[256];
    int n = 0, cross = 0, incomplete = 0, moved = 0;
    for (int k = 0; k < kills && n < 256; k++) {
        struct worker A, B;
        start(&A, 8, 1, 64, "spin");   // the active worker: its consume threads run
        start(&B, 8, 0, 64, "sleep");  // the standby: its consume threads are parked
        nap_ms(30);
        uint64_t t0 = now_ns();
        kill(A.pid, SIGKILL);
        await(&A, t0, 10000000000ull);
        nap_ms(20); // give any wrongly delivered report time to arrive
        int b_early = 0;
        for (int i = 0; i < B.nc; i++) b_early += B.t_report[i] != 0 || (__atomic_load_n((uint32_t *)B.seg->cell[i].bytes, __ATOMIC_SEQ_CST) & FUTEX_OWNER_DIED);
        cross += b_early;
        uint64_t f = UINT64_MAX, l = 0;
        for (int i = 0; i < A.nc; i++) {
            if (A.t_report[i] && A.t_report[i] < f) f = A.t_report[i];
            if (A.t_report[i] > l) l = A.t_report[i];
            if (A.t_report[i] && __atomic_load_n(&A.seg->counter[i].v, __ATOMIC_SEQ_CST) != A.own_counter[i]) moved++;
        }
        int a_ok = A.reports == A.nc && A.t_pidfd;
        uint64_t t1 = now_ns();
        kill(B.pid, SIGKILL);
        await(&B, t1, 10000000000ull);
        uint64_t bl = 0;
        for (int i = 0; i < B.nc; i++) if (B.t_report[i] > bl) bl = B.t_report[i];
        int b_ok = B.reports == B.nc && B.t_pidfd;
        finish(&A); finish(&B);
        if (!a_ok || !b_ok) { incomplete++; continue; }
        a_first[n] = (f - t0) / 1e6; a_last[n] = (l - t0) / 1e6; a_pid[n] = (A.t_pidfd - t0) / 1e6; b_last[n] = (bl - t1) / 1e6; n++;
    }
    char s1[64], s2[64], s3[64], s4[64];
    q(a_first, n, s1, sizeof s1); q(a_last, n, s2, sizeof s2); q(a_pid, n, s3, sizeof s3); q(b_last, n, s4, sizeof s4);
    printf("16 watchers, %d kill pairs\n", n);
    printf("  active worker, 8 running consume threads:  first lock %s   last of 8 %s   pidfd %s ms\n", s1, s2, s3);
    printf("  standby, 8 parked consume threads:         last of 8 after its own kill %s ms\n", s4);
    printf("  standby locks that reported while only the active worker was killed: %d\n", cross);
    printf("  consume threads that ran after their own report: %d%s\n", moved, incomplete ? "; SOME RUNS INCOMPLETE" : "");
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc == 5 && !strcmp(argv[1], "crash")) return crash_mode(argv[2], atoi(argv[3]), strtoul(argv[4], NULL, 10));
    if (argc == 3 && !strcmp(argv[1], "sixteen")) return sixteen_mode(atoi(argv[2]));
    fprintf(stderr, "usage: sup2 crash <kill|segv-consume|segv-runtime|abort-consume> <kills> <MiB> | sup2 sixteen <kills>\n");
    return 2;
}
