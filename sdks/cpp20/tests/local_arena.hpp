// A sidecar in the test process: an arena in two sealed memfds laid out by the rule of
// abi/scz_abi.toml `[layout]`, the handshake on an abstract socket, a heartbeat, and a C++
// model of the chaos worker consuming ring 0. It lets the unit tests, the allocation gate, and
// the sanitizer builds run the whole client, handshake included, without the Rust binaries.
//
// The worker answers a keyed request (conformance/proto) with the FNV-1a hash of its bytes,
// and honours the opcodes normal, delay (µs), hang (ms, 0 for ever), and status. With
// `frames`, the registry holds one synthetic template pair (features F32[256] → score F32[1])
// and the worker answers the sum of the features.
#pragma once
#include <fcntl.h>
#include <poll.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>

#include "scz/sidecarzero.hpp"

namespace scztest {

using namespace scz::detail;
namespace abi = scz::abi;

inline std::uint64_t fnv1a64(const std::uint8_t* p, std::size_t n) {
    std::uint64_t h = 0xcbf29ce484222325ULL;
    for (std::size_t i = 0; i < n; ++i) h = (h ^ p[i]) * 0x100000001b3ULL;
    return h;
}

inline std::uint64_t splitmix64(std::uint64_t x) {
    x += 0x9e3779b97f4a7c15ULL;
    x = (x ^ (x >> 30)) * 0xbf58476d1ce4e5b9ULL;
    x = (x ^ (x >> 27)) * 0x94d049bb133111ebULL;
    return x ^ (x >> 31);
}

constexpr std::uint64_t kMagic = 0x534F4148435A4353ULL;  // "SCZCHAOS"
namespace op {
constexpr std::uint64_t NORMAL = 0, DELAY = 1, HANG = 3, STATUS = 4;
}

/// A keyed request, into a caller's fixed buffer: no allocation on the measured path.
struct KeyedReq {
    std::uint64_t w[5 + 8];
    std::size_t n = 5;
    KeyedReq(std::uint64_t o, std::uint64_t arg, std::uint32_t t, std::uint32_t s, std::uint64_t seed = 1, std::size_t rows = 0) {
        const std::uint64_t nonce = (static_cast<std::uint64_t>(t) << 32) | s;
        w[0] = kMagic, w[1] = o, w[2] = arg, w[3] = nonce, w[4] = seed;
        for (std::size_t i = 0; i < rows && i < 8; ++i) w[5 + i] = splitmix64(seed ^ nonce ^ i);
        n = 5 + (rows < 8 ? rows : 8);
    }
    [[nodiscard]] std::span<const std::byte> bytes() const { return {reinterpret_cast<const std::byte*>(w), n * 8}; }
    [[nodiscard]] std::uint64_t answer() const { return fnv1a64(reinterpret_cast<const std::uint8_t*>(w), n * 8); }
};

struct Config {
    std::uint32_t ring_cap = 64;
    std::uint32_t slot_count = 64;
    std::uint32_t slot_size = 4096;
    std::uint32_t req_cap = 2048;
    std::uint32_t resp_cap = 2048;
    std::uint64_t spin_ns = 50'000;
    std::uint64_t hb_stale_ns = 15'000'000;
    std::uint64_t max_deadline_ns = 100'000'000;
    bool frames = false;
};

inline std::uint64_t up(std::uint64_t v, std::uint64_t a) { return (v + a - 1) / a * a; }

template <class T>
inline void put(std::byte* base, std::size_t off, T v) {
    std::memcpy(base + off, &v, sizeof v);
}

class FakeSidecar {
public:
    FakeSidecar(std::string name, Config c = {}) : name_(std::move(name)), c_(c) {}
    ~FakeSidecar() { stop(); }

    [[nodiscard]] const std::string& endpoint() const { return name_; }
    [[nodiscard]] std::byte* data() const { return data_; }
    [[nodiscard]] std::byte* ctl() const { return ctl_; }
    [[nodiscard]] std::uint64_t ring_off() const { return ring_off_; }
    [[nodiscard]] std::uint64_t table_off() const { return table_off_; }
    [[nodiscard]] std::uint64_t slot_data_off() const { return slot_data_off_; }
    [[nodiscard]] const Config& config() const { return c_; }
    std::atomic<std::uint64_t> served{0};
    std::atomic<int> busy_sent{0};

