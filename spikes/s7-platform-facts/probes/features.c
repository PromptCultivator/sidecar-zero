// S7 probe: which of the kernel features the design names are present on this kernel,
// and do the two that the control segment depends on behave as assumed?
//
//   - a memfd sealed with F_SEAL_FUTURE_WRITE can still be mapped read-only, cannot be
//     mapped writable, and cannot be made writable afterwards;
//   - a futex wait works on that read-only mapping and is woken from another process.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef F_SEAL_FUTURE_WRITE
#define F_SEAL_FUTURE_WRITE 0x0010
#endif
#ifndef MADV_POPULATE_WRITE
#define MADV_POPULATE_WRITE 23
#endif
#ifndef MADV_POPULATE_READ
#define MADV_POPULATE_READ 22
#endif
#ifndef SYS_landlock_create_ruleset
#define SYS_landlock_create_ruleset 444
#endif
#ifndef LANDLOCK_CREATE_RULESET_VERSION
#define LANDLOCK_CREATE_RULESET_VERSION 1
#endif

static void row(const char *what, int ok, const char *detail) { printf("%-58s %-4s %s\n", what, ok ? "yes" : "NO", detail ? detail : ""); }

static uint64_t now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec; }

int main(void) {
    struct utsname u;
    uname(&u);
    printf("kernel %s %s, page size %ld\n\n", u.release, u.machine, sysconf(_SC_PAGESIZE));
    char d[160];

    // ── memfd and seals ──
    int fd = syscall(SYS_memfd_create, "scz_ctl", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    row("memfd_create with sealing", fd >= 0, NULL);
    size_t len = 65536;
    if (ftruncate(fd, len)) perror("ftruncate");
    uint32_t *rw = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0); // the supervisor's own writable view
    row("supervisor maps it writable before sealing", rw != MAP_FAILED, NULL);

    int r = madvise(rw, len, MADV_POPULATE_WRITE);
    snprintf(d, sizeof d, r ? "errno %d: fall back to MAP_POPULATE or touching pages" : "", errno);
    row("MADV_POPULATE_WRITE", r == 0, d);

    r = fcntl(fd, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_FUTURE_WRITE | F_SEAL_SEAL);
    snprintf(d, sizeof d, r ? "errno %d" : "", errno);
    row("seal GROW|SHRINK|FUTURE_WRITE|SEAL with a writable map held", r == 0, d);
    int seals = fcntl(fd, F_GET_SEALS);
    snprintf(d, sizeof d, "F_GET_SEALS = 0x%x", seals);
    row("seals read back", (seals & F_SEAL_FUTURE_WRITE) != 0, d);

    rw[0] = 0; // the supervisor's existing mapping must still be writable
    row("supervisor's existing mapping still writes", 1, NULL);

    void *w2 = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    snprintf(d, sizeof d, "errno %d (%s)", errno, strerror(errno));
    row("a NEW writable mapping is refused", w2 == MAP_FAILED, w2 == MAP_FAILED ? d : "it was allowed!");
    uint32_t *ro = mmap(NULL, len, PROT_READ, MAP_SHARED, fd, 0);
    row("a new read-only shared mapping is allowed", ro != MAP_FAILED, NULL);
    r = mprotect(ro, len, PROT_READ | PROT_WRITE);
    snprintf(d, sizeof d, "errno %d (%s)", errno, strerror(errno));
    row("mprotect cannot add write to it", r != 0, r != 0 ? d : "it was allowed!");
    r = ftruncate(fd, len * 2);
    row("the file cannot be resized", r != 0, NULL);
    ssize_t wr = pwrite(fd, "x", 1, 0);
    row("write() through the descriptor is refused", wr < 0, NULL);
    r = madvise(ro, len, MADV_POPULATE_READ);
    row("MADV_POPULATE_READ on the read-only mapping", r == 0, NULL);

    // ── futex on the read-only mapping, across processes ──
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        // The child plays a worker: it has only the read-only view.
        munmap(rw, len);
        struct timespec to = {2, 0};
        uint64_t t0 = now_ns();
        long fr = syscall(SYS_futex, ro, FUTEX_WAIT, 0, &to, NULL, 0); // shared futex: no PRIVATE flag
        int e = errno;
        uint64_t dt = now_ns() - t0;
        printf("%-58s %-4s returned %ld errno %d after %.3f ms, word now %u\n", "futex wait on the read-only mapping is woken",
               (fr == 0 && ro[0] == 1) ? "yes" : "NO", fr, fr ? e : 0, dt / 1e6, ro[0]);

        // Absolute-deadline wait, which is what the design uses, must time out cleanly.
        struct timespec dl;
        clock_gettime(CLOCK_MONOTONIC, &dl);
        dl.tv_nsec += 20 * 1000 * 1000;
        if (dl.tv_nsec >= 1000000000) { dl.tv_sec++; dl.tv_nsec -= 1000000000; }
        t0 = now_ns();
        fr = syscall(SYS_futex, ro, FUTEX_WAIT_BITSET, 1, &dl, NULL, FUTEX_BITSET_MATCH_ANY);
        e = errno;
        dt = now_ns() - t0;
        printf("%-58s %-4s errno %d after %.3f ms\n", "FUTEX_WAIT_BITSET with an absolute deadline times out",
               (fr == -1 && e == ETIMEDOUT) ? "yes" : "NO", e, dt / 1e6);
        fflush(stdout);
        _exit(0);
    }
    struct timespec nap = {0, 100 * 1000 * 1000};
    nanosleep(&nap, NULL);
    __atomic_store_n(&rw[0], 1, __ATOMIC_SEQ_CST);
    long woke = syscall(SYS_futex, rw, FUTEX_WAKE, 1, NULL, NULL, 0);
    waitpid(pid, NULL, 0);
    snprintf(d, sizeof d, "FUTEX_WAKE returned %ld", woke);
    row("the supervisor's wake found the sleeper", woke == 1, d);

    // ── the other calls the design names ──
    int pfd = syscall(SYS_pidfd_open, getpid(), 0);
    row("pidfd_open", pfd >= 0, NULL);
    r = syscall(SYS_close_range, 10000, 10010, 0);
    snprintf(d, sizeof d, r ? "errno %d" : "", errno);
    row("close_range", r == 0, d);
    long ll = syscall(SYS_landlock_create_ruleset, NULL, 0, LANDLOCK_CREATE_RULESET_VERSION);
    if (ll >= 0) snprintf(d, sizeof d, "ABI version %ld", ll);
    else snprintf(d, sizeof d, "errno %d (%s)", errno, strerror(errno));
    row("Landlock", ll >= 0, d);

    struct stat st;
    row("cgroup v2 (cgroup.controllers present)", stat("/sys/fs/cgroup/cgroup.controllers", &st) == 0, NULL);
    row("memory pressure file in this cgroup", stat("/sys/fs/cgroup/memory.pressure", &st) == 0, NULL);
    row("system-wide pressure file", stat("/proc/pressure/memory", &st) == 0, NULL);
    row("memory.oom.group in this cgroup", stat("/sys/fs/cgroup/memory.oom.group", &st) == 0, NULL);
    return 0;
}
