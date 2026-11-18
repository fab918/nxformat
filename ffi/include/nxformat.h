/* nxformat C API: runs the nxconvert commands inside the calling process.
 *
 * Every command and option of `nxconvert` is available: pass the same
 * arguments, without the program name. Each output line goes to the line
 * callback instead of stdout/stderr. Add "--frontend" to get the
 * machine-readable `@` lines (see nxconvert --help).
 *
 * Strings are UTF-8 and NUL-terminated. Paths are UTF-8 on every platform.
 * Calls on different threads run independently; one run blocks its thread
 * until it ends, so frontends call nxformat_run from a worker thread.
 */
#ifndef NXFORMAT_H
#define NXFORMAT_H

#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32) && !defined(NXFORMAT_STATIC)
#define NXFORMAT_API __declspec(dllimport)
#else
#define NXFORMAT_API
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* Bumped when a declaration below changes in an incompatible way. */
#define NXFORMAT_ABI_VERSION 1

enum {
    NXFORMAT_OK = 0,
    NXFORMAT_FAILED = 1,    /* The command failed; see the error text. */
    NXFORMAT_CANCELLED = 2, /* The cancel handle was set; nothing was left half-written. */
    NXFORMAT_INVALID = 3,   /* Null/non-UTF-8 arguments or an excessive count. */
    NXFORMAT_INTERNAL = 4   /* A bug in nxformat; the error text says where. */
};

/* Called once per output line, without the line break. `is_error` is non-zero
 * for diagnostic lines. `line` is valid only during the call. It may be called
 * from a helper thread of the run, never after nxformat_run returns. Calls
 * within one run are serialized. Independent runs may call concurrently. */
typedef void (*nxformat_line_fn)(void* user, int is_error, const char* line);

typedef struct nxformat_cancel nxformat_cancel;

/* NXFORMAT_ABI_VERSION of the loaded library. */
NXFORMAT_API uint32_t nxformat_abi_version(void);
/* Library version, such as "1.0.0". Static string. */
NXFORMAT_API const char* nxformat_version(void);

/* A cancel handle, shared between the run and the thread that cancels it.
 * Set it from any thread; the run stops at its next check. Free it only
 * after the run that uses it has returned. */
NXFORMAT_API nxformat_cancel* nxformat_cancel_new(void);
NXFORMAT_API void nxformat_cancel_set(nxformat_cancel* cancel);
NXFORMAT_API void nxformat_cancel_free(nxformat_cancel* cancel);

/* Runs one command. `on_line` and `cancel` may be null: lines then go to
 * stdout/stderr, and the run cannot be cancelled. On failure, `error` (when
 * not null) receives the message, truncated to `error_size` bytes with its
 * NUL. Returns one of the NXFORMAT_* codes. */
NXFORMAT_API int nxformat_run(const char* const* args, size_t count, nxformat_line_fn on_line,
                              void* user, nxformat_cancel* cancel, char* error,
                              size_t error_size);

/* Reads `size` bytes at `offset` into `buffer` and returns 0, or non-zero on
 * failure. Calls for one source are serialized but may come from a helper
 * thread of a run. */
typedef int (*nxformat_read_fn)(void* user, uint64_t offset, void* buffer, size_t size);

typedef struct nxformat_source nxformat_source;

/* A source the caller reads, for example a game the emulator decrypts on the
 * fly: nxformat only sees the plaintext bytes `read` returns, never keys.
 * `name` is the file name, whose extension gives the package type (".nsp",
 * ".xci"). Pass nxformat_source_path() to nxformat_run wherever a command takes
 * a source file ("pack", "add", "scan"). Returns null on a null or non-UTF-8
 * argument. Free it only after the runs that use it have returned. */
NXFORMAT_API nxformat_source* nxformat_source_new(const char* name, uint64_t size,
                                                  nxformat_read_fn read, void* user);
/* The path to give commands; valid until the source is freed. */
NXFORMAT_API const char* nxformat_source_path(const nxformat_source* source);
NXFORMAT_API void nxformat_source_free(nxformat_source* source);

#ifdef __cplusplus
}
#endif

#endif /* NXFORMAT_H */
