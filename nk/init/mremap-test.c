// mremap micro-test, in the two shapes that reached the kernel through the
// desktop: weston growing a wl_shm pool (a shared memfd mapping, remapped
// with MREMAP_MAYMOVE) and glibc's realloc growing a large private chunk.
#define _GNU_SOURCE 1
#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/mman.h>
#include <stdio.h>
#include <unistd.h>

#define CHECK(cond, name) do { if (!(cond)) { \
    printf("MREMAP_FAIL %s errno=%d (%s)\n", name, errno, strerror(errno)); \
    _exit(1); } } while (0)

int main(void)
{
    printf("MREMAP_START\n");

    // -- shared memfd pool, the weston shape --
    int fd = memfd_create("mremaptest", 0);
    CHECK(fd >= 0, "memfd");
    CHECK(ftruncate(fd, 4096) == 0, "truncate 4096");
    unsigned char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(p != MAP_FAILED, "mmap shared");
    for (int i = 0; i < 4096; i++) p[i] = (unsigned char)(i ^ 0xA5);
    // The client's grow: bigger file, then the move.
    CHECK(ftruncate(fd, 8192) == 0, "truncate 8192");
    unsigned char *q = mremap(p, 4096, 8192, MREMAP_MAYMOVE);
    CHECK(q != MAP_FAILED, "mremap shared grow");
    int bad = 0;
    for (int i = 0; i < 4096; i++) if (q[i] != (unsigned char)(i ^ 0xA5)) bad++;
    CHECK(bad == 0, "shared content after move");
    q[5000] = 0x5A;                       // into the new page
    CHECK(q[5000] == 0x5A, "shared write past old end");
    unsigned char *w = mmap(0, 8192, PROT_READ, MAP_SHARED, fd, 0);
    CHECK(w != MAP_FAILED, "second holder maps the pool");
    CHECK(w[100] == q[100], "second holder sees head bytes");
    CHECK(w[5000] == 0x5A, "second holder sees the write");
    printf("MREMAP_SHARED_OK\n");
    munmap(q, 8192);
    munmap(w, 8192);
    close(fd);

    // -- private anonymous, the glibc realloc shape --
    unsigned char *a = mmap(0, 1 << 20, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(a != MAP_FAILED, "mmap private");
    for (int i = 0; i < (1 << 20); i += 4096) a[i] = (unsigned char)(i / 4096);
    a[(1 << 20) - 1] = 0xC3;              // last byte of the last page
    unsigned char *b = mremap(a, 1 << 20, 2 << 20, MREMAP_MAYMOVE);
    CHECK(b != MAP_FAILED, "mremap private grow");
    int bad2 = 0;
    for (int i = 0; i < (1 << 20); i += 4096) if (b[i] != (unsigned char)(i / 4096)) bad2++;
    CHECK(bad2 == 0, "private page heads after move");
    CHECK(b[(1 << 20) - 1] == 0xC3, "private last byte after move");
    b[(2 << 20) - 1] = 0x3C;              // in the demand-paged new half
    CHECK(b[(2 << 20) - 1] == 0x3C, "write into the grown half");
    printf("MREMAP_PRIVATE_OK\n");
    munmap(b, 2 << 20);

    // -- shrink, shared: the cursor pool also shrinks --
    int fd2 = memfd_create("mremaptest", 0);
    CHECK(fd2 >= 0, "memfd 2");
    CHECK(ftruncate(fd2, 8192) == 0, "truncate 8192 (2)");
    unsigned char *c = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd2, 0);
    CHECK(c != MAP_FAILED, "mmap shared (2)");
    c[7] = 0x77;
    unsigned char *d = mremap(c, 8192, 4096, MREMAP_MAYMOVE);
    CHECK(d != MAP_FAILED, "mremap shared shrink");
    CHECK(d[7] == 0x77, "shrink keeps head bytes");
    printf("MREMAP_SHRINK_OK\n");
    munmap(d, 4096);
    close(fd2);

    printf("MREMAP_DONE\n");
    return 0;
}