    void set_gate(std::uint32_t state, std::uint32_t gen = 1) { store_relaxed<std::uint64_t>(ctl_ + abi::control_header::GATE, gate_word(gen, 0, state)); }
    void heartbeat(bool on) { hb_on_.store(on); }
    /// Leave the next requests in PROCESSING until released (a worker that stalls).
    void hold_worker(bool on) { hold_.store(on); }

    bool start() {
        if (!make_arena()) return false;
        listen_fd_ = ::socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
        sockaddr_un a{};
        a.sun_family = AF_UNIX;
        std::memcpy(a.sun_path + 1, name_.data() + 1, name_.size() - 1);  // "@name": abstract
        const socklen_t alen = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + name_.size());
        if (::bind(listen_fd_, reinterpret_cast<sockaddr*>(&a), alen) != 0 || ::listen(listen_fd_, 4) != 0) {
            std::perror("fake sidecar: bind");
            return false;
        }
        set_gate(abi::gate_state::HEALTHY);
        running_.store(true);
        hb_ = std::thread([this] { beat(); });
        acceptor_ = std::thread([this] { accept_loop(); });
        worker_ = std::thread([this] { consume(); });
        return true;
    }

    /// The supervisor goes away: its end of the session closes, the threads stop. The
    /// mappings stay until the object is destroyed, so a client still inside can finish.
    void stop() {
        if (!running_.exchange(false)) return;
        acceptor_.join();  // it polls with a 5 ms timeout
        if (listen_fd_ >= 0) {
            ::close(listen_fd_);
            listen_fd_ = -1;
        }
        hb_.join();
        worker_.join();
    }

