/* Unix-socket fd passing and MAP_SHARED coherence, the WebKit shape.
 *
 * WebKit's renderer paints into a memfd it maps MAP_SHARED, then ships the
 * fd to the UI process (and weston) over a socketpair with SCM_RIGHTS; the
 * receiver maps the same file and reads the pixels. Two things have to be
 * true for anything to appear on screen:
 *
 *   1. the fd survives the socket (SCM_RIGHTS),
 *   2. the receiver's mapping shows the sender's writes (shared coherence).
 *
 * This is that, minimally: fork, child paints one byte per page through its
 * mapping and sends the fd, parent maps and reads them back. A page that
 * reads back zero or stale is a frame the UI process would never see.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>

#define PAGE 4096
#define MAPSIZE (2048 * PAGE)

#ifndef __NR_memfd_create
#define __NR_memfd_create 279
#endif

static int fail(const char *what)
{
    printf("SCMTEST_FAIL %s: %s\n", what, strerror(errno));
    return 1;
}

int main(void)
{
    int sv[2];

    setvbuf(stdout, NULL, _IONBF, 0);

    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0)
        return fail("socketpair");

    pid_t pid = fork();
    if (pid < 0)
        return fail("fork");

    if (pid == 0) {
        unsigned char *p;
        struct msghdr msg;
        struct iovec iov;
        unsigned char byte = 0x42;
        union { struct cmsghdr h; char space[CMSG_SPACE(sizeof(int))]; } ctl;
        struct cmsghdr *cm;

        close(sv[0]);
        int fd = syscall(__NR_memfd_create, "paint", 0);
        if (fd < 0) { dprintf(2, "SCMTEST_FAIL memfd_create: %s\n", strerror(errno)); _exit(2); }
        if (ftruncate(fd, MAPSIZE) < 0) { dprintf(2, "SCMTEST_FAIL ftruncate: %s\n", strerror(errno)); _exit(2); }

        p = mmap(NULL, MAPSIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (p == MAP_FAILED) { dprintf(2, "SCMTEST_FAIL child mmap: %s\n", strerror(errno)); _exit(2); }
        for (size_t i = 0; i < MAPSIZE; i += PAGE)
            p[i] = (unsigned char)(i / PAGE);
        /* Deliberately no msync: WebKit paints and posts, it never syncs. */

        memset(&msg, 0, sizeof msg);
        iov.iov_base = &byte;
        iov.iov_len = 1;
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = &ctl;
        msg.msg_controllen = sizeof ctl;
        cm = CMSG_FIRSTHDR(&msg);
        cm->cmsg_level = SOL_SOCKET;
        cm->cmsg_type = SCM_RIGHTS;
        cm->cmsg_len = CMSG_LEN(sizeof(int));
        *((int *)CMSG_DATA(cm)) = fd;
        if (sendmsg(sv[1], &msg, 0) < 0) { dprintf(2, "SCMTEST_FAIL sendmsg: %s\n", strerror(errno)); _exit(2); }

        usleep(300000);   /* hold the mapping while the parent reads */
        _exit(0);
    }

    close(sv[1]);
    struct msghdr msg;
    struct iovec iov;
    unsigned char byte;
    union { struct cmsghdr h; char space[CMSG_SPACE(sizeof(int))]; } ctl;
    struct cmsghdr *cm;
    int fd = -1;

    memset(&msg, 0, sizeof msg);
    iov.iov_base = &byte;
    iov.iov_len = 1;
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = &ctl;
    msg.msg_controllen = sizeof ctl;

    ssize_t n = recvmsg(sv[0], &msg, 0);
    if (n < 0)
        return fail("recvmsg");
    for (cm = CMSG_FIRSTHDR(&msg); cm; cm = CMSG_NXTHDR(&msg, cm)) {
        if (cm->cmsg_level == SOL_SOCKET && cm->cmsg_type == SCM_RIGHTS)
            fd = *((int *)CMSG_DATA(cm));
    }
    if (fd < 0) {
        printf("SCMTEST_FAIL no fd in control message\n");
        return 1;
    }
    printf("SCMTEST_FD_OK fd=%d byte=%#x\n", fd, byte);

    unsigned char *p = mmap(NULL, MAPSIZE, PROT_READ, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED)
        return fail("parent mmap");

    int bad = 0;
    for (size_t i = 0; i < MAPSIZE; i += PAGE) {
        unsigned char want = (unsigned char)(i / PAGE);
        if (p[i] != want) {
            if (bad < 4)
                printf("SCMTEST_PAGE page %zu: want %#x got %#x\n", i / PAGE, want, p[i]);
            bad++;
        }
    }
    printf("SCMTEST_%s badpages=%d/64\n", bad ? "INCOHERENT" : "COHERENT", bad);
    if (!bad)
        printf("SCMTEST_DONE\n");
    return bad ? 1 : 0;
}
