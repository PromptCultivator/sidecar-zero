/* Spike S5 - allocation recorder. Throwaway.
 *
 * LD_PRELOADed for the zero-copy test only. It interposes the C allocation entry points for the whole
 * process (ONNX Runtime and libstdc++ included, since both reach malloc through the dynamic linker),
 * forwards to glibc, and records the size of every allocation made between mhook_begin() and
 * mhook_end(). If the runtime copied a request or a response into a buffer of its own, the buffer
 * would show up here with the size of that request or response.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stddef.h>

extern void *__libc_malloc(size_t);
extern void __libc_free(void *);
extern void *__libc_calloc(size_t, size_t);
extern void *__libc_realloc(void *, size_t);
extern void *__libc_memalign(size_t, size_t);

#define MAXREC 4096
static volatile int on;
static size_t n_alloc, n_bytes, rec[MAXREC];

static void note(size_t sz) {
    if (!on) return;
    if (n_alloc < MAXREC) rec[n_alloc] = sz;
    n_alloc++;
    n_bytes += sz;
}

void *malloc(size_t n) { note(n); return __libc_malloc(n); }
void free(void *p) { __libc_free(p); }
void *calloc(size_t a, size_t b) { note(a * b); return __libc_calloc(a, b); }
void *realloc(void *p, size_t n) { note(n); return __libc_realloc(p, n); }
void *memalign(size_t al, size_t n) { note(n); return __libc_memalign(al, n); }
void *aligned_alloc(size_t al, size_t n) { note(n); return __libc_memalign(al, n); }
int posix_memalign(void **out, size_t al, size_t n) {
    note(n);
    void *p = __libc_memalign(al, n);
    if (!p) return ENOMEM;
    *out = p;
    return 0;
}

void mhook_begin(void) { n_alloc = 0; n_bytes = 0; on = 1; }

/* Returns the number of allocations since mhook_begin(); *sizes points at up to MAXREC of their sizes. */
size_t mhook_end(size_t *bytes, const size_t **sizes) {
    on = 0;
    *bytes = n_bytes;
    *sizes = rec;
    return n_alloc;
}
