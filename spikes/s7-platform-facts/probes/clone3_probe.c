// S7 probe: what does the container's seccomp profile do with clone3, and does the
// fallback to clone with CLONE_PIDFD work?
//
// The supervisor spawns workers with clone3 and falls back to clone. The design doc
// says, from memory, that default seccomp profiles answer clone3 with "not implemented"
// so that callers fall back.
#define _GNU_SOURCE
#include <errno.h>
#include <linux/sched.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int child_fn(void *arg) { (void)arg; _exit(7); }

static int wait_pidfd(int pidfd, pid_t pid) {
    struct pollfd p = {.fd = pidfd, .events = POLLIN};
    int r = poll(&p, 1, 3000);
    int st = 0;
    waitpid(pid, &st, 0);
    return r == 1 && WIFEXITED(st) && WEXITSTATUS(st) == 7;
}

int main(void) {
    int pidfd = -1;
    struct clone_args args;
    memset(&args, 0, sizeof args);
    args.flags = CLONE_PIDFD;
    args.pidfd = (uint64_t)(uintptr_t)&pidfd;
    args.exit_signal = SIGCHLD;

    long r = syscall(SYS_clone3, &args, sizeof args);
    if (r == 0) _exit(7);
    int clone3_ok = 0;
    if (r > 0) {
        clone3_ok = wait_pidfd(pidfd, (pid_t)r);
        printf("clone3: returned pid %ld, pidfd %d, pidfd became readable on exit: %s\n", r, pidfd, clone3_ok ? "yes" : "no");
    } else {
        printf("clone3: failed, errno %d (%s)\n", errno, strerror(errno));
    }

    // The fallback the supervisor uses when clone3 is refused.
    static char stack[64 * 1024];
    int pidfd2 = -1;
    pid_t c = clone(child_fn, stack + sizeof stack, CLONE_PIDFD | SIGCHLD, NULL, &pidfd2);
    int clone_ok = 0;
    if (c > 0) {
        clone_ok = wait_pidfd(pidfd2, c);
        printf("clone + CLONE_PIDFD: returned pid %d, pidfd %d, pidfd became readable on exit: %s\n", c, pidfd2, clone_ok ? "yes" : "no");
    } else {
        printf("clone + CLONE_PIDFD: failed, errno %d (%s)\n", errno, strerror(errno));
    }

    printf("\nRESULT clone3=%s fallback=%s\n", clone3_ok ? "works" : "refused", clone_ok ? "works" : "FAILS");
    printf("VERDICT clone: %s\n", (clone3_ok || clone_ok) ? "PASS (a worker can be spawned with a pidfd)" : "FAIL");
    return 0;
}
