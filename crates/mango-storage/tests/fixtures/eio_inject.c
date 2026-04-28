/*
 * LD_PRELOAD shim for ROADMAP:826 — fsync-EIO crash-recovery tests.
 *
 * When MANGO_TEST_INJECT_FSYNC_EIO=1 is set in the environment at
 * process start, this shim overrides fsync(2) and fdatasync(2) to
 * fail with errno=EIO. All other syscalls fall through to libc via
 * dlsym(RTLD_NEXT, ...).
 *
 * Linux-only. macOS uses fcntl(F_FULLFSYNC) for File::sync_data
 * which is variadic and not addressed here. The test that loads
 * this shim is gated #[cfg(target_os = "linux")].
 *
 * Build (matches crash_recovery_eio.rs::build_shim):
 *   cc -shared -fPIC -o libeio_inject.so eio_inject.c -ldl
 * (-ldl AFTER source: GNU ld defaults to --as-needed; libraries
 *  listed before the source they satisfy are dropped.)
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <dlfcn.h>

static int inject = 0;
static int (*real_fsync)(int) = NULL;
static int (*real_fdatasync)(int) = NULL;

__attribute__((constructor))
static void mango_eio_inject_init(void) {
    const char *env = getenv("MANGO_TEST_INJECT_FSYNC_EIO");
    inject = (env != NULL && env[0] == '1');
    real_fsync = dlsym(RTLD_NEXT, "fsync");
    real_fdatasync = dlsym(RTLD_NEXT, "fdatasync");
    /*
     * Canary: emit one line to stderr at constructor time so a
     * sandbox-stripped LD_PRELOAD or a chained interceptor that
     * swallows our load shows up as "no canary in stderr" in the
     * parent's failure dump, not a confusing 101-exit with no
     * attribution. Two distinct strings so parent can tell
     * "loaded with injection" from "loaded without injection".
     */
    if (inject) {
        fputs("eio_inject: armed\n", stderr);
    } else {
        fputs("eio_inject: loaded but inactive\n", stderr);
    }
}

int fsync(int fd) {
    if (inject) {
        errno = EIO;
        return -1;
    }
    /*
     * Defensive: if dlsym(RTLD_NEXT, "fsync") returned NULL at
     * constructor time (some other interceptor consumed the
     * symbol resolution, or RTLD_NEXT had nothing further to
     * resolve), refuse rather than NULL-deref. Today every parent
     * caller pairs LD_PRELOAD with MANGO_TEST_INJECT_FSYNC_EIO=1,
     * so this branch is unreachable; the guard exists so a future
     * caller that loads the shim "inactive" doesn't crash.
     */
    if (real_fsync == NULL) {
        errno = EIO;
        return -1;
    }
    return real_fsync(fd);
}

int fdatasync(int fd) {
    if (inject) {
        errno = EIO;
        return -1;
    }
    if (real_fdatasync == NULL) {
        errno = EIO;
        return -1;
    }
    return real_fdatasync(fd);
}