    /// Whether the client's end of the session has closed (it left the arena).
    [[nodiscard]] bool client_gone() const { return client_gone_.load(); }

private:
    bool make_arena() {
        const std::uint64_t staging_off = abi::STATUS_PAGE_LEN, staging_len = 0x1000;
        ring_off_ = up(staging_off + abi::BEAT_SLOTS * staging_len, 4096);
        table_off_ = up(ring_off_ + abi::RING_HEADER_LEN + static_cast<std::uint64_t>(c_.ring_cap) * abi::CELL_LEN, 4096);
        slot_data_off_ = up(table_off_ + static_cast<std::uint64_t>(c_.slot_count) * abi::SLOT_RECORD_LEN, 4096);
        const std::uint64_t end = slot_data_off_ + static_cast<std::uint64_t>(c_.slot_count) * c_.slot_size;
        data_len_ = 1;
        while (data_len_ < end) data_len_ <<= 1;
        ctl_len_ = abi::CTL_HEADER_LEN + abi::CTL_REGISTRY_LEN;
        ctl_fd_ = ::memfd_create("scz-control", MFD_CLOEXEC | MFD_ALLOW_SEALING);
        data_fd_ = ::memfd_create("scz-data", MFD_CLOEXEC | MFD_ALLOW_SEALING);
        if (ctl_fd_ < 0 || data_fd_ < 0 || ::ftruncate(ctl_fd_, static_cast<off_t>(ctl_len_)) != 0 || ::ftruncate(data_fd_, static_cast<off_t>(data_len_)) != 0)
            return false;
        ctl_ = static_cast<std::byte*>(::mmap(nullptr, ctl_len_, PROT_READ | PROT_WRITE, MAP_SHARED, ctl_fd_, 0));
        data_ = static_cast<std::byte*>(::mmap(nullptr, data_len_, PROT_READ | PROT_WRITE, MAP_SHARED, data_fd_, 0));
        if (ctl_ == MAP_FAILED || data_ == MAP_FAILED) return false;
        namespace h = abi::control_header;
        const std::uint64_t arena_id = 0x5EED0000ULL + static_cast<std::uint64_t>(::getpid());
        put<std::uint64_t>(ctl_, h::MAGIC, abi::CTL_MAGIC);
        put<std::uint16_t>(ctl_, h::ABI_MAJOR, abi::ABI_MAJOR);
        put<std::uint16_t>(ctl_, h::ABI_MINOR, abi::ABI_MINOR);
        put<std::uint32_t>(ctl_, h::HEADER_LEN, static_cast<std::uint32_t>(abi::CTL_HEADER_LEN));
        put<std::uint64_t>(ctl_, h::DATA_LEN, data_len_);
        put<std::uint64_t>(ctl_, h::ARENA_ID, arena_id);
        put<std::uint32_t>(ctl_, h::CTL_LEN, static_cast<std::uint32_t>(ctl_len_));
        put<std::uint32_t>(ctl_, h::PAGE_SIZE, 4096);
        put<std::uint16_t>(ctl_, h::RING_COUNT, 1);
        put<std::uint16_t>(ctl_, h::SLAB_CLASS_COUNT, 1);
        put<std::uint64_t>(ctl_, h::CREATED_MONO_NS, monotonic_ns());
        put<std::uint64_t>(ctl_, h::HB_INTERVAL_NS, 5'000'000);
        put<std::uint64_t>(ctl_, h::HB_STALE_NS, c_.hb_stale_ns);
        put<std::uint64_t>(ctl_, h::SPIN_NS, c_.spin_ns);
        put<std::uint64_t>(ctl_, h::DEFAULT_DEADLINE_NS, 2'000'000);
        put<std::uint64_t>(ctl_, h::MAX_DEADLINE_NS, c_.max_deadline_ns);
        std::byte* rd = ctl_ + h::ring_dir::OFF;
        put<std::uint64_t>(rd, abi::ring_dir_entry::RING_OFF, ring_off_);
        put<std::uint32_t>(rd, abi::ring_dir_entry::CAPACITY, c_.ring_cap);
        put<std::uint32_t>(rd, abi::ring_dir_entry::KIND, abi::ring_kind::MPSC);
        put<std::uint32_t>(rd, abi::ring_dir_entry::CELL_STRIDE, static_cast<std::uint32_t>(abi::CELL_LEN));
        std::byte* sd = ctl_ + h::slab_dir::OFF;
        put<std::uint64_t>(sd, abi::slab_dir_entry::TABLE_OFF, table_off_);
        put<std::uint64_t>(sd, abi::slab_dir_entry::DATA_OFF, slot_data_off_);
        put<std::uint32_t>(sd, abi::slab_dir_entry::SLOT_SIZE, c_.slot_size);
        put<std::uint32_t>(sd, abi::slab_dir_entry::SLOT_COUNT, c_.slot_count);
        put<std::uint32_t>(sd, abi::slab_dir_entry::REQ_CAP, c_.req_cap);
        put<std::uint32_t>(sd, abi::slab_dir_entry::RESP_CAP, c_.resp_cap);
        put<std::uint16_t>(sd, abi::slab_dir_entry::RECORD_STRIDE, static_cast<std::uint16_t>(abi::SLOT_RECORD_LEN));
        std::byte* dm = ctl_ + h::data_map::OFF;
        put<std::uint64_t>(dm, abi::data_map::STATUS_OFF, 0);
        put<std::uint64_t>(dm, abi::data_map::STAGING_OFF, staging_off);
        put<std::uint32_t>(dm, abi::data_map::STAGING_LEN, static_cast<std::uint32_t>(staging_len));
        put<std::uint32_t>(ctl_, h::LAYOUT_CRC32C, layout_crc(ctl_));
        put<std::uint64_t>(data_, abi::status_page::MAGIC, abi::DATA_MAGIC);
        put<std::uint64_t>(data_, abi::status_page::ARENA_ID, arena_id);
        for (std::uint64_t i = 0; i < c_.ring_cap; ++i) put<std::uint64_t>(data_ + ring_off_ + abi::ring_header::CELLS_OFF + i * abi::CELL_LEN, 0, i);
        write_model_block();
        if (::fcntl(data_fd_, kernel::kAddSeals, kernel::kDataSeals) != 0 || ::fcntl(ctl_fd_, kernel::kAddSeals, kernel::kCtlSeals) != 0) {
            std::perror("fake sidecar: seals");
            return false;
        }
        return true;
    }

