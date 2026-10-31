// Strict Simulator mode interposer (CLAUDE.md section 12a).
//
// Linked into Simulator builds only. dyld applies the __DATA,__interpose tuples
// below to every image, so any attempt to start a process, or to touch the host
// file system outside the simulated device, aborts with a clear message and a
// backtrace, exactly where a real iPhone would fail.
//
// Environment:
//   ZIOS_STRICT_SANDBOX=abort|warn|off   (default abort)
//   ZIOS_STRICT_ALLOWED_ROOTS=/a:/b      extra allowed roots (user-picked folders)

#include <dirent.h>
#include <dlfcn.h>
#include <execinfo.h>
#include <fcntl.h>
#include <pthread.h>
#include <spawn.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

// Some of these are marked unavailable in the iOS SDK headers, so bind to the
// symbols directly instead of relying on the declarations.
extern int zios_real_posix_spawn(pid_t *, const char *, const posix_spawn_file_actions_t *,
                                 const posix_spawnattr_t *, char *const[], char *const[])
    __asm("_posix_spawn");
extern int zios_real_posix_spawnp(pid_t *, const char *, const posix_spawn_file_actions_t *,
                                  const posix_spawnattr_t *, char *const[], char *const[])
    __asm("_posix_spawnp");
extern pid_t zios_real_fork(void) __asm("_fork");
extern pid_t zios_real_vfork(void) __asm("_vfork");
extern int zios_real_execve(const char *, char *const[], char *const[]) __asm("_execve");
extern int zios_real_system(const char *) __asm("_system");

#define ZIOS_INTERPOSE(replacement, original)                                                    \
    __attribute__((used)) static const struct {                                                  \
        const void *replacement_function;                                                        \
        const void *original_function;                                                          \
    } zios_interpose_##original __attribute__((section("__DATA,__interpose"))) = {               \
        (const void *)(unsigned long)&replacement, (const void *)(unsigned long)&original}

static __thread int zios_reporting;

static void zios_write_report(const char *kind, const char *detail, void *caller) {
    char message[2048];
    Dl_info info;
    const char *image = "?";
    const char *symbol = "?";
    if (caller && dladdr(caller, &info)) {
        if (info.dli_fname) image = info.dli_fname;
        if (info.dli_sname) symbol = info.dli_sname;
    }
    snprintf(message, sizeof message,
             "ZIOS STRICT MODE VIOLATION: %s (%s)\n"
             "  called from %s in %s\n"
             "  A real iPhone forbids this. Route it through crates/inproc or the app container.\n",
             kind, detail ? detail : "", symbol, image);
    fputs(message, stderr);

    void *frames[64];
    int count = backtrace(frames, 64);
    backtrace_symbols_fd(frames, count, STDERR_FILENO);

    // Leave a file next to the app logs so CI and the next launch can see it.
    const char *home = getenv("HOME");
    if (home) {
        char path[1024];
        snprintf(path, sizeof path, "%s/Library/Logs/Zed/strict-violation.txt", home);
        int file = open(path, O_WRONLY | O_CREAT | O_APPEND, 0644);
        if (file >= 0) {
            write(file, message, strlen(message));
            backtrace_symbols_fd(frames, count, file);
            close(file);
        }
    }
}

__attribute__((noreturn)) static void zios_violation(const char *kind, const char *detail,
                                                     void *caller) {
    zios_reporting = 1;
    zios_write_report(kind, detail, caller);
    abort();
}

// ---------------------------------------------------------------------------
// Process creation: always forbidden on iOS.

static int zios_posix_spawn(pid_t *pid, const char *path, const posix_spawn_file_actions_t *actions,
                            const posix_spawnattr_t *attributes, char *const arguments[],
                            char *const environment[]) {
    zios_violation("posix_spawn", path, __builtin_return_address(0));
}
ZIOS_INTERPOSE(zios_posix_spawn, zios_real_posix_spawn);

