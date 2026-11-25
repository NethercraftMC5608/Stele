/* SPDX-License-Identifier: GPL-2.0-or-later
 * Freestanding x86-64 probe for SA_RESTART: no libc, raw syscalls.
 *
 * A child writes one byte into a pipe 400 ms after the parent blocks reading
 * it; a 100 ms ITIMER_REAL interrupts the read with SIGALRM first.
 *   phase 1: the handler has SA_RESTART -> read returns the byte, the
 *            handler having run once.
 *   phase 2: no SA_RESTART -> read returns -EINTR; a second read gets it.
 */
typedef unsigned long u64;

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

static void number(long v)
{
    char text[24];
    int i = 23;
    text[i--] = 0;
    int negative = v < 0;
    unsigned long u = negative ? -(unsigned long)v : (unsigned long)v;
    do { text[i--] = '0' + u % 10; u /= 10; } while (u);
    if (negative) text[i--] = '-';
    say(&text[i + 1]);
}

static void check(int ok, const char *what)
{
    if (ok) return;
    say("RESTART_FAIL "); say(what); say("\n");
    call(231, 1, 0, 0, 0, 0, 0);
    for (;;) {}
}

static volatile int alarms;
static void on_alarm(int sig) { (void)sig; alarms++; }

/* rt_sigreturn trampoline for SA_RESTORER. */
__asm__(".text\n.global restorer\nrestorer:\n  mov $15, %eax\n  syscall\n");
void restorer(void);

struct kernel_sigaction { u64 handler, flags, restorer, mask; };
struct timespec { long sec, nsec; };
struct itimerval { struct timespec_us { long sec, usec; } interval, value; };

#define SA_RESTORER 0x04000000UL
#define SA_RESTART  0x10000000UL
#define SIGALRM 14
#define EINTR 4

static void handle(u64 flags)
{
    struct kernel_sigaction act = {(u64)on_alarm, flags | SA_RESTORER, (u64)restorer, 0};
    check(call(13, SIGALRM, (long)&act, 0, 8, 0, 0) == 0, "rt_sigaction");
}

/* Fork a child that writes one byte into fd after 400 ms; arm a 100 ms alarm. */
static void writer_and_alarm(int fd[2])
{
    long pid = call(57, 0, 0, 0, 0, 0, 0);
    check(pid >= 0, "fork");
    if (pid == 0) {
        struct timespec t = {0, 400 * 1000 * 1000};
        call(35, (long)&t, 0, 0, 0, 0, 0);
        char byte = 'x';
        call(1, fd[1], (long)&byte, 1, 0, 0, 0);
        call(60, 0, 0, 0, 0, 0, 0);
    }
    struct itimerval it = {{0, 0}, {0, 100 * 1000}};
    check(call(38, 0, (long)&it, 0, 0, 0, 0) == 0, "setitimer");
}

static void reap(void)
{
    int status;
    call(61, -1, (long)&status, 0, 0, 0, 0);
}

void probe(void)
{
    say("RESTART_BEGIN\n");
    int fd[2];
    char byte = 0;

    handle(SA_RESTART);
    check(call(22, (long)fd, 0, 0, 0, 0, 0) == 0, "pipe");
    alarms = 0;
    writer_and_alarm(fd);
    long n = call(0, fd[0], (long)&byte, 1, 0, 0, 0);
    say("phase 1: read "); number(n); say(", alarms "); number(alarms); say("\n");
    check(n == 1 && byte == 'x' && alarms == 1, "SA_RESTART read");
    say("RESTART_SA_RESTART_OK\n");
    reap();

    handle(0);
    check(call(22, (long)fd, 0, 0, 0, 0, 0) == 0, "pipe 2");
    alarms = 0;
    byte = 0;
    writer_and_alarm(fd);
    n = call(0, fd[0], (long)&byte, 1, 0, 0, 0);
    say("phase 2: read "); number(n); say(", alarms "); number(alarms); say("\n");
    check(n == -EINTR && alarms == 1, "EINTR without SA_RESTART");
    n = call(0, fd[0], (long)&byte, 1, 0, 0, 0);
    check(n == 1 && byte == 'x', "read after EINTR");
    say("RESTART_EINTR_OK\n");
    reap();

    say("RESTART_PASS\n");
    call(231, 0, 0, 0, 0, 0, 0);
    for (;;) {}
}

__asm__(".text\n.balign 16\n.global _start\n_start:\n  xor %ebp, %ebp\n  and $-16, %rsp\n  call probe\n  ud2\n");