    /// The model block and, with `frames`, a synthetic registry (arrow-data-plane §4).
    void write_model_block() {
        namespace h = abi::control_header;
        std::uint32_t reg_len = 0;
        std::uint64_t fp = 0;
        if (c_.frames) {
            std::byte* r = ctl_ + abi::CTL_HEADER_LEN;
            namespace rh = abi::registry_header;
            namespace td = abi::template_desc;
            namespace cd = abi::column_desc;
            namespace pd = abi::patch_desc;
            const std::uint32_t toff = 64, coff = toff + 2 * 48, poff = coff + 2 * 32, names_off = 248, in_off = 264, req_prefix = 264, resp_prefix = 328;
            reg_len = 392;
            put<std::uint64_t>(r, rh::MAGIC, abi::REGISTRY_MAGIC);
            put<std::uint32_t>(r, rh::REGISTRY_LEN, reg_len);
            put<std::uint16_t>(r, rh::TEMPLATE_COUNT, 2);
            put<std::uint16_t>(r, rh::TEMPLATE_STRIDE, 48);
            put<std::uint16_t>(r, rh::COLUMN_STRIDE, 32);
            put<std::uint16_t>(r, rh::PATCH_STRIDE, 12);
            put<std::uint32_t>(r, rh::IN_SCHEMA_OFF, in_off);
            put<std::uint32_t>(r, rh::OUT_SCHEMA_OFF, in_off);
            put<std::uint32_t>(r, rh::TEMPLATES_OFF, toff);
            put<std::uint32_t>(r, rh::COLUMNS_OFF, coff);
            put<std::uint32_t>(r, rh::PATCHES_OFF, poff);
            put<std::uint32_t>(r, rh::NAMES_OFF, names_off);
            put<std::uint32_t>(r, rh::NAMES_LEN, 13);
            put<std::uint32_t>(r, rh::COLUMN_TOTAL, 2);
            put<std::uint32_t>(r, rh::PATCH_TOTAL, 2);
            std::memcpy(r + names_off, "featuresscore", 13);
            auto tmpl = [&](std::uint32_t k, std::uint16_t id, std::uint8_t dir, std::uint32_t prefix, std::uint32_t frame_cap) {
                std::byte* t = r + toff + k * 48;
                put<std::uint16_t>(t, td::TEMPLATE_ID, id);
                put<std::uint8_t>(t, td::DIRECTION, dir);
                put<std::uint16_t>(t, td::SLAB_CLASS, 0);
                put<std::uint16_t>(t, td::COLUMN_COUNT, 1);
                put<std::uint32_t>(t, td::MAX_ROWS, 1);
                put<std::uint32_t>(t, td::PREFIX_OFF, prefix);
                put<std::uint32_t>(t, td::PREFIX_LEN, 64);
                put<std::uint32_t>(t, td::BODY_OFF, 64);
                put<std::uint32_t>(t, td::FRAME_CAP, frame_cap);
                put<std::uint32_t>(t, td::FIRST_COLUMN, k);
                put<std::uint32_t>(t, td::FIRST_PATCH, k);
                put<std::uint16_t>(t, td::PATCH_COUNT, 1);
                put<std::uint32_t>(t, td::ROWS_FIELD_OFF, 8);
                std::byte* p = r + prefix;
                for (int i = 0; i < 64; ++i) p[i] = static_cast<std::byte>(0xA0 + k);
                put<std::uint64_t>(p, 8, 1);
            };
            tmpl(0, 1, static_cast<std::uint8_t>(abi::direction::REQUEST), req_prefix, 64 + 1024);
            tmpl(1, 2, static_cast<std::uint8_t>(abi::direction::RESPONSE), resp_prefix, 72);
            auto col = [&](std::uint32_t k, std::uint32_t row_bytes, std::uint32_t list, std::uint32_t name_off, std::uint16_t name_len) {
                std::byte* c = r + coff + k * 32;
                put<std::uint32_t>(c, cd::VALUES_OFF, 64);
                put<std::uint32_t>(c, cd::ROW_BYTES, row_bytes);
                put<std::uint32_t>(c, cd::LIST_SIZE, list);
                put<std::uint32_t>(c, cd::NAME_OFF, name_off);
                put<std::uint16_t>(c, cd::NAME_LEN, name_len);
                put<std::uint8_t>(c, cd::ELEM_TYPE, static_cast<std::uint8_t>(abi::elem_type::F32));
            };
            col(0, 1024, 256, 0, 8);
            col(1, 4, 1, 8, 5);
            for (std::uint32_t k = 0; k < 2; ++k) {
                std::byte* p = r + poff + k * 12;
                put<std::uint32_t>(p, pd::PREFIX_OFF, 8);
                put<std::uint32_t>(p, pd::SCALE, 1);
                put<std::uint8_t>(p, pd::KIND, static_cast<std::uint8_t>(abi::patch_kind::ROWS_SCALED));
            }
            fp = 0x57078e8d7a2e3733ULL;  // abi/golden/fingerprints.txt: features:F32[256] score:F32[1]
        }
        put<std::uint32_t>(ctl_, h::MODEL_GEN, 1);
        put<std::uint32_t>(ctl_, h::REGISTRY_EPOCH, 1);
        put<std::uint64_t>(ctl_, h::SCHEMA_FINGERPRINT, fp);
        put<std::uint32_t>(ctl_, h::REGISTRY_OFF, static_cast<std::uint32_t>(abi::CTL_HEADER_LEN));
        put<std::uint32_t>(ctl_, h::REGISTRY_LEN, reg_len);
        put<std::uint64_t>(ctl_, h::MODEL_SEQ, 2);
    }

