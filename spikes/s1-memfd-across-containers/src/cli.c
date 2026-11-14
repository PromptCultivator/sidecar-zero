// S1 spike, client side: the host container. Receives the two segments, checks that the
// control segment can only be read, writes every page of the data segment, and reports
// what its own cgroup was charged for doing so.
#include "common.h"

int main(int argc, char **argv) {
    const char *sock = "/run/scz/scz.sock";
    for (int i = 1; i < argc; i++)
        if (!strncmp(argv[i], "--sock=", 7)) sock = argv[i] + 7;

    printf("client: uid %d gid %d\n", getuid(), getgid());
    struct charge c0 = read_charge();
    print_charge("client", "at start", c0);

    int s = -1;
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    strncpy(a.sun_path, sock, sizeof a.sun_path - 1);
    for (int i = 0; i < 300; i++) { // the sidecar may still be starting
        s = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
        if (connect(s, (struct sockaddr *)&a, sizeof a) == 0) break;
        close(s);
        s = -1;
        struct timespec nap = {0, 100 * 1000 * 1000};
        nanosleep(&nap, NULL);
    }
    if (s < 0) { printf("client: could not connect to %s (errno %d)\n", sock, errno); return 2; }

    uint64_t welcome[2];
    struct iovec iov = {welcome, sizeof welcome};
    union { char buf[CMSG_SPACE(2 * sizeof(int))]; struct cmsghdr align; } u;
    struct msghdr mh = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.buf, .msg_controllen = sizeof u.buf};
    if (recvmsg(s, &mh, MSG_CMSG_CLOEXEC) <= 0) { perror("recvmsg"); return 2; }
    struct cmsghdr *cm = CMSG_FIRSTHDR(&mh);
    if (!cm || cm->cmsg_type != SCM_RIGHTS) { printf("client: no descriptors arrived\n"); return 2; }
    int fds[2];
    memcpy(fds, CMSG_DATA(cm), sizeof fds);
    int ctl = fds[0], data = fds[1];
    size_t data_len = welcome[1];

    int ok = 1, t;
    printf("client: received 2 descriptors; close-on-exec set: %s\n",
           (fcntl(ctl, F_GETFD) & FD_CLOEXEC) && (fcntl(data, F_GETFD) & FD_CLOEXEC) ? "yes" : "NO");
    printf("client: seals  control 0x%x   data 0x%x   (GROW 0x4, SHRINK 0x2, SEAL 0x1, FUTURE_WRITE 0x10)\n",
           fcntl(ctl, F_GET_SEALS), fcntl(data, F_GET_SEALS));

    // The control segment must be readable and nothing more.
    void *w = mmap(NULL, CTL_LEN, PROT_READ | PROT_WRITE, MAP_SHARED, ctl, 0);
    t = (w == MAP_FAILED); ok &= t;
    printf("CHECK control segment refuses a writable mapping ............ %s (errno %d)\n", t ? "PASS" : "FAIL", t ? errno : 0);
    uint64_t *ctlp = mmap(NULL, CTL_LEN, PROT_READ, MAP_SHARED, ctl, 0);
    t = (ctlp != MAP_FAILED && ctlp[0] == CTL_MAGIC); ok &= t;
    printf("CHECK control segment maps read-only and reads correctly .... %s\n", t ? "PASS" : "FAIL");
    t = (mprotect(ctlp, CTL_LEN, PROT_READ | PROT_WRITE) != 0); ok &= t;
    printf("CHECK mprotect cannot make it writable ...................... %s (errno %d)\n", t ? "PASS" : "FAIL", t ? errno : 0);
    t = (ftruncate(data, data_len / 2) != 0); ok &= t;
    printf("CHECK the data segment cannot be shrunk under the others .... %s\n", t ? "PASS" : "FAIL");

    // The data segment: map it, then write every page, as a host under load would.
    unsigned char *datap = mmap(NULL, data_len, PROT_READ | PROT_WRITE, MAP_SHARED, data, 0);
    if (datap == MAP_FAILED) { perror("mmap data"); return 2; }
    if (madvise(datap, data_len, MADV_POPULATE_WRITE)) printf("client: MADV_POPULATE_WRITE failed (errno %d)\n", errno);
    print_charge("client", "after mapping and populating page tables", read_charge());
    memset(datap, 0xAB, data_len);
    struct charge c1 = read_charge();
    print_charge("client", "after writing every page", c1);
    say(s, "WROTE");

    double grew = (c1.current - c0.current) / MIB, shm = (c1.shmem - (c0.shmem < 0 ? 0 : c0.shmem)) / MIB;
    printf("RESULT the client's cgroup grew by %.2f MiB (shmem %.2f MiB) after writing %.0f MiB\n", grew, shm, data_len / MIB);

    // Sleep on a word in memory we can only read; the supervisor wakes us from its side.
    uint32_t *word = (uint32_t *)&ctlp[16];
    say(s, "WAITING");
    struct timespec to = {3, 0};
    uint64_t t0 = now_ns();
    long fr = syscall(SYS_futex, word, FUTEX_WAIT, 0, &to, NULL, 0);
    int e = errno;
    t = (fr == 0 && *word == 1); ok &= t;
    printf("CHECK futex wait on the read-only mapping, woken across containers  %s (returned %ld, errno %d, after %.1f ms)\n",
           t ? "PASS" : "FAIL", fr, fr ? e : 0, (now_ns() - t0) / 1e6);

    say(s, "DONE");
    printf("VERDICT client checks: %s\n", ok ? "PASS" : "FAIL");
    fflush(stdout);
    return 0;
}
