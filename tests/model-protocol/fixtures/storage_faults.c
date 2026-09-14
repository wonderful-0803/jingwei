/* Linux-only private child-process harness. Never linked into Jingwei.
 * Write failures come from the kernel's RLIMIT_FSIZE. Sync failures are
 * injected at libc's syscall boundary, not simulated power loss. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <unistd.h>

static struct rlimit original_limit;
static int limited;

static int armed(void) {
    const char *arm = getenv("JW_FAULT_ARM");
    return arm && access(arm, F_OK) == 0;
}
static int selected(int fd) {
    const char *target = getenv("JW_FAULT_TARGET");
    char name[64], path[PATH_MAX];
    snprintf(name, sizeof(name), "/proc/self/fd/%d", fd);
    ssize_t len = readlink(name, path, sizeof(path) - 1);
    if (!target || len < 0) return 0;
    path[len] = 0;
    return strcmp(path, target) == 0;
}
static void hit(void) {
    const char *path = getenv("JW_FAULT_HIT");
    if (!path) _exit(90);
    int fd = open(path, O_CREAT | O_WRONLY, 0600);
    if (fd < 0) _exit(91);
    close(fd);
}
ssize_t write(int fd, const void *buffer, size_t size) {
    ssize_t (*real_write)(int, const void *, size_t) = dlsym(RTLD_NEXT, "write");
    if (!armed() && limited) {
        if (setrlimit(RLIMIT_FSIZE, &original_limit)) _exit(92);
        limited = 0;
    }
    if (armed() && selected(fd)) {
        const char *mode = getenv("JW_FAULT_MODE");
        if (!strcmp(mode, "write-zero") || !strcmp(mode, "write-partial")) {
            if (!limited) {
                struct stat state;
                if (fstat(fd, &state) || getrlimit(RLIMIT_FSIZE, &original_limit)) _exit(93);
                struct rlimit limit = original_limit;
                limit.rlim_cur = state.st_size + (!strcmp(mode, "write-partial") ? 17 : 0);
                signal(SIGXFSZ, SIG_IGN);
                if (setrlimit(RLIMIT_FSIZE, &limit)) _exit(94);
                limited = 1;
            }
            ssize_t result = real_write(fd, buffer, size);
            int saved_errno = errno;
            if (result < 0 && saved_errno == EFBIG) hit();
            errno = saved_errno;
            return result;
        }
        if (!strcmp(mode, "crash-after-write")) {
            ssize_t result = real_write(fd, buffer, size);
            if (result == (ssize_t)size) { hit(); _exit(86); }
            return result;
        }
    }
    return real_write(fd, buffer, size);
}
static int sync_file(int fd, const char *symbol) {
    int (*real_sync)(int) = dlsym(RTLD_NEXT, symbol);
    if (armed() && selected(fd)) {
        const char *mode = getenv("JW_FAULT_MODE");
        if (!strcmp(mode, "sync-before") || !strcmp(mode, "sync-after")) {
            if (!strcmp(mode, "sync-after") && real_sync(fd)) return -1;
            hit(); errno = EIO; return -1;
        }
    }
    return real_sync(fd);
}
int fsync(int fd) { return sync_file(fd, "fsync"); }
int fdatasync(int fd) { return sync_file(fd, "fdatasync"); }
