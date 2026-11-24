/* SPDX-License-Identifier: GPL-2.0-or-later
 * Test-initrd entropy provisioning: credit 256 fresh host CSPRNG bits in LKL,
 * then require nonblocking getrandom to succeed. No fixed/time-derived seed.
 */
static long call(long nr, long a, long b, long c)
{
    long result;
    __asm__ volatile("syscall" : "=a"(result)
        : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory", "cc");
    return result;
}
__attribute__((noreturn)) static void finish(int ok)
{
    const char *message = ok ? "HOST_ENTROPY_SEEDED\n" : "HOST_ENTROPY_FAIL\n";
    long size = 0;
    while (message[size]) ++size;
    call(1, 1, (long)message, size);
    call(231, !ok, 0, 0);
    for (;;) {}
}
__attribute__((used,noreturn)) void seed(void)
{
    struct { int entropy_count, buffer_size; unsigned char bytes[32]; } pool;
    pool.entropy_count = 256;
    pool.buffer_size = 32;
    long fd = call(2, (long)"/entropy.seed", 0, 0);
    if (fd < 0) finish(0);
    long done = 0;
    while (done < 32) {
        long got = call(0, fd, (long)(pool.bytes + done), 32 - done);
        if (got == -4) continue;
        if (got <= 0) finish(0);
        done += got;
    }
    call(3, fd, 0, 0);
    fd = call(2, (long)"/dev/random", 2, 0);
    if (fd < 0) finish(0);
    /* Linux UAPI RNDADDENTROPY = _IOW('R', 3, int[2]). */
    long rc = call(16, fd, 0x40085203, (long)&pool);
    call(3, fd, 0, 0);
    if (rc != 0) finish(0);
    /* Flags=GRND_NONBLOCK, so success proves the CRNG is ready. */
    unsigned char check[32];
    finish(call(318, (long)check, sizeof(check), 1) == sizeof(check));
}
__asm__(".text\n.global _start\n_start:\n andq $-16,%rsp\n call seed\n ud2\n");
