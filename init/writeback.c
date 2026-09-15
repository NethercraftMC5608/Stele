#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <sched.h>
#include <signal.h>
#include <pthread.h>
#include <stdint.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/syscall.h>

/* Shared writeback: a MAP_SHARED mapping is the same pages as the file,
 * so a write through the mapping must be readable through the file --
 * after munmap, and after msync while still mapped.
 *
 * One PIE binary in the shape of runtime.c/soak.c: a marker per check,
 * perror and a distinct exit code on failure.
 */
static int check_munmap_writeback(void) {
    char path[] = "/tmp/wbXXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) { perror("mkstemp"); return 1; }
    if (ftruncate(fd, 8192) < 0) { perror("ftruncate"); return 1; }
    char *mp = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (mp == MAP_FAILED) { perror("mmap shared"); return 1; }
    memcpy(mp, "written-shared", 15);
    memcpy(mp + 4096, "second-page!", 12);
    if (munmap(mp, 8192) < 0) { perror("munmap"); return 1; }
    // Read back through the file: the mapping is gone, the bytes stay.
    char buf[16] = { 0 };
    if (pread(fd, buf, 15, 0) != 15 || memcmp(buf, "written-shared", 15) != 0) {
        fprintf(stderr, "writeback: first page mismatch %15s\n", buf);
        return 1;
    }
    memset(buf, 0, sizeof buf);
    if (pread(fd, buf, 12, 4096) != 12 || memcmp(buf, "second-page!", 12) != 0) {
        fprintf(stderr, "writeback: second page mismatch\n");
        return 1;
    }
    close(fd);
    unlink(path);
    puts("WB_MUNMAP_OK");
    fflush(stdout);
    return 0;
}

static int check_msync_writeback(void) {
    char path[] = "/tmp/msXXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) { perror("mkstemp msync"); return 2; }
    if (ftruncate(fd, 4096) < 0) { perror("ftruncate msync"); return 2; }
    char *mp = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (mp == MAP_FAILED) { perror("mmap shared msync"); return 2; }
    memcpy(mp, "msync-bytes", 12);
    if (msync(mp, 4096, MS_SYNC) < 0) { perror("msync"); return 2; }
    // Still mapped, already visible through the file.
    char buf[16];
    memset(buf, 0, sizeof buf);
    ssize_t nr = pread(fd, buf, 12, 0);
    if (nr != 12 || memcmp(buf, "msync-bytes", 12) != 0) {
        fprintf(stderr, "msync: bytes not visible\n");
        return 2;
    }
    if (munmap(mp, 4096) < 0) { perror("munmap msync"); return 2; }
    close(fd);
    unlink(path);
    puts("WB_MSYNC_OK");
    fflush(stdout);
    return 0;
}

static int check_private_unaffected(void) {
    // A MAP_PRIVATE mapping of the same file must not see shared writes
    // made after it was taken, and must not write back its own.
    char path[] = "/tmp/pvXXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) { perror("mkstemp private"); return 3; }
    if (ftruncate(fd, 4096) < 0) { perror("ftruncate private"); return 3; }
    if (write(fd, "original", 9) != 9) { perror("write original"); return 3; }
    char *priv = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if (priv == MAP_FAILED) { perror("mmap private"); return 3; }
    char *sh = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (sh == MAP_FAILED) { perror("mmap shared"); return 3; }
    memcpy(sh, "changed!!", 10);
    if (munmap(sh, 4096) < 0) { perror("munmap shared"); return 3; }
    // The private snapshot predates the shared write: still "original".
    if (memcmp(priv, "original", 8) != 0) {
        fprintf(stderr, "private snapshot polluted: %.8s\n", priv);
        return 3;
    }
    // And the private mapping's own writes never reach the file.
    memcpy(priv, "priv-only", 10);
    if (munmap(priv, 4096) < 0) { perror("munmap private"); return 3; }
    char buf[10] = { 0 };
    if (pread(fd, buf, 9, 0) != 9 || memcmp(buf, "changed!!", 9) != 0) {
        fprintf(stderr, "private write leaked or shared lost: %.9s\n", buf);
        return 3;
    }
    close(fd);
    unlink(path);
    puts("WB_PRIVATE_OK");
    fflush(stdout);
    return 0;
}

