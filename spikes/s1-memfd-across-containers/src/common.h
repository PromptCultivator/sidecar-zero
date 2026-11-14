// Shared helpers for the S1 spike. Throwaway code.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#ifndef F_SEAL_FUTURE_WRITE
#define F_SEAL_FUTURE_WRITE 0x0010
#endif
#ifndef MADV_POPULATE_WRITE
#define MADV_POPULATE_WRITE 23
#endif

#define CTL_LEN (64 * 1024)
#define CTL_MAGIC 0x414e4552415a4353ull // "SCZARENA", little-endian
#define MIB (1024.0 * 1024.0)

static inline uint64_t now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec;
}

// What this container's own cgroup is charged for. Inside a container on cgroup v2 the
// container's cgroup is the root of its cgroup namespace.
struct charge { long current, anon, file, shmem, pagetables; };

static inline long stat_field(const char *text, const char *key) {
    size_t n = strlen(key);
    for (const char *p = text; p && *p; p = strchr(p, '\n') ? strchr(p, '\n') + 1 : NULL)
        if (!strncmp(p, key, n) && p[n] == ' ') return atol(p + n + 1);
    return -1;
}

static inline struct charge read_charge(void) {
    struct charge c = {-1, -1, -1, -1, -1};
    char buf[8192];
    FILE *f = fopen("/sys/fs/cgroup/memory.current", "r");
    if (f) { if (fscanf(f, "%ld", &c.current) != 1) c.current = -1; fclose(f); }
    f = fopen("/sys/fs/cgroup/memory.stat", "r");
    if (f) {
        size_t n = fread(buf, 1, sizeof buf - 1, f);
        buf[n] = 0;
        fclose(f);
        c.anon = stat_field(buf, "anon");
        c.file = stat_field(buf, "file");
        c.shmem = stat_field(buf, "shmem");
        c.pagetables = stat_field(buf, "pagetables");
    }
    return c;
}

static inline void print_charge(const char *who, const char *when, struct charge c) {
    printf("%-10s %-42s current %7.2f MiB   shmem %7.2f MiB   anon %6.2f MiB   pagetables %5.2f MiB\n", who, when,
           c.current / MIB, c.shmem / MIB, c.anon / MIB, c.pagetables / MIB);
    fflush(stdout);
}

static inline void say(int fd, const char *msg) { send(fd, msg, strlen(msg), MSG_NOSIGNAL); }

static inline int hear(int fd, const char *want) {
    char buf[64];
    ssize_t n = recv(fd, buf, sizeof buf - 1, 0);
    if (n <= 0) return 0;
    buf[n] = 0;
    return strcmp(buf, want) == 0;
}