    void beat() {
        while (running_.load()) {
            if (hb_on_.load()) store_relaxed<std::uint64_t>(ctl_ + abi::control_header::SUPERVISOR_HB_MONO_NS, monotonic_ns());
            std::this_thread::sleep_for(std::chrono::microseconds(500));
        }
    }

    void send_welcome(int fd, std::uint32_t status) {
        std::uint8_t m[64] = {};
        namespace mw = abi::msg_welcome;
        put32(m + mw::MAGIC, static_cast<std::uint32_t>(abi::MSG_MAGIC));
        put32(m + mw::KIND, abi::msg_kind::WELCOME);
        put32(m + mw::STATUS, status);
        put16(m + mw::ABI_MAJOR, abi::ABI_MAJOR);
        put16(m + mw::ABI_MINOR, abi::ABI_MINOR);
        put32(m + mw::CTL_LEN, static_cast<std::uint32_t>(ctl_len_));
        put64(m + mw::DATA_LEN, data_len_);
        put64(m + mw::ARENA_ID, 0x5EED0000ULL + static_cast<std::uint64_t>(::getpid()));
        put64(m + mw::SESSION_ID, 1);
        iovec iov{m, sizeof m};
        alignas(cmsghdr) unsigned char cbuf[CMSG_SPACE(2 * sizeof(int))] = {};
        msghdr mh{};
        mh.msg_iov = &iov;
        mh.msg_iovlen = 1;
        if (status == abi::welcome_status::OK) {
            mh.msg_control = cbuf;
            mh.msg_controllen = sizeof cbuf;
            cmsghdr* c = CMSG_FIRSTHDR(&mh);
            c->cmsg_level = SOL_SOCKET;
            c->cmsg_type = SCM_RIGHTS;
            c->cmsg_len = CMSG_LEN(2 * sizeof(int));
            const int fds[2] = {ctl_fd_, data_fd_};
            std::memcpy(CMSG_DATA(c), fds, sizeof fds);
        }
        (void)::sendmsg(fd, &mh, MSG_NOSIGNAL);
    }

