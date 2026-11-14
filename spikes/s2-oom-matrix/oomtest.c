// S2 spike: what survives when a worker runs the container out of memory?
//
// This program is PID 1 of a container with a memory limit. It starts a "standby" that
// just sits there, then repeatedly starts a "worker" that allocates until the kernel
// kills something. After each round it says who is still alive. If the kernel kills the
// whole container, PID 1 dies with it and Kubernetes restarts the container, which the
// driver script sees as a restart count above zero.
//
//   --adj      first try to set our own oom_score_adj to -1000 (the oomScoreAdj shield)
//   --nested   first try to put ourselves and each child in child cgroups, with the
//              worker under its own memory limit (the nestedCgroup shield)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static char CG[512] = "/sys/fs/cgroup";

// A privileged container may see the node's whole cgroup tree. Our own cgroup is then a
// path below the mount, named in /proc/self/cgroup, and that is where we must work.
static void find_own_cgroup(void) {
    char line[512], path[512];
    FILE *f = fopen("/proc/self/cgroup", "r");
    if (!f) return;
    while (fgets(line, sizeof line, f)) {
        if (strncmp(line, "0::", 3)) continue;
        line[strcspn(line, "\n")] = 0;
        if (strcmp(line + 3, "/") == 0) break; // private cgroup namespace: the mount root is ours
        snprintf(path, sizeof path, "/sys/fs/cgroup%s/cgroup.procs", line + 3);
        if (access(path, F_OK) == 0) snprintf(CG, sizeof CG, "/sys/fs/cgroup%s", line + 3);
    }
    fclose(f);
}

static const char *cg(const char *rel) {
    static char buf[4][640];
    static int i;
    char *b = buf[i++ & 3];
    snprintf(b, 640, "%s%s", CG, rel);
    return b;
}

static int put(const char *path, const char *val) {
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -errno;
    int r = write(fd, val, strlen(val)) < 0 ? -errno : 0;
    close(fd);
    return r;
}

static void get(const char *path, char *out, size_t n) {
    out[0] = 0;
    int fd = open(path, O_RDONLY);
    if (fd < 0) { snprintf(out, n, "(unreadable: errno %d)", errno); return; }
    ssize_t r = read(fd, out, n - 1);
    close(fd);
    if (r < 0) r = 0;
    out[r] = 0;
    for (char *p = out; *p; p++) if (*p == '\n') *p = ' ';
}

static void show(const char *label, const char *path) {
    char b[512];
    get(path, b, sizeof b);
    printf("  %-34s %s\n", label, b);
}

static void cap_eff(char *out, size_t n) {
    FILE *f = fopen("/proc/self/status", "r");
    char line[256];
    snprintf(out, n, "?");
    while (f && fgets(line, sizeof line, f))
        if (!strncmp(line, "CapEff:", 7)) { snprintf(out, n, "%s", line + 8); out[strcspn(out, "\n")] = 0; }
    if (f) fclose(f);
}

static void nap(int ms) { struct timespec t = {ms / 1000, (ms % 1000) * 1000000L}; nanosleep(&t, NULL); }

static int join_cgroup(const char *name) {
    char p[768], pid[32];
    snprintf(p, sizeof p, "%s/%s/cgroup.procs", CG, name);
    snprintf(pid, sizeof pid, "%d", getpid());
    return put(p, pid);
}

int main(int argc, char **argv) {
    int want_adj = 0, want_nested = 0, rounds = 3;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--adj")) want_adj = 1;
        if (!strcmp(argv[i], "--nested")) want_nested = 1;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    find_own_cgroup();
    char b[512];
    cap_eff(b, sizeof b);
    printf("supervisor: pid %d uid %d, effective capabilities %s\n", getpid(), getuid(), b);
    printf("  our cgroup                         %s\n", CG);
    show("memory.max", cg("/memory.max"));
    show("memory.oom.group", cg("/memory.oom.group"));
    show("our oom_score_adj", "/proc/self/oom_score_adj");

    if (want_adj) {
        int r = put("/proc/self/oom_score_adj", "-1000");
        printf("SHIELD oomScoreAdj: writing -1000 %s (errno %d)\n", r ? "FAILED" : "succeeded", -r);
        show("our oom_score_adj now", "/proc/self/oom_score_adj");
    }

    int nested = 0;
    if (want_nested) {
        int r1 = mkdir(cg("/supervisor"), 0755) ? -errno : 0;
        int r2 = mkdir(cg("/worker"), 0755) ? -errno : 0;
        int r3 = mkdir(cg("/standby"), 0755) ? -errno : 0;
        printf("SHIELD nestedCgroup: mkdir child cgroups -> %d %d %d (0 is success; negative is -errno)\n", r1, r2, r3);
        if (!r1 && !r2 && !r3) {
            int rj = join_cgroup("supervisor"); // a cgroup that delegates a controller may hold no processes
            printf("SHIELD nestedCgroup: move ourselves into the child cgroup -> %d\n", rj);
            show("processes left in the container cgroup", cg("/cgroup.procs"));
            show("controllers available to it", cg("/cgroup.controllers"));
            int r4 = put(cg("/cgroup.subtree_control"), "+memory");
            get(cg("/memory.max"), b, sizeof b);
            long limit = atol(b), wl = limit > 0 ? limit - (40l << 20) : (56l << 20);
            char v[32];
            snprintf(v, sizeof v, "%ld", wl);
            int r5 = put(cg("/worker/memory.max"), v);
            printf("SHIELD nestedCgroup: enable memory for children -> %d, worker limit %ld MiB -> %d\n", r4, wl >> 20, r5);
            nested = !r4 && !r5;
        }
        printf("SHIELD nestedCgroup: %s\n", nested ? "in effect" : "NOT available here");
    }

    nap(4000);
    pid_t standby = fork();
    if (standby == 0) {
        if (nested) join_cgroup("standby");
        put("/proc/self/oom_score_adj", "1000");
        char *p = mmap(NULL, 8 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        memset(p, 1, 8 << 20);
        for (;;) pause();
    }

    int survived = 0, standby_alive = 1;
    for (int r = 1; r <= rounds; r++) {
        pid_t w = fork();
        if (w == 0) {
            if (nested) join_cgroup("worker");
            put("/proc/self/oom_score_adj", "1000"); // raising it needs no privilege
            for (;;) { // allocate until something gives
                char *p = mmap(NULL, 4 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
                if (p == MAP_FAILED) { nap(10); continue; }
                memset(p, 1, 4 << 20);
            }
        }
        int st = 0;
        waitpid(w, &st, 0);
        survived++;
        int sst;
        if (standby_alive && waitpid(standby, &sst, WNOHANG) == standby) standby_alive = 0;
        printf("round %d: worker ended by signal %d; supervisor alive; standby %s\n", r, WIFSIGNALED(st) ? WTERMSIG(st) : 0,
               standby_alive ? "alive" : "KILLED");
        show("container memory.events", cg("/memory.events"));
        if (nested) show("worker cgroup memory.events", cg("/worker/memory.events"));
        nap(500);
    }
    printf("RESULT supervisor survived %d worker OOM%s; standby %s\n", survived, survived == 1 ? "" : "s", standby_alive ? "survived" : "was killed");
    for (;;) pause();
}
