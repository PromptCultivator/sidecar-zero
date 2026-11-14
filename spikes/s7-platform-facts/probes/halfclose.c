// S7 probe: does closing the sending side of a SOCK_SEQPACKET Unix socket deliver
// end-of-stream to the peer, and is the peer's later close seen by the first side?
//
// This is the mechanism behind "reset requested by the supervisor": the supervisor
// half-closes the session socket, the client reads end-of-stream, leaves the arena,
// and closes its own end, which the supervisor takes as proof the arena is unmapped.
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *path = "/tmp/scz-halfclose.sock";

static int poll_in(int fd, int timeout_ms, short *revents) {
    struct pollfd p = {.fd = fd, .events = POLLIN | POLLRDHUP};
    int r = poll(&p, 1, timeout_ms);
    *revents = p.revents;
    return r;
}

static void describe(char *out, size_t n, short rev) {
    snprintf(out, n, "%s%s%s%s", (rev & POLLIN) ? "POLLIN " : "", (rev & POLLRDHUP) ? "POLLRDHUP " : "",
             (rev & POLLHUP) ? "POLLHUP " : "", (rev & POLLERR) ? "POLLERR " : "");
}

int main(void) {
    unlink(path);
    int ls = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    strncpy(a.sun_path, path, sizeof a.sun_path - 1);
    if (bind(ls, (struct sockaddr *)&a, sizeof a) || listen(ls, 4)) { perror("bind/listen"); return 2; }

    int go[2], rep[2];
    if (pipe(go) || pipe(rep)) { perror("pipe"); return 2; }

    pid_t pid = fork();
    if (pid == 0) { // ── the client ──
        close(go[1]); close(rep[0]);
        int c = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
        if (connect(c, (struct sockaddr *)&a, sizeof a)) { dprintf(rep[1], "client: connect failed errno=%d\n", errno); _exit(1); }
        send(c, "HELLO", 5, MSG_NOSIGNAL);

        short rev; char ev[64], buf[64];
        int r = poll_in(c, 3000, &rev);
        describe(ev, sizeof ev, rev);
        ssize_t n = recv(c, buf, sizeof buf, 0);
        dprintf(rep[1], "client: after supervisor SHUT_WR: poll=%d [%s] recv=%zd\n", r, ev, n);
        dprintf(rep[1], "CLIENT_SAW_EOF=%d\n", r == 1 && n == 0);

        ssize_t s = send(c, "STILL", 5, MSG_NOSIGNAL);
        dprintf(rep[1], "client: send after peer's SHUT_WR -> %zd (errno %d)\n", s, s < 0 ? errno : 0);

        char x;
        (void)!read(go[0], &x, 1); // hold our end open until told to close
        close(c);
        dprintf(rep[1], "client: closed its end\n");
        _exit(0);
    }

    // ── the supervisor ──
    close(go[0]); close(rep[1]);
    int s = accept4(ls, NULL, NULL, SOCK_CLOEXEC);
    char buf[64];
    recv(s, buf, sizeof buf, 0); // HELLO

    if (shutdown(s, SHUT_WR)) perror("shutdown");

    // While the client still holds its end open, we must NOT see end-of-stream.
    int eof_before = 0, msgs_before = 0;
    for (int i = 0; i < 6; i++) {
        short rev;
        if (poll_in(s, 100, &rev) > 0) {
            ssize_t n = recv(s, buf, sizeof buf, MSG_DONTWAIT);
            if (n == 0) eof_before = 1;
            if (n > 0) msgs_before++;
        }
    }
    printf("supervisor: while client holds its end: messages=%d end-of-stream=%d\n", msgs_before, eof_before);

    (void)!write(go[1], "x", 1); // tell the client to close

    short rev; char ev[64];
    int r = poll_in(s, 3000, &rev);
    describe(ev, sizeof ev, rev);
    ssize_t n = recv(s, buf, sizeof buf, 0);
    printf("supervisor: after client close: poll=%d [%s] recv=%zd\n", r, ev, n);
    int eof_after = (r == 1 && n == 0);

    // Relay what the client observed.
    int client_saw_eof = 0;
    FILE *f = fdopen(rep[0], "r");
    char line[256];
    while (fgets(line, sizeof line, f)) {
        fputs(line, stdout);
        if (strstr(line, "CLIENT_SAW_EOF=1")) client_saw_eof = 1;
    }
    waitpid(pid, NULL, 0);
    unlink(path);

    printf("\nQ1 client reads end-of-stream after the supervisor's SHUT_WR ........ %s\n", client_saw_eof ? "YES" : "NO");
    printf("Q2 supervisor sees nothing while the client keeps its end open ...... %s\n", !eof_before ? "YES" : "NO");
    printf("Q3 supervisor sees end-of-stream once the client closes ............ %s\n", eof_after ? "YES" : "NO");
    printf("VERDICT halfclose: %s\n", (client_saw_eof && !eof_before && eof_after) ? "PASS" : "FAIL");
    return 0;
}
