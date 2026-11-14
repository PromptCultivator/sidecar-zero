// The session socket (sdk-contract §6, handshake): a SOCK_SEQPACKET Unix socket carrying three
// fixed 64-byte messages (HELLO, WELCOME with two memfds by SCM_RIGHTS, READY) and then held
// open, idle, as the liveness channel. Linux only. Mirrors scz-shm's `handshake.rs`.
#pragma once
#include "platform.hpp"

#if SCZ_LINUX
#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>

#include <cstddef>
#include <cstdint>
#include <cstring>

#include "shared.hpp"

namespace scz::inline v1::detail {

using Msg = std::uint8_t[abi::MSG_LEN];

inline void encode_hello(Msg& b, std::uint16_t sdk_lang, std::uint32_t sdk_version) noexcept {
    namespace mh = abi::msg_hello;
    std::memset(b, 0, sizeof b);
    put32(b + mh::MAGIC, static_cast<std::uint32_t>(abi::MSG_MAGIC));
    put32(b + mh::KIND, abi::msg_kind::HELLO);
    put16(b + mh::ABI_MAJOR, abi::ABI_MAJOR);
    put16(b + mh::ABI_MINOR_MIN, abi::ABI_MINOR_MIN);
    put16(b + mh::ABI_MINOR_MAX, abi::ABI_MINOR);
    put16(b + mh::SDK_LANG, sdk_lang);
    put32(b + mh::SDK_VERSION, sdk_version);
    put32(b + mh::PID, static_cast<std::uint32_t>(::getpid()));
    put64(b + mh::FLAGS, 0);
}

struct Welcome {
    std::uint32_t status = 0;
    std::uint16_t abi_major = 0;
    std::uint16_t abi_minor = 0;
    std::uint32_t ctl_len = 0;
    std::uint64_t data_len = 0;
    std::uint64_t arena_id = 0;
    std::uint64_t session_id = 0;
};

[[nodiscard]] inline bool decode_welcome(const Msg& b, Welcome& w) noexcept {
    namespace mw = abi::msg_welcome;
    if (le32(b + mw::MAGIC) != static_cast<std::uint32_t>(abi::MSG_MAGIC) || le32(b + mw::KIND) != abi::msg_kind::WELCOME) return false;
    w = {le32(b + mw::STATUS), le16(b + mw::ABI_MAJOR), le16(b + mw::ABI_MINOR), le32(b + mw::CTL_LEN), le64(b + mw::DATA_LEN), le64(b + mw::ARENA_ID),
         le64(b + mw::SESSION_ID)};
    return true;
}

inline void encode_ready(Msg& b, std::uint64_t arena_id) noexcept {
    namespace mr = abi::msg_ready;
    std::memset(b, 0, sizeof b);
    put32(b + mr::MAGIC, static_cast<std::uint32_t>(abi::MSG_MAGIC));
    put32(b + mr::KIND, abi::msg_kind::READY);
    put64(b + mr::ARENA_ID, arena_id);
}

/// A new SOCK_SEQPACKET socket, close-on-exec, with `timeout_ms` on every send and receive
/// (a Unix-socket connect honours SO_SNDTIMEO). -1 on failure.
[[nodiscard]] inline int seqpacket(std::uint64_t timeout_ms) noexcept {
    const int fd = ::socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;
    timeval tv{static_cast<time_t>(timeout_ms / 1000), static_cast<suseconds_t>((timeout_ms % 1000) * 1000)};
    if (::setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv) != 0 || ::setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv) != 0) {
        ::close(fd);
        return -1;
    }
    return fd;
}

/// Connect to a path, or to an abstract name written `@name`.
[[nodiscard]] inline bool connect_to(int fd, const char* endpoint, std::size_t len) noexcept {
    sockaddr_un a{};
    a.sun_family = AF_UNIX;
    const bool abstract = len > 0 && endpoint[0] == '@';
    const std::size_t start = abstract ? 1 : 0;
    const std::size_t n = len - start;
    if (start + n >= sizeof a.sun_path) return false;
    std::memcpy(a.sun_path + start, endpoint + start, n);
    const socklen_t alen = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + (abstract ? 1 + n : n + 1));
    return ::connect(fd, reinterpret_cast<const sockaddr*>(&a), alen) == 0;
}

[[nodiscard]] inline bool send_msg(int fd, const Msg& m) noexcept {
    return ::send(fd, m, sizeof m, MSG_NOSIGNAL) == static_cast<ssize_t>(sizeof m);
}

/// Receive one message and up to two descriptors (close-on-exec). Every descriptor received is
/// taken, so none leaks: extras are closed. Returns false on end of stream, error, or a
/// malformed message; `fds` are then closed and -1.
[[nodiscard]] inline bool recv_msg(int fd, Msg& m, int (&fds)[2]) noexcept {
    fds[0] = fds[1] = -1;
    iovec iov{m, sizeof m};
    alignas(cmsghdr) unsigned char cbuf[CMSG_SPACE(4 * sizeof(int))];
    msghdr mh{};
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    mh.msg_control = cbuf;
    mh.msg_controllen = sizeof cbuf;
    const ssize_t n = ::recvmsg(fd, &mh, MSG_CMSG_CLOEXEC);
    if (n < 0) return false;
    int got = 0;
    for (cmsghdr* c = CMSG_FIRSTHDR(&mh); c != nullptr; c = CMSG_NXTHDR(&mh, c)) {
        if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS) continue;
        const std::size_t count = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
        for (std::size_t i = 0; i < count; ++i) {
            int f;
            std::memcpy(&f, CMSG_DATA(c) + i * sizeof(int), sizeof f);
            if (got < 2) fds[got++] = f;
            else ::close(f);
        }
    }
    const bool ok = n == static_cast<ssize_t>(sizeof m) && (mh.msg_flags & (MSG_TRUNC | MSG_CTRUNC)) == 0;
    if (!ok) {
        for (int& f : fds)
            if (f >= 0) {
                ::close(f);
                f = -1;
            }
    }
    return ok;
}

/// Wait up to `ms` for the socket to become readable or hung up.
[[nodiscard]] inline int wait_readable(int fd, int ms) noexcept {
    pollfd p{fd, static_cast<short>(POLLIN | POLLRDHUP), 0};
    const int r = ::poll(&p, 1, ms);
    if (r < 0) return errno == EINTR ? 0 : -1;
    return r > 0 ? 1 : 0;
}

/// Whether the peer has closed a connected socket: a non-blocking peek that returns 0 bytes.
/// 1 closed, 0 not, -1 error.
[[nodiscard]] inline int peer_closed(int fd) noexcept {
    std::uint8_t b[64];
    const ssize_t r = ::recv(fd, b, sizeof b, MSG_PEEK | MSG_DONTWAIT);
    if (r == 0) return 1;
    if (r > 0) return 0;
    if (errno == EAGAIN || errno == EINTR) return 0;
    if (errno == ECONNRESET) return 1;
    return -1;
}

[[nodiscard]] inline int seals_of(int fd) noexcept { return ::fcntl(fd, kernel::kGetSeals); }

/// The file size by lseek to the end (sdk-contract §6 suggests it over fstat).
[[nodiscard]] inline std::int64_t file_len(int fd) noexcept { return static_cast<std::int64_t>(::lseek(fd, 0, SEEK_END)); }

}  // namespace scz::inline v1::detail
#endif