static int zios_posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *actions,
                             const posix_spawnattr_t *attributes, char *const arguments[],
                             char *const environment[]) {
    zios_violation("posix_spawnp", file, __builtin_return_address(0));
}
ZIOS_INTERPOSE(zios_posix_spawnp, zios_real_posix_spawnp);

static pid_t zios_fork(void) { zios_violation("fork", NULL, __builtin_return_address(0)); }
ZIOS_INTERPOSE(zios_fork, zios_real_fork);

static pid_t zios_vfork(void) { zios_violation("vfork", NULL, __builtin_return_address(0)); }
ZIOS_INTERPOSE(zios_vfork, zios_real_vfork);

static int zios_execve(const char *path, char *const arguments[], char *const environment[]) {
    zios_violation("execve", path, __builtin_return_address(0));
}
ZIOS_INTERPOSE(zios_execve, zios_real_execve);

static int zios_system(const char *command) {
    zios_violation("system", command, __builtin_return_address(0));
}
ZIOS_INTERPOSE(zios_system, zios_real_system);

// ---------------------------------------------------------------------------
// Sandbox: the device app can only reach its container and user-picked folders.
//
// The check is a deny list of host locations (home folders, temp folders, mounted
// volumes) rather than an allow list, because Apple's frameworks legitimately
// read many system paths that differ between Simulator runtimes.

enum zios_sandbox_mode { ZIOS_SANDBOX_ABORT, ZIOS_SANDBOX_WARN, ZIOS_SANDBOX_OFF };

#define ZIOS_MAX_ROOTS 64
static pthread_once_t zios_sandbox_once = PTHREAD_ONCE_INIT;
static pthread_mutex_t zios_roots_lock = PTHREAD_MUTEX_INITIALIZER;
static enum zios_sandbox_mode zios_mode = ZIOS_SANDBOX_ABORT;
static char *zios_roots[ZIOS_MAX_ROOTS];
static int zios_root_count;

static void zios_add_root_locked(const char *root) {
    if (zios_root_count >= ZIOS_MAX_ROOTS || !root || !*root) return;
    zios_roots[zios_root_count++] = strdup(root);
}

static void zios_sandbox_init(void) {
    const char *mode = getenv("ZIOS_STRICT_SANDBOX");
    if (mode && strcmp(mode, "warn") == 0) zios_mode = ZIOS_SANDBOX_WARN;
    if (mode && strcmp(mode, "off") == 0) zios_mode = ZIOS_SANDBOX_OFF;

    pthread_mutex_lock(&zios_roots_lock);
    // HOME is .../Devices/<UDID>/data/Containers/Data/Application/<UUID>. Allow
    // the whole simulated device ("data") so system services' files are reachable,
    // as they are on a device.
    const char *home = getenv("HOME");
    if (home) {
        const char *containers = strstr(home, "/data/Containers/");
        if (containers) {
            size_t length = (size_t)(containers - home) + strlen("/data/");
            char *device_root = strndup(home, length);
            zios_add_root_locked(device_root);
            free(device_root);
        } else {
            zios_add_root_locked(home);
        }
    }
    const char *extra = getenv("ZIOS_STRICT_ALLOWED_ROOTS");
    if (extra) {
        char *copy = strdup(extra);
        for (char *cursor = copy, *root; (root = strsep(&cursor, ":"));) zios_add_root_locked(root);
        free(copy);
    }
    pthread_mutex_unlock(&zios_roots_lock);
}

// Called from Rust when the user picks a folder with the document picker.
__attribute__((visibility("default"))) void zios_strict_allow_root(const char *root) {
    pthread_once(&zios_sandbox_once, zios_sandbox_init);
    pthread_mutex_lock(&zios_roots_lock);
    zios_add_root_locked(root);
    pthread_mutex_unlock(&zios_roots_lock);
}

__attribute__((visibility("default"))) int zios_strict_interposer_loaded(void) { return 1; }

static int zios_has_prefix(const char *path, const char *prefix) {
    size_t length = strlen(prefix);
    if (strncmp(path, prefix, length) != 0) return 0;
    return path[length] == '\0' || path[length] == '/' || prefix[length - 1] == '/';
}

