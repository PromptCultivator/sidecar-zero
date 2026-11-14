// The few operating-system services the SDK needs: the monotonic clock, the spin hint, and on
// Linux the shared futex, thread ids, and the kernel constants that older headers lack
// (sdk-cpp20 §5). Everything Linux-specific is behind SCZ_LINUX; elsewhere the client compiles
// and always falls back with NOT_CONNECTED (sdk-contract §9).
#pragma once
#include <cerrno>
#include <cstdint>
#include <ctime>

#if defined(__linux__)
#define SCZ_LINUX 1
#include <sys/syscall.h>
#include <unistd.h>
#else
#define SCZ_LINUX 0
#endif

// The conformance agent's UNWIND hook (sdk-contract §11 item 11) throws from inside the
// enqueue; with SCZ_TEST_HOOKS (and exceptions on) the call path is therefore declared
// noexcept(false) so that the exception reaches the agent after the guards have run. Without
// it, which is every application build, infer is noexcept.
#if defined(SCZ_TEST_HOOKS) && defined(__cpp_exceptions)
#define SCZ_HOOKS_THROW 1
#define SCZ_CALL_NOEXCEPT noexcept(false)
#else
#define SCZ_HOOKS_THROW 0
#define SCZ_CALL_NOEXCEPT noexcept
#endif

namespace scz::inline v1::detail {

/// Nanoseconds of CLOCK_MONOTONIC, the clock every deadline and heartbeat is in (a vDSO call).
[[nodiscard]] inline std::uint64_t monotonic_ns() noexcept {
    timespec ts{};
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return static_cast<std::uint64_t>(ts.tv_sec) * 1'000'000'000ULL + static_cast<std::uint64_t>(ts.tv_nsec);
}

/// One turn of a spin loop: `pause` on x86-64, `isb` on arm64 (where `yield` is a no-op).
inline void cpu_relax() noexcept {
#if defined(__x86_64__) || defined(__i386__)
    __builtin_ia32_pause();
#elif defined(__aarch64__)
    asm volatile("isb" ::: "memory");
#endif
}

#if SCZ_LINUX
namespace kernel {
// Not every distribution's headers that the compiler floor covers define these.
inline constexpr int kAddSeals = 1033;
inline constexpr int kGetSeals = 1034;
inline constexpr int kSealSeal = 0x1;
inline constexpr int kSealShrink = 0x2;
inline constexpr int kSealGrow = 0x4;
inline constexpr int kSealFutureWrite = 0x10;
inline constexpr int kMadvPopulateRead = 22;
inline constexpr int kMadvPopulateWrite = 23;
inline constexpr int kFutexWake = 1;
inline constexpr int kFutexWaitBitset = 9;  // shared (no FUTEX_PRIVATE_FLAG): waiter and waker are different processes
inline constexpr unsigned kFutexBitsetMatchAny = 0xffffffffU;
inline constexpr int kCtlSeals = kSealGrow | kSealShrink | kSealFutureWrite | kSealSeal;
inline constexpr int kDataSeals = kSealGrow | kSealShrink | kSealSeal;
}  // namespace kernel

/// The calling thread's kernel thread id.
[[nodiscard]] inline std::uint32_t kernel_tid() noexcept { return static_cast<std::uint32_t>(::syscall(SYS_gettid)); }

/// Whether a thread of this process with that id still exists (background thread only).
[[nodiscard]] inline bool thread_alive(std::uint32_t tid) noexcept {
    const int saved = errno;
    const long r = ::syscall(SYS_tgkill, static_cast<long>(::getpid()), static_cast<long>(tid), 0L);
    const bool alive = r == 0 || errno == EPERM;
    errno = saved;
    return alive;
}

/// Sleep while the 32-bit word at `addr` holds `expected`, until the absolute monotonic
/// `deadline_ns` (FUTEX_WAIT_BITSET, so an interrupted wait does not drift). False on timeout;
/// spurious returns are possible and callers re-check. errno is left as it was.
inline bool futex_wait_until(const void* addr, std::uint32_t expected, std::uint64_t deadline_ns) noexcept {
    const int saved = errno;
    timespec ts{static_cast<time_t>(deadline_ns / 1'000'000'000ULL), static_cast<long>(deadline_ns % 1'000'000'000ULL)};
    const timespec* tsp = (deadline_ns == 0) ? nullptr : &ts;
    const long r = ::syscall(SYS_futex, addr, kernel::kFutexWaitBitset, expected, tsp, nullptr, kernel::kFutexBitsetMatchAny);
    const bool timed_out = r == -1 && errno == ETIMEDOUT;
    errno = saved;
    return !timed_out;
}

/// Wake up to `n` waiters on the 32-bit word at `addr`.
inline void futex_wake(const void* addr, int n) noexcept {
    const int saved = errno;
    ::syscall(SYS_futex, addr, kernel::kFutexWake, n, nullptr, nullptr, 0);
    errno = saved;
}
#endif

}  // namespace scz::inline v1::detail
