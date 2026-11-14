// S1 spike, supervisor side. Creates the two segments the way the design says, hands
// them to a client in another container, and reports what its own cgroup is charged
// before and after the client has written every page.
#include "common.h"

int main(int argc, char **argv) {
    int prefault = 1, data_mb = 64, stay = 0;
    const char *sock = "/run/scz/scz.sock";
    for (int i = 1; i < argc; i++) {
        if (!strncmp(argv[i], "--prefault=", 11)) prefault = atoi(argv[i] + 11);
        else if (!strncmp(argv[i], "--data-mb=", 10)) data_mb = atoi(argv[i] + 10);
        else if (!strncmp(argv[i], "--sock=", 7)) sock = argv[i] + 7;
        else if (!strcmp(argv[i], "--stay")) stay = 1;
    }
    size_t data_len = (size_t)data_mb << 20;
    printf("supervisor: uid %d gid %d, data segment %d MiB, prefault=%d\n", getuid(), getgid(), data_mb, prefault);
    print_charge("supervisor", "at start", read_charge());

    // Control segment: mapped writable by us first, then sealed against future writes.
    int ctl = syscall(SYS_memfd_create, "scz_ctl", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (ctl < 0 || ftruncate(ctl, CTL_LEN)) { perror("ctl memfd"); return 2; }
    uint64_t *ctlp = mmap(NULL, CTL_LEN, PROT_READ | PROT_WRITE, MAP_SHARED, ctl, 0);
    ctlp[0] = CTL_MAGIC;
    uint32_t *futex_word = (uint32_t *)&ctlp[16]; // stands in for standby_wait
    if (fcntl(ctl, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_FUTURE_WRITE | F_SEAL_SEAL)) { perror("seal ctl"); return 2; }

    // Data segment: everyone writes it.
    int data = syscall(SYS_memfd_create, "scz_data", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (data < 0 || ftruncate(data, data_len)) { perror("data memfd"); return 2; }
    unsigned char *datap = mmap(NULL, data_len, PROT_READ | PROT_WRITE, MAP_SHARED, data, 0);
    if (prefault) {
        if (madvise(datap, data_len, MADV_POPULATE_WRITE)) {
            printf("supervisor: MADV_POPULATE_WRITE failed (errno %d), touching pages instead\n", errno);
            for (size_t i = 0; i < data_len; i += 4096) datap[i] = 0;
        }
    }
    if (fcntl(data, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL)) { perror("seal data"); return 2; }
    print_charge("supervisor", prefault ? "after creating and pre-faulting" : "after creating, NOT pre-faulted", read_charge());

    // Hand both descriptors to whoever connects.
    unlink(sock);
    int ls = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    strncpy(a.sun_path, sock, sizeof a.sun_path - 1);
    if (bind(ls, (struct sockaddr *)&a, sizeof a) || listen(ls, 4)) { perror("bind/listen"); return 2; }
    chmod(sock, 0660);
    printf("supervisor: listening on %s\n", sock);
    fflush(stdout);

    int c = accept4(ls, NULL, NULL, SOCK_CLOEXEC);
    struct ucred cred;
    socklen_t cl = sizeof cred;
    getsockopt(c, SOL_SOCKET, SO_PEERCRED, &cred, &cl);
    printf("supervisor: client connected, SO_PEERCRED uid %d gid %d pid %d\n", cred.uid, cred.gid, cred.pid);

    uint64_t welcome[2] = {CTL_LEN, data_len};
    struct iovec iov = {welcome, sizeof welcome};
    union { char buf[CMSG_SPACE(2 * sizeof(int))]; struct cmsghdr align; } u;
    memset(&u, 0, sizeof u);
    struct msghdr mh = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = u.buf, .msg_controllen = sizeof u.buf};
    struct cmsghdr *cm = CMSG_FIRSTHDR(&mh);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(2 * sizeof(int));
    int fds[2] = {ctl, data};
    memcpy(CMSG_DATA(cm), fds, sizeof fds);
    if (sendmsg(c, &mh, MSG_NOSIGNAL) < 0) { perror("sendmsg"); return 2; }

    if (!hear(c, "WROTE")) { printf("supervisor: client did not report its writes\n"); return 2; }
    print_charge("supervisor", "after the client wrote every page", read_charge());

    // The client is now asleep on a word in the control segment, which it can only read.
    if (!hear(c, "WAITING")) { printf("supervisor: client did not start waiting\n"); return 2; }
    struct timespec nap = {0, 100 * 1000 * 1000};
    nanosleep(&nap, NULL);
    __atomic_store_n(futex_word, 1, __ATOMIC_SEQ_CST);
    long woke = syscall(SYS_futex, futex_word, FUTEX_WAKE, 1, NULL, NULL, 0);
    printf("supervisor: stored the word and woke %ld sleeper(s)\n", woke);

    hear(c, "DONE");
    print_charge("supervisor", "at the end", read_charge());
    fflush(stdout);
    if (stay) for (;;) pause(); // a native sidecar keeps running
    return 0;
}
