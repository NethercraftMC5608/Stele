/* Wayland-shaped unix-socket stress: repeated memfd + SCM_RIGHTS passing.
 *
 * When the nk shell opens an app window, the UI process hands weston a fresh
 * shm pool (memfd) over the Wayland socket and commits a new toplevel; some
 * time after that both sides log "error in client communication" and the
 * socket is dead. This is that exchange in miniature, in a loop, so the
 * break (if it is in the socket/fd layer and not in WebKit) can be caught
 * with an errno attached.
 *
 * Phases:
 *   1. fd churn     -- open/close/dup in a loop; fd numbers must not creep.
 *   2. wl exchange  -- client sends 64-byte messages each carrying one 64KB
 *                      memfd (SCM_RIGHTS, MSG_CMSG_CLOEXEC), server mmaps it,
 *                      checks the pattern, unmaps, closes the fd, replies.
 *   3. fd batching  -- one message carrying 24 fds, server closes them all.
 *
 * Open-fd counts are printed from /proc/self/fd so leaks are visible.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef __NR_memfd_create
#define __NR_memfd_create 279
#endif

#define POOL (64 * 1024)

static int fail(const char *what)
{
    printf("SCMSTRESS_FAIL %s: %s\n", what, strerror(errno));
    return 1;
}

static int open_fd_count(void)
{
    /* No /proc in this rootfs: measure with the lowest free descriptor
     * instead -- a leak shows up as the number creeping upward. */
    int fd = open("/", O_RDONLY | O_DIRECTORY);
    int n;
    if (fd < 0)
        return -1;
    /* The lowest free fd equals the number of fds 0..fd-1 that are open. */
    n = fd;
    close(fd);
    return n;
}

static int phase1_churn(void)
{
    int base = open_fd_count();
    int fd, hi = 0, i;

    for (i = 0; i < 2000; i++) {
        fd = open("/", O_RDONLY | O_DIRECTORY);
        if (fd < 0)
            return fail("churn open");
        if (fd > hi)
            hi = fd;
        int dup = fcntl(fd, F_DUPFD, 0);
        if (dup < 0)
            return fail("churn dup");
        if (dup > hi)
            hi = dup;
        close(dup);
        close(fd);
    }
    printf("SCMSTRESS_CHURN_OK highfd=%d open_before=%d open_after=%d\n",
           hi, base, open_fd_count());
    return 0;
}

/* One message: 64 bytes of data + nfd file descriptors. */
static int send_fds(int sock, int *fds, int nfd)
{
    struct msghdr msg;
    struct iovec iov;
    unsigned char data[64];
    union {
        struct cmsghdr h;
        char space[CMSG_SPACE(28 * sizeof(int))];
    } ctl;
    struct cmsghdr *cm;

    memset(&msg, 0, sizeof msg);
    memset(data, 0x5a, sizeof data);
    iov.iov_base = data;
    iov.iov_len = sizeof data;
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = &ctl;
    msg.msg_controllen = sizeof ctl;
    cm = CMSG_FIRSTHDR(&msg);
    cm->cmsg_level = SOL_SOCKET;
    cm->cmsg_type = SCM_RIGHTS;
    cm->cmsg_len = CMSG_LEN(nfd * sizeof(int));
    memcpy(CMSG_DATA(cm), fds, nfd * sizeof(int));
    msg.msg_controllen = cm->cmsg_len;
    return sendmsg(sock, &msg, MSG_NOSIGNAL);
}

/* Receive one message; fds received are stored (up to 28), count returned
 * through *nfd_out. Returns the data length, or -1. */
static ssize_t recv_fds(int sock, int *fds_out, int *nfd_out, void *buf, size_t len)
{
    struct msghdr msg;
    struct iovec iov;
    union {
        struct cmsghdr h;
        char space[CMSG_SPACE(28 * sizeof(int))];
    } ctl;
    struct cmsghdr *cm;
    ssize_t n;

    memset(&msg, 0, sizeof msg);
    iov.iov_base = buf;
    iov.iov_len = len;
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = &ctl;
    msg.msg_controllen = sizeof ctl;
    n = recvmsg(sock, &msg, MSG_CMSG_CLOEXEC);
    if (n < 0)
        return n;
    *nfd_out = 0;
    for (cm = CMSG_FIRSTHDR(&msg); cm; cm = CMSG_NXTHDR(&msg, cm)) {
        if (cm->cmsg_level == SOL_SOCKET && cm->cmsg_type == SCM_RIGHTS) {
            int count = (cm->cmsg_len - CMSG_LEN(0)) / sizeof(int);
            memcpy(fds_out + *nfd_out, CMSG_DATA(cm), count * sizeof(int));
            *nfd_out += count;
        }
    }
    return n;
}