static const char *const zios_host_locations[] = {
    "/Users/", "/Volumes/", "/tmp/", "/private/tmp/", "/var/folders/",
    "/private/var/folders/", "/opt/", "/usr/local/", "/home/",
};

static int zios_path_is_forbidden(const char *path) {
    if (!path || path[0] != '/') return 0;  // Relative paths resolve inside the container.
    int host_location = 0;
    for (size_t index = 0; index < sizeof zios_host_locations / sizeof *zios_host_locations; index++) {
        if (zios_has_prefix(path, zios_host_locations[index])) {
            host_location = 1;
            break;
        }
    }
    if (!host_location) return 0;
    int allowed = 0;
    pthread_mutex_lock(&zios_roots_lock);
    for (int index = 0; index < zios_root_count; index++) {
        if (zios_has_prefix(path, zios_roots[index])) {
            allowed = 1;
            break;
        }
    }
    pthread_mutex_unlock(&zios_roots_lock);
    return !allowed;
}

static void zios_check_path(const char *operation, const char *path, void *caller) {
    if (zios_reporting) return;
    pthread_once(&zios_sandbox_once, zios_sandbox_init);
    if (zios_mode == ZIOS_SANDBOX_OFF || !zios_path_is_forbidden(path)) return;
    char detail[1200];
    snprintf(detail, sizeof detail, "%s outside the app sandbox: %s", operation, path);
    if (zios_mode == ZIOS_SANDBOX_WARN) {
        zios_reporting = 1;
        zios_write_report("sandbox", detail, caller);
        zios_reporting = 0;
        return;
    }
    zios_violation("sandbox", detail, caller);
}

static int zios_open(const char *path, int flags, ...) {
    zios_check_path("open", path, __builtin_return_address(0));
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list arguments;
        va_start(arguments, flags);
        mode = (mode_t)va_arg(arguments, int);
        va_end(arguments);
    }
    return open(path, flags, mode);
}
ZIOS_INTERPOSE(zios_open, open);

static int zios_openat(int directory, const char *path, int flags, ...) {
    zios_check_path("openat", path, __builtin_return_address(0));
    mode_t mode = 0;
    if (flags & O_CREAT) {
        va_list arguments;
        va_start(arguments, flags);
        mode = (mode_t)va_arg(arguments, int);
        va_end(arguments);
    }
    return openat(directory, path, flags, mode);
}
ZIOS_INTERPOSE(zios_openat, openat);

static int zios_stat(const char *path, struct stat *buffer) {
    zios_check_path("stat", path, __builtin_return_address(0));
    return stat(path, buffer);
}
ZIOS_INTERPOSE(zios_stat, stat);

static int zios_lstat(const char *path, struct stat *buffer) {
    zios_check_path("lstat", path, __builtin_return_address(0));
    return lstat(path, buffer);
}
ZIOS_INTERPOSE(zios_lstat, lstat);

static int zios_access(const char *path, int mode) {
    zios_check_path("access", path, __builtin_return_address(0));
    return access(path, mode);
}
ZIOS_INTERPOSE(zios_access, access);

static DIR *zios_opendir(const char *path) {
    zios_check_path("opendir", path, __builtin_return_address(0));
    return opendir(path);
}
ZIOS_INTERPOSE(zios_opendir, opendir);

static int zios_mkdir(const char *path, mode_t mode) {
    zios_check_path("mkdir", path, __builtin_return_address(0));
    return mkdir(path, mode);
}
ZIOS_INTERPOSE(zios_mkdir, mkdir);

static int zios_unlink(const char *path) {
    zios_check_path("unlink", path, __builtin_return_address(0));
    return unlink(path);
}
ZIOS_INTERPOSE(zios_unlink, unlink);

static int zios_rename(const char *from, const char *to) {
    zios_check_path("rename", from, __builtin_return_address(0));
    zios_check_path("rename", to, __builtin_return_address(0));
    return rename(from, to);
}
ZIOS_INTERPOSE(zios_rename, rename);
