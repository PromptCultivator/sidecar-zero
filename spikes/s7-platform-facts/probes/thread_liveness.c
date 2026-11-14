// S7 probe: can a process tell that one of its own threads has exited, from that
// thread's kernel thread id?
//
// The SDK's in-call marker carries the caller's thread id so that a marker left by a
// thread that died without unwinding can be ignored. That is safe only if "this thread
// id no longer names a live thread of this process" can be answered reliably, and
// never answers "gone" for a thread that is alive.
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static sem_t ready, go;
static pid_t the_tid;

static int alive_tgkill(pid_t tid) { return syscall(SYS_tgkill, getpid(), tid, 0) == 0 ? 1 : (errno == ESRCH ? 0 : -errno); }

static int alive_proc(pid_t tid) {
    char p[64];
    struct stat st;
    snprintf(p, sizeof p, "/proc/self/task/%d", tid);
    return stat(p, &st) == 0;
}

static void *returns_normally(void *arg) {
    (void)arg;
    the_tid = gettid();
    sem_post(&ready);
    sem_wait(&go);
    return NULL;
}

// Leaves through the raw exit system call: no unwinding, no thread-exit hooks.
static void *exits_raw(void *arg) {
    (void)arg;
    the_tid = gettid();
    sem_post(&ready);
    sem_wait(&go);
    syscall(SYS_exit, 0);
    return NULL;
}

static void *noop(void *arg) { *(pid_t *)arg = gettid(); return NULL; }

static double ms_since(struct timespec *t0) {
    struct timespec t1;
    clock_gettime(CLOCK_MONOTONIC, &t1);
    return (t1.tv_sec - t0->tv_sec) * 1e3 + (t1.tv_nsec - t0->tv_nsec) / 1e6;
}

static int one_case(const char *name, void *(*fn)(void *), int detach) {
    pthread_t th;
    pthread_create(&th, NULL, fn, NULL);
    if (detach) pthread_detach(th);
    sem_wait(&ready);
    pid_t tid = the_tid;
    int a1 = alive_tgkill(tid), p1 = alive_proc(tid);

    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    sem_post(&go);
    if (!detach) pthread_join(th, NULL);

    // Poll until the kernel stops naming the thread.
    double gone_after = -1;
    for (int i = 0; i < 200000; i++) {
        if (alive_tgkill(tid) == 0) { gone_after = ms_since(&t0); break; }
    }
    int a2 = alive_tgkill(tid), p2 = alive_proc(tid);
    printf("%-34s tid=%d  alive before: tgkill=%d /proc=%d   after exit: tgkill=%d /proc=%d   reported gone after %.3f ms\n",
           name, tid, a1, p1, a2, p2, gone_after);
    return a1 == 1 && p1 == 1 && a2 == 0 && p2 == 0;
}

int main(void) {
    sem_init(&ready, 0, 0);
    sem_init(&go, 0, 0);
    int ok = 1;
    ok &= one_case("joined, returns normally", returns_normally, 0);
    ok &= one_case("joined, raw exit (no unwinding)", exits_raw, 0);
    ok &= one_case("detached, raw exit (no unwinding)", exits_raw, 1);

    // A live thread must never be reported gone. Ask a million times about ourselves.
    int false_gone = 0;
    pid_t self = gettid();
    for (int i = 0; i < 1000000; i++) false_gone += alive_tgkill(self) != 1;
    printf("live thread reported gone in 1,000,000 checks: %d\n", false_gone);

    // How soon is a dead thread's id handed to a new thread? Reuse makes a dead thread
    // look alive, which only delays; it never makes a live one look dead.
    pid_t dead = the_tid, t;
    int reused_after = -1;
    for (int i = 0; i < 20000; i++) {
        pthread_t th;
        pthread_create(&th, NULL, noop, &t);
        pthread_join(th, NULL);
        if (t == dead) { reused_after = i + 1; break; }
    }
    FILE *f = fopen("/proc/sys/kernel/pid_max", "r");
    long pid_max = 0;
    if (f) { if (fscanf(f, "%ld", &pid_max) != 1) pid_max = 0; fclose(f); }
    if (reused_after < 0) printf("dead tid %d not reused within 20,000 new threads (pid_max=%ld)\n", dead, pid_max);
    else printf("dead tid %d reused after %d new threads (pid_max=%ld)\n", dead, reused_after, pid_max);

    printf("\nVERDICT thread_liveness: %s\n", (ok && false_gone == 0) ? "PASS" : "FAIL");
    return 0;
}