    void accept_loop() {
        int session = -1;
        while (running_.load()) {
            pollfd p[2] = {{listen_fd_, POLLIN, 0}, {session, POLLIN | POLLRDHUP, 0}};
            if (::poll(p, session >= 0 ? 2 : 1, 5) <= 0) continue;
            if (session >= 0 && (p[1].revents != 0)) {
                std::uint8_t b[64];
                const ssize_t n = ::recv(session, b, sizeof b, MSG_DONTWAIT);
                if (n == 0 || (n < 0 && errno != EAGAIN)) {
                    ::close(session);
                    session = -1;
                    client_gone_.store(true);
                }
            }
            if ((p[0].revents & POLLIN) == 0) continue;
            const int fd = ::accept4(listen_fd_, nullptr, nullptr, SOCK_CLOEXEC);
            if (fd < 0) continue;
            std::uint8_t hello[64];
            if (::recv(fd, hello, sizeof hello, 0) != 64) {
                ::close(fd);
                continue;
            }
            if (session >= 0) {
                send_welcome(fd, abi::welcome_status::BUSY);
                busy_sent.fetch_add(1);
                ::close(fd);
                continue;
            }
            send_welcome(fd, abi::welcome_status::OK);
            std::uint8_t ready[64];
            if (::recv(fd, ready, sizeof ready, 0) != 64) {
                ::close(fd);
                continue;
            }
            session = fd;
            client_gone_.store(false);
        }
        if (session >= 0) ::close(session);  // the supervisor's end of the session closes
    }