static int check_anon_shared(void) {
    // MAP_SHARED|MAP_ANONYMOUS: zeroed pages, writable, unmappable, and --
    // with no file -- nothing to write back. Survives as ordinary memory.
    char *mp = mmap(NULL, 8192, PROT_READ | PROT_WRITE,
                    MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (mp == MAP_FAILED) { perror("mmap anon shared"); return 4; }
    for (int i = 0; i < 8192; i++) {
        if (mp[i] != 0) {
            fprintf(stderr, "anon shared not zeroed at %d\n", i);
            return 4;
        }
    }
    memcpy(mp, "anon-data", 10);
    if (memcmp(mp, "anon-data", 9) != 0) {
        fprintf(stderr, "anon shared readback failed\n");
        return 4;
    }
    if (munmap(mp, 8192) < 0) { perror("munmap anon"); return 4; }
    puts("WB_ANON_OK");
    fflush(stdout);
    return 0;
}

static int exec_shared_child(void *arg) {
    char *args[] = { arg, "--vfork-exec-child", NULL };
    char *env[] = { NULL };
    execve(arg, args, env);
    _exit(127);
}

static int check_vfork_exec_shared(const char *self) {
    char path[] = "/tmp/vfXXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) { perror("mkstemp vfork"); return 5; }
    if (ftruncate(fd, 8192) < 0) { perror("ftruncate vfork"); return 5; }
    unsigned char *file = mmap(NULL, 8192, PROT_READ | PROT_WRITE,
                               MAP_SHARED, fd, 0);
    unsigned char *anon = mmap(NULL, 8192, PROT_READ | PROT_WRITE,
                               MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    void *stack = mmap(NULL, 65536, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (file == MAP_FAILED || anon == MAP_FAILED || stack == MAP_FAILED) {
        perror("mmap vfork"); return 5;
    }
    memset(file, 0x5a, 8192);
    memset(anon, 0xa5, 8192);
    pid_t pid = clone(exec_shared_child, (char *)stack + 65536,
                      CLONE_VM | CLONE_VFORK | SIGCHLD, (void *)self);
    if (pid < 0) { perror("clone vfork"); return 5; }
    int status;
    pid_t waited;
    do { waited = waitpid(pid, &status, 0); } while (waited < 0 && errno == EINTR);
    if (waited != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 37) {
        fprintf(stderr, "vfork exec child did not exit 37\n"); return 5;
    }
    unsigned char *alias = mmap(NULL, 8192, PROT_READ | PROT_WRITE,
                                MAP_SHARED, fd, 0);
    if (alias == MAP_FAILED) { perror("mmap vfork alias"); return 5; }
    for (int i = 0; i < 8192; i++) {
        if (file[i] != 0x5a || alias[i] != 0x5a || anon[i] != 0xa5) {
            fprintf(stderr, "vfork exec lost parent shared bytes at %d\n", i);
            return 5;
        }
    }
    file[0] = 0x31;
    alias[4096] = 0x72;
    if (alias[0] != 0x31 || file[4096] != 0x72) {
        fprintf(stderr, "vfork exec lost parent shared pool ownership\n"); return 5;
    }
    if (msync(file, 8192, MS_SYNC) < 0) { perror("msync vfork"); return 5; }
    unsigned char buf[8192];
    if (pread(fd, buf, sizeof buf, 0) != sizeof buf || memcmp(buf, file, sizeof buf)) {
        fprintf(stderr, "vfork exec lost parent writeback\n"); return 5;
    }
    if (munmap(alias, 8192) || munmap(file, 8192) ||
        munmap(anon, 8192) || munmap(stack, 65536)) {
        perror("munmap vfork"); return 5;
    }
    close(fd);
    unlink(path);
    puts("WB_VFORK_EXEC_OK");
    fflush(stdout);
    return 0;
}

static int check_fork_fd_flags(void) {
    char path[] = "/tmp/fdXXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) { perror("mkstemp fd flags"); return 6; }
    int dense[16];
    for (int i = 0; i < 16; i++) {
        dense[i] = dup(fd);
        if (dense[i] < 0) { perror("dup dense flags"); return 6; }
    }
    int plain = fcntl(fd, F_DUPFD, 32);
    int marked = fcntl(fd, F_DUPFD_CLOEXEC, 40);
    if (plain < 0 || marked < 0) { perror("dup fd flags"); return 6; }
    pid_t pid = fork();
    if (pid < 0) { perror("fork fd flags"); return 6; }
    if (pid == 0) {
        for (int i = 0; i < 16; i++) {
            int flags = fcntl(dense[i], F_GETFD);
            if (flags != 0) {
                fprintf(stderr, "fork dense fd=%d flags=%d\n", dense[i], flags);
                _exit(6);
            }
        }
        int a = fcntl(fd, F_GETFD);
        int b = fcntl(plain, F_GETFD);
        int c = fcntl(marked, F_GETFD);
        if (a != 0 || b != 0 || c != FD_CLOEXEC) {
            fprintf(stderr, "fork fd flags: dense=%d sparse=%d marked=%d\n", a, b, c);
            _exit(6);
        }
        if (fcntl(fd, F_SETFD, FD_CLOEXEC) < 0) _exit(6);
        _exit(0);
    }
    int status;
    pid_t waited;
    do { waited = waitpid(pid, &status, 0); } while (waited < 0 && errno == EINTR);
    if (waited != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 0 ||
        fcntl(fd, F_GETFD) != 0 || fcntl(marked, F_GETFD) != FD_CLOEXEC) {
        fprintf(stderr, "fork descriptor flags not preserved or isolated\n"); return 6;
    }
    for (int i = 0; i < 16; i++) close(dense[i]);
    close(marked);
    close(plain);
    close(fd);
    unlink(path);
    puts("WB_FORK_FD_FLAGS_OK");
    fflush(stdout);
    return 0;
}

struct exec_fd_args { const char *self; pid_t parent; };

static int exec_fd_child(void *arg) {
    struct exec_fd_args *config = arg;
    const char *self = config->self;
    if (syscall(246, config->parent) != 0) _exit(13);
    int fd = open("/tmp/exec-fds", O_CREAT | O_RDWR | O_TRUNC, 0600);
    if (fd < 0 || write(fd, "kept", 4) != 4 ||
        dup3(fd, 40, 0) != 40 || dup3(fd, 700, O_CLOEXEC) != 700 ||
        dup3(fd, 701, 0) != 701) _exit(7);
    close(fd);
    char *args[] = { (char *)self, "--exec-fd-child", NULL };
    char *env[] = { NULL };
    execve("/missing-exec-fds", args, env);
    if (errno != ENOENT || fcntl(700, F_GETFD) != FD_CLOEXEC ||
        fcntl(40, F_GETFD) != 0 || fcntl(701, F_GETFD) != 0) _exit(8);
    execve("/tmp/exec-fds", args, env);
    if (errno != ENOEXEC || fcntl(700, F_GETFD) != FD_CLOEXEC ||
        fcntl(40, F_GETFD) != 0 || fcntl(701, F_GETFD) != 0) _exit(9);
    execve(self, args, env);
    _exit(10);
}

static int check_exec_fd_child(void) {
    errno = 0;
    if (fcntl(700, F_GETFD) != -1 || errno != EBADF) {
        fprintf(stderr, "exec CLOEXEC fd 700 still open\n"); return 11;
    }
    char buf[4];
    if (fcntl(40, F_GETFD) != 0 || fcntl(701, F_GETFD) != 0 ||
        pread(40, buf, sizeof buf, 0) != sizeof buf || memcmp(buf, "kept", 4) ||
        pread(701, buf, sizeof buf, 0) != sizeof buf || memcmp(buf, "kept", 4))
        return 12;
    return 0;
}

static int check_exec_fd_flags(const char *self) {
    void *stack = mmap(NULL, 65536, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stack == MAP_FAILED) return 7;
    struct exec_fd_args config = { self, getpid() };
    pid_t pid = clone(exec_fd_child, (char *)stack + 65536,
                      CLONE_VM | CLONE_VFORK | SIGCHLD, &config);
    if (pid < 0) { perror("clone exec flags"); return 7; }
    int status;
    pid_t waited;
    do { waited = waitpid(pid, &status, 0); } while (waited < 0 && errno == EINTR);
    if (waited != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "exec fd flags child status=%d\n", status); return 7;
    }
    if (fcntl(700, F_GETFD) != FD_CLOEXEC || fcntl(40, F_GETFD) != 0 ||
        fcntl(701, F_GETFD) != 0) {
        fprintf(stderr, "exec changed shared parent descriptor table\n"); return 14;
    }
    close(700);
    close(701);
    close(40);
    munmap(stack, 65536);
    unlink("/tmp/exec-fds");
    puts("WB_EXEC_FD_FLAGS_OK");
    fflush(stdout);
    return 0;
}

static int check_unfaulted_reserve_uaccess(void) {
    char path[] = "/tmp/uaccXXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) { perror("mkstemp uacc"); return 15; }
    if (write(fd, "12345678", 8) != 8) { perror("write uacc"); return 15; }
    char *mp = mmap(NULL, 65536, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if (mp == MAP_FAILED) { perror("mmap noreserve"); return 15; }
    ssize_t n = pread(fd, mp, 8, 0);
    if (n != 8 || memcmp(mp, "12345678", 8) != 0) {
        fprintf(stderr, "pread into untouched reserved memory failed: %zd\n", n);
        return 15;
    }
    munmap(mp, 65536);
    close(fd);
    unlink(path);
    puts("WB_RESERVE_UACCESS_OK");
    fflush(stdout);
    return 0;
}

static void *check_stack_bounds(void *label) {
    pthread_attr_t attr;
    char local;
    void *base = NULL;
    size_t size = 0;
    int rc = pthread_getattr_np(pthread_self(), &attr);
    if (!rc) {
        rc = pthread_attr_getstack(&attr, &base, &size);
        pthread_attr_destroy(&attr);
    }
    int contains = !rc && (uintptr_t)&local >= (uintptr_t)base &&
                   (uintptr_t)&local - (uintptr_t)base < size;
    printf("WB_STACK %s rc=%d base=%p size=%zu local=%p contains=%d\n",
           (char *)label, rc, base, size, &local, contains);
    fflush(stdout);
    return (void *)(uintptr_t)!contains;
}

static int check_pthread_stack_bounds(void) {
    if (mkdir("/proc", 0555) && errno != EEXIST) return 16;
    if (mount("proc", "/proc", "proc", 0, NULL) && errno != EBUSY) return 16;
    struct rlimit rl;
    if (getrlimit(RLIMIT_NOFILE, &rl) || rl.rlim_cur == 0 || rl.rlim_cur == (rlim_t)-1) return 16;
    if (getrlimit(RLIMIT_STACK, &rl) || rl.rlim_cur != 262144) return 16;
    void *main_result = check_stack_bounds("main");
    pthread_t thread;
    void *result = NULL;
    int rc = pthread_create(&thread, NULL, check_stack_bounds, "worker");
    if (rc || pthread_join(thread, &result)) return 16;
    if (main_result || result) return 16;
    puts("WB_STACK_BOUNDS_OK");
    fflush(stdout);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "--exec-fd-child")) return check_exec_fd_child();
    if (argc == 2 && !strcmp(argv[1], "--vfork-exec-child")) return 37;
    if (argc == 2 && !strcmp(argv[1], "--vfork-exec-only"))
        return check_vfork_exec_shared(argv[0]);
    int rc;
    if ((rc = check_munmap_writeback())) return rc;
    if ((rc = check_msync_writeback())) return rc;
    if ((rc = check_private_unaffected())) return rc;
    if ((rc = check_anon_shared())) return rc;
    if ((rc = check_vfork_exec_shared(argv[0]))) return rc;
    if ((rc = check_fork_fd_flags())) return rc;
    if ((rc = check_exec_fd_flags(argv[0]))) return rc;
    if ((rc = check_unfaulted_reserve_uaccess())) return rc;
    if ((rc = check_pthread_stack_bounds())) return rc;
    return 0;
}
