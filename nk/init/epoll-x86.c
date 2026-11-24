/* SPDX-License-Identifier: GPL-2.0-or-later
 * Freestanding Linux x86-64 ABI probe: no libc, sysroot or guest disk needed.
 * Use raw calls so libc cannot silently substitute one epoll variant for another.
 */
typedef unsigned long u64;
struct event { unsigned events; u64 data; } __attribute__((packed));
_Static_assert(sizeof(struct event) == 12, "x86 epoll_event layout");

static long call(long nr, long a, long b, long c, long d, long e, long f)
{
    register long r10 __asm__("r10") = d;
    register long r8 __asm__("r8") = e;
    register long r9 __asm__("r9") = f;
    long rc;
    __asm__ volatile("syscall" : "=a"(rc) : "a"(nr), "D"(a), "S"(b),
                     "d"(c), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory");
    return rc;
}

static void say(const char *s)
{
    long n = 0;
    while (s[n]) ++n;
    call(1, 1, (long)s, n, 0, 0, 0);
}

static void check(int ok, const char *what)
{
    if (ok) return;
    say("EPOLL_FAIL "); say(what); say("\n");
    call(231, 1, 0, 0, 0, 0, 0);
    for (;;) {}
}

static void hex(u64 value)
{
    char text[] = "0x0000000000000000\n";
    for (int i = 0; i < 16; ++i) text[17-i] = "0123456789abcdef"[(value >> (i*4)) & 15];
    call(1, 1, (long)text, 19, 0, 0, 0);
}

static void wait_events(long nr, int ep, int fd0, int fd1)
{
    /* Guards catch copying 16-byte LKL records straight to 12-byte slots. */
    struct { u64 before; struct event ev[2]; u64 after; } out;
    out.before = out.after = 0xfeedfacecafebeefUL;
    out.ev[0].data = out.ev[1].data = 0;
    u64 mask = 0;
    long timeout[2] = {0, 0};
    long n = call(nr, ep, (long)out.ev, 2, nr == 441 ? (long)timeout : 0, (long)&mask, 8);
    check(n == 2, "wait count");
    check(out.before == 0xfeedfacecafebeefUL && out.after == out.before, "wait bounds");
    unsigned seen = 0;
    for (int i = 0; i < 2; ++i) {
        check(out.ev[i].events == 1, "EPOLLIN");
        if (out.ev[i].data == (0x1234567800000000UL | (unsigned)fd0)) seen |= 1;
        else if (out.ev[i].data == (0xabcdef0100000000UL | (unsigned)fd1)) seen |= 2;
        else { say("unexpected event data: "); hex(out.ev[i].data); check(0, "64-bit data/fd"); }
    }
    check(seen == 3, "distinct fd/data");
}

void probe(void)
{
    say("EPOLL_BEGIN\n");
    int ep = call(291, 0, 0, 0, 0, 0, 0);
    check(ep >= 0, "epoll_create1");
    int fd[2];
    for (int i = 0; i < 2; ++i) {
        fd[i] = call(290, 0, 0x800, 0, 0, 0, 0); /* eventfd2, NONBLOCK */
        check(fd[i] >= 0, "eventfd2");
        struct event e = {1, (i ? 0xabcdef0100000000UL : 0x1234567800000000UL) | (unsigned)fd[i]};
        check(call(233, ep, 1, fd[i], (long)&e, 0, 0) == 0, "ctl ADD");
    }
    struct event out[2];
    check(call(232, ep, (long)out, 2, 0, 0, 0) == 0, "empty timeout");
    for (int i = 0; i < 2; ++i) {
        u64 one = 1;
        check(call(1, fd[i], (long)&one, 8, 0, 0, 0) == 8, "make ready");
    }
    wait_events(232, ep, fd[0], fd[1]);
    say("EPOLL_WAIT_OK\n");
    wait_events(281, ep, fd[0], fd[1]);
    say("EPOLL_PWAIT_OK\n");
    wait_events(441, ep, fd[0], fd[1]);
    say("EPOLL_PWAIT2_OK\n");
    check(call(232, ep, 1, 2, 0, 0, 0) == -14, "wait bad pointer");
    check(call(233, ep, 3, fd[0], 1, 0, 0) == -14, "ctl bad pointer");
    u64 mask = 0;
    check(call(281, ep, (long)out, 2, 0, (long)&mask, 4) == -22, "mask size");
    struct event e = {1, 0xfedcba9876543210UL};
    check(call(233, ep, 3, fd[0], (long)&e, 0, 0) == 0, "ctl MOD");
    check(call(233, ep, 2, fd[1], 0, 0, 0) == 0, "ctl DEL null");
    check(call(232, ep, (long)out, 2, 0, 0, 0) == 1, "modified count");
    check(out[0].data == e.data && out[0].events == 1, "modified data");
    u64 value;
    check(call(0, fd[0], (long)&value, 8, 0, 0, 0) == 8 && value == 1, "drain");
    check(call(232, ep, (long)out, 2, 0, 0, 0) == 0, "drained timeout");
    check(call(232, ep, (long)out, 0, 0, 0, 0) == -22, "zero maxevents");
    for (int i = 0; i < 2; ++i) check(call(3, fd[i], 0, 0, 0, 0, 0) == 0, "close eventfd");
    check(call(3, ep, 0, 0, 0, 0, 0) == 0, "close epoll");
    say("EPOLL_PASS\n");
    call(231, 0, 0, 0, 0, 0, 0);
    for (;;) {}
}

/* A Linux process starts without a return address. Align before calling C. */
__asm__(".global _start\n_start:\n xor %ebp,%ebp\n and $-16,%rsp\n call probe\n ud2\n");
