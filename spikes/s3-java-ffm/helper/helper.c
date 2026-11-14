// Spike S3 helper: plays the supervisor for the Java FFM transport spike.
// Throwaway code. Creates two sealed memfds, hands them to a client over a
// SOCK_SEQPACKET Unix socket with SCM_RIGHTS, and stores + FUTEX_WAKEs on
// request so the Java side can be woken from another process.
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
#include <sys/syscall.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#define F_GET_SEALS 1034
#endif
#ifndef F_SEAL_SEAL
#define F_SEAL_SEAL 0x0001
#define F_SEAL_SHRINK 0x0002
#define F_SEAL_GROW 0x0004
#define F_SEAL_WRITE 0x0008
#endif
#ifndef F_SEAL_FUTURE_WRITE
#define F_SEAL_FUTURE_WRITE 0x0010
#endif
#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#define MFD_ALLOW_SEALING 0x0002U
#endif

#define CTL_LEN (64 * 1024)
#define DATA_LEN (8 * 1024 * 1024)
#define MAGIC 0x3353504b495a4353ULL /* "SCZIKPS3" little-endian, arbitrary */

// Control segment layout (all the spike needs):
//   0  u64 magic     8  u64 data_len    16  u64 ctl_len
//  64  u64 "gate"    (written AFTER sealing, through the pre-seal mapping)
//  72  u64 "heartbeat"
// Data segment: byte 0 u64 magic; everything else is scratch for the client.

enum { CMD_STORE_WAKE = 1, CMD_STORE_CTL = 2, CMD_QUIT = 3 };

struct req {
    uint32_t cmd, off, val, nwake, delay_ms, pad;
};
struct rep {
    int32_t rc, err;
};
struct welcome {
    uint64_t magic, ctl_len, data_len;
};

static void die(const char *what) {
    perror(what);
    exit(1);
}

static void sleep_ms(unsigned ms) {
    struct timespec ts = {ms / 1000, (long)(ms % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) == -1 && errno == EINTR) {
    }
}

static int make_memfd(const char *name, size_t len, int seals, uint8_t **map_out) {
    int fd = (int)syscall(SYS_memfd_create, name, MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (fd < 0) die("memfd_create");
    if (ftruncate(fd, (off_t)len) != 0) die("ftruncate");
    // Map writable BEFORE sealing: with F_SEAL_FUTURE_WRITE this mapping stays
    // writable, and no later one can be.
    uint8_t *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) die("mmap");
    if (fcntl(fd, F_ADD_SEALS, seals) != 0) die("F_ADD_SEALS");
    int got = fcntl(fd, F_GET_SEALS);
    fprintf(stderr, "helper: memfd %-8s fd=%d len=%zu seals=0x%x (asked 0x%x)\n", name, fd, len, got, seals);
    *map_out = p;
    return fd;
}

static int send_welcome(int c, int ctl_fd, int data_fd) {
    struct welcome w = {MAGIC, CTL_LEN, DATA_LEN};
    struct iovec iov = {&w, sizeof w};
    union {
        char buf[CMSG_SPACE(2 * sizeof(int))];
        struct cmsghdr align;
    } u;
    memset(&u, 0, sizeof u);
    struct msghdr m;
    memset(&m, 0, sizeof m);
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = u.buf;
    m.msg_controllen = sizeof u.buf;
    struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(2 * sizeof(int));
    int fds[2] = {ctl_fd, data_fd};
    memcpy(CMSG_DATA(cm), fds, sizeof fds);
    return sendmsg(c, &m, MSG_NOSIGNAL) == (ssize_t)sizeof w ? 0 : -1;
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/tmp/scz-s3.sock";
    setvbuf(stderr, NULL, _IOLBF, 0);

    uint8_t *ctl, *data;
    int ctl_fd = make_memfd("scz-ctl", CTL_LEN, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_FUTURE_WRITE | F_SEAL_SEAL, &ctl);
    int data_fd = make_memfd("scz-data", DATA_LEN, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL, &data);

    // Written after sealing, through the mapping made before it.
    *(uint64_t *)(ctl + 0) = MAGIC;
    *(uint64_t *)(ctl + 8) = DATA_LEN;
    *(uint64_t *)(ctl + 16) = CTL_LEN;
    *(uint64_t *)(ctl + 64) = 0x1111;
    *(uint64_t *)(data + 0) = MAGIC;

    int ls = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    if (ls < 0) die("socket");
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    strncpy(sa.sun_path, path, sizeof sa.sun_path - 1);
    unlink(path);
    if (bind(ls, (struct sockaddr *)&sa, sizeof sa) != 0) die("bind");
    if (listen(ls, 8) != 0) die("listen");
    fprintf(stderr, "helper: listening on %s pid=%d\n", path, (int)getpid());

    for (;;) {
        int c = accept4(ls, NULL, NULL, SOCK_CLOEXEC);
        if (c < 0) {
            if (errno == EINTR) continue;
            die("accept");
        }
        uint64_t hello = 0;
        ssize_t n = recv(c, &hello, sizeof hello, 0);
        fprintf(stderr, "helper: client connected, HELLO %zd bytes 0x%llx\n", n, (unsigned long long)hello);
        if (n != (ssize_t)sizeof hello || send_welcome(c, ctl_fd, data_fd) != 0) {
            close(c);
            continue;
        }
        for (;;) {
            struct req r;
            n = recv(c, &r, sizeof r, 0);
            if (n == 0) {
                fprintf(stderr, "helper: client EOF\n");
                break;
            }
            if (n != (ssize_t)sizeof r) {
                fprintf(stderr, "helper: bad request (%zd bytes, errno %d)\n", n, errno);
                break;
            }
            struct rep p = {0, 0};
            if (r.cmd == CMD_STORE_WAKE && r.off <= DATA_LEN - 4 && (r.off & 3) == 0) {
                sleep_ms(r.delay_ms);
                uint32_t *w = (uint32_t *)(data + r.off);
                __atomic_store_n(w, r.val, __ATOMIC_SEQ_CST);
                // Shared futex: no FUTEX_PRIVATE_FLAG, so the key is the file page.
                long rc = syscall(SYS_futex, w, FUTEX_WAKE, r.nwake, NULL, NULL, 0);
                p.rc = (int32_t)rc;
                p.err = rc < 0 ? errno : 0;
            } else if (r.cmd == CMD_STORE_CTL && r.off <= CTL_LEN - 8 && (r.off & 7) == 0) {
                __atomic_store_n((uint64_t *)(ctl + r.off), (uint64_t)r.val, __ATOMIC_SEQ_CST);
            } else if (r.cmd == CMD_QUIT) {
                fprintf(stderr, "helper: QUIT\n");
                send(c, &p, sizeof p, MSG_NOSIGNAL);
                unlink(path);
                return 0;
            } else {
                p.rc = -1;
                p.err = EINVAL;
            }
            if (send(c, &p, sizeof p, MSG_NOSIGNAL) != (ssize_t)sizeof p) break;
        }
        close(c);
    }
}