    /// A C++ model of the chaos worker on ring 0 (worker §4; shm-abi §4.4, §6.1).
    void consume() {
        std::byte* hdr = data_ + ring_off_;
        std::uint64_t head = 0;
        const std::uint64_t cap = c_.ring_cap;
        namespace sr = abi::acc::slot_record;
        namespace ss = abi::slot_state;
        while (running_.load()) {
            std::byte* cell = hdr + abi::ring_header::CELLS_OFF + (head & (cap - 1)) * abi::CELL_LEN;
            if (abi::acc::cell::seq(cell) != head + 1) {
                std::this_thread::sleep_for(std::chrono::microseconds(5));
                continue;
            }
            const std::uint32_t idx = abi::acc::cell::slot_index(cell);
            const std::uint32_t e = abi::acc::cell::slot_epoch(cell);
            const std::uint32_t flags = abi::acc::cell::flags(cell);
            abi::acc::cell::set_seq(cell, head + cap);  // step A
            abi::acc::ring_header::set_head(hdr, head + 1);
            ++head;
            if (flags & abi::cell_flag::VOID) continue;
            if (idx >= c_.slot_count) continue;
            std::byte* rec = data_ + table_off_ + static_cast<std::uint64_t>(idx) * abi::SLOT_RECORD_LEN;
            std::byte* req = data_ + slot_data_off_ + static_cast<std::uint64_t>(idx) * c_.slot_size;
            std::byte* resp = req + c_.req_cap;
            // READY → PROCESSING, keeping WAITER, OWNER = 1.
            bool claimed = false;
            for (int i = 0; i < 64; ++i) {
                std::uint64_t w = sr::word(rec);
                if (epoch(w) != e || state(w) != ss::READY) break;
                if (sr::cas_word(rec, w, with_owner(slot_word(e, ss::PROCESSING) | (w & abi::slot_word::WAITER_MASK), 1))) {
                    claimed = true;
                    break;
                }
            }
            if (!claimed) continue;
            sr::set_t_dequeue_ns(rec, monotonic_ns());
            std::uint32_t status = abi::status::OK;
            std::uint32_t resp_len = 0;
            if (c_.frames) {
                float x[256];
                std::memcpy(x, req + 64, sizeof x);
                float sum = 0;
                for (float v : x) sum += v;
                const std::uint32_t want = (1U << 16) | 2U;
                if (sr::resp_stamp(rec) != want) {
                    std::memcpy(resp, ctl_ + abi::CTL_HEADER_LEN + 328, 64);
                    sr::set_resp_stamp(rec, want);
                }
                put<std::uint64_t>(resp, 8, fetch_once<std::uint64_t>(req + 8));
                std::memcpy(resp + 64, &sum, 4);
                resp_len = 72;
            } else {
                const std::uint32_t req_len = sr::req_len(rec);
                std::uint64_t o = 0, arg = 0;
                if (req_len >= 40 && req_len <= c_.req_cap) {
                    std::uint64_t magic;
                    std::memcpy(&magic, req, 8);
                    if (magic == kMagic) {
                        std::memcpy(&o, req + 8, 8);
                        std::memcpy(&arg, req + 16, 8);
                    }
                }
                if (o == op::DELAY) std::this_thread::sleep_for(std::chrono::microseconds(arg));
                if (o == op::HANG) {
                    const std::uint64_t until = arg == 0 ? UINT64_MAX : monotonic_ns() + arg * 1'000'000;
                    while (running_.load() && monotonic_ns() < until) std::this_thread::sleep_for(std::chrono::microseconds(200));
                }
                while (hold_.load() && running_.load()) std::this_thread::sleep_for(std::chrono::microseconds(100));
                if (o == op::STATUS) {
                    status = static_cast<std::uint32_t>(arg);
                } else {
                    const std::uint64_t ans = fnv1a64(reinterpret_cast<const std::uint8_t*>(req), req_len <= c_.req_cap ? req_len : 0);
                    std::memcpy(resp, &ans, 8);
                    resp_len = 8;
                }
            }
            sr::set_resp_len(rec, resp_len);
            sr::set_status(rec, status);
            sr::set_model_gen(rec, 1);
            sr::set_t_done_ns(rec, monotonic_ns());
            // PROCESSING → DONE (clears WAITER and OWNER), or ABANDONED → FREE.
            for (int i = 0; i < 64; ++i) {
                std::uint64_t w = sr::word(rec);
                if (epoch(w) != e) break;
                if (state(w) == ss::ABANDONED) {
                    if (sr::cas_word(rec, w, slot_word(e, ss::FREE))) break;
                } else if (state(w) == ss::PROCESSING) {
                    const bool wake = waiter(w);
                    if (sr::cas_word(rec, w, slot_word(e, ss::DONE))) {
                        served.fetch_add(1);
                        if (wake) futex_wake(rec + abi::slot_record::WORD, 1);
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }

    std::string name_;
    Config c_;
    int ctl_fd_ = -1, data_fd_ = -1, listen_fd_ = -1;
    std::byte* ctl_ = nullptr;
    std::byte* data_ = nullptr;
    std::size_t ctl_len_ = 0, data_len_ = 0;
    std::uint64_t ring_off_ = 0, table_off_ = 0, slot_data_off_ = 0;
    std::atomic<bool> running_{false}, hb_on_{true}, hold_{false}, client_gone_{false};
    std::thread hb_, acceptor_, worker_;

public:
    /// Unmap and close everything; the client must have left first.
    void destroy() {
        stop();
        if (ctl_ != nullptr) ::munmap(ctl_, ctl_len_);
        if (data_ != nullptr) ::munmap(data_, data_len_);
        if (ctl_fd_ >= 0) ::close(ctl_fd_);
        if (data_fd_ >= 0) ::close(data_fd_);
        ctl_ = data_ = nullptr;
        ctl_fd_ = data_fd_ = -1;
    }
};

/// A unique abstract socket name per process and test.
inline std::string unique_endpoint(const char* tag) {
    static std::atomic<int> n{0};
    return "@scz-cpp-test-" + std::to_string(::getpid()) + "-" + tag + "-" + std::to_string(n.fetch_add(1));
}

/// Arena mappings of this process (lines of /proc/self/maps naming memfd:scz-) that the client
/// holds, i.e. excluding the fake sidecar's own: counted by device-inode pairs is overkill;
/// tests compare counts before and after.
inline int scz_mappings() {
    FILE* f = std::fopen("/proc/self/maps", "r");
    if (f == nullptr) return -1;
    char line[512];
    int n = 0;
    while (std::fgets(line, sizeof line, f) != nullptr)
        if (std::strstr(line, "memfd:scz-") != nullptr) ++n;
    std::fclose(f);
    return n;
}

}  // namespace scztest