static int phase2_exchange(void)
{
    int sv[2];
    pid_t pid;
    int i;

    setvbuf(stdout, NULL, _IONBF, 0);

    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv) < 0)
        return fail("socketpair");

    pid = fork();
    if (pid < 0)
        return fail("fork");

    if (pid == 0) {
        /* Client: the UI-process shape. One fresh memfd per message, sent
         * with its fd, closed after; a reply is awaited so the two sides
         * stay in lockstep. */
        unsigned char reply[8];
        int fds[1];
        close(sv[0]);
        for (i = 0; i < 300; i++) {
            int fd = syscall(__NR_memfd_create, "wl-pool", 0);
            if (fd < 0) {
                printf("SCMSTRESS_FAIL client memfd at %d: %s\n", i, strerror(errno));
                _exit(2);
            }
            if (ftruncate(fd, POOL) < 0) {
                printf("SCMSTRESS_FAIL client ftruncate: %s\n", strerror(errno));
                _exit(2);
            }
            fds[0] = fd;
            if (send_fds(sv[1], fds, 1) < 0) {
                printf("SCMSTRESS_FAIL client sendmsg at %d: %s\n", i, strerror(errno));
                _exit(3);
            }
            close(fd);
            if (recv(sv[1], reply, sizeof reply, 0) < 0) {
                printf("SCMSTRESS_FAIL client recv at %d: %s\n", i, strerror(errno));
                _exit(4);
            }
            if (i % 50 == 0)
                printf("SCMSTRESS_CLIENT %d openfds=%d\n", i, open_fd_count());
        }
        printf("SCMSTRESS_CLIENT_DONE openfds=%d\n", open_fd_count());
        _exit(0);
    }

    /* Server: the weston shape. mmap each received pool, verify, unmap,
     * close, reply. */
    close(sv[1]);
    {
        unsigned char buf[64];
        int fds[28];
        int nfd = 0;
        for (i = 0; i < 300; i++) {
            ssize_t n = recv_fds(sv[0], fds, &nfd, buf, sizeof buf);
            if (n < 0) {
                printf("SCMSTRESS_FAIL server recvmsg at %d: %s\n", i, strerror(errno));
                break;
            }
            if (nfd != 1) {
                printf("SCMSTRESS_FAIL server got %d fds at %d\n", nfd, i);
                break;
            }
            void *p = mmap(NULL, POOL, PROT_READ, MAP_SHARED, fds[0], 0);
            if (p == MAP_FAILED) {
                printf("SCMSTRESS_FAIL server mmap at %d: %s\n", i, strerror(errno));
                break;
            }
            if (memcmp(p, buf, 16) != 0)
                printf("SCMSTRESS_NOTE server pattern mismatch at %d (unread pool ok)\n", i);
            munmap(p, POOL);
            close(fds[0]);
            if (send(sv[0], "ok", 2, MSG_NOSIGNAL) < 0) {
                printf("SCMSTRESS_FAIL server reply at %d: %s\n", i, strerror(errno));
                break;
            }
            if (i % 50 == 0)
                printf("SCMSTRESS_SERVER %d openfds=%d\n", i, open_fd_count());
        }
        waitpid(pid, NULL, 0);
        printf("SCMSTRESS_SERVER_END at %d openfds=%d\n", i, open_fd_count());
    }
    return 0;
}

static int phase3_batch(void)
{
    int sv[2];
    pid_t pid;

    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv) < 0)
        return fail("socketpair batch");

    pid = fork();
    if (pid < 0)
        return fail("fork batch");

    if (pid == 0) {
        int fds[24];
        int i;
        close(sv[0]);
        for (i = 0; i < 24; i++)
            fds[i] = syscall(__NR_memfd_create, "batch", 0);
        for (i = 0; i < 24; i++)
            if (fds[i] < 0) {
                printf("SCMSTRESS_FAIL batch memfd %d: %s\n", i, strerror(errno));
                _exit(2);
            }
        if (send_fds(sv[1], fds, 24) < 0) {
            printf("SCMSTRESS_FAIL batch sendmsg: %s\n", strerror(errno));
            _exit(3);
        }
        for (i = 0; i < 24; i++)
            close(fds[i]);
        _exit(0);
    }

    close(sv[1]);
    {
        unsigned char buf[64];
        int fds[28];
        int nfd = 0;
        ssize_t n = recv_fds(sv[0], fds, &nfd, buf, sizeof buf);
        if (n < 0)
            return fail("batch recvmsg");
        printf("SCMSTRESS_BATCH_OK got=%d data=%zd\n", nfd, n);
        for (int i = 0; i < nfd; i++)
            close(fds[i]);
        waitpid(pid, NULL, 0);
    }
    return 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("SCMSTRESS_START\n");
    if (phase1_churn())
        return 1;
    if (phase2_exchange())
        return 2;
    if (phase3_batch())
        return 3;
    printf("SCMSTRESS_DONE\n");
    return 0;
}
