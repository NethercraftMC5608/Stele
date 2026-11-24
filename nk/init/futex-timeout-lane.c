#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef FUTEX_TIMEOUT_LANE_MODE
#define FUTEX_TIMEOUT_LANE_MODE 0
#endif

static _Atomic uint32_t word;
static _Atomic int ready;
static int failures;

static int64_t now_ns(clockid_t clock)
{
    struct timespec t;
    if (clock_gettime(clock, &t)) {
        perror("clock_gettime");
        exit(2);
    }
    return (int64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

static struct timespec timespec_ns(int64_t ns)
{
    return (struct timespec){ ns / 1000000000, ns % 1000000000 };
}

static long futex_call(int op, unsigned val, const struct timespec *t, unsigned bits)
{
    errno = 0;
    return syscall(SYS_futex, &word, op | FUTEX_PRIVATE_FLAG, val, t, NULL, bits);
}

static void check(const char *name, int ok, long rc, int error, int64_t elapsed)
{
    printf("FUTEX_TIMEOUT_LANE_%s name=%s rc=%ld errno=%d elapsed_ns=%lld\n",
           ok ? "OK" : "FAIL", name, rc, error, (long long)elapsed);
    if (!ok)
        failures++;
}

static void error_case(const char *name, int op, unsigned val,
                       const struct timespec *t, unsigned bits, int expected)
{
    int64_t start = now_ns(CLOCK_MONOTONIC);
    long rc = futex_call(op, val, t, bits);
    int error = errno;
    check(name, rc == -1 && error == expected, rc, error,
          now_ns(CLOCK_MONOTONIC) - start);
}

static void expiry(int op, clockid_t clock, const char *name, int64_t delay)
{
    int64_t start = now_ns(CLOCK_MONOTONIC);
    struct timespec t = timespec_ns(delay +
        ((op & FUTEX_CMD_MASK) == FUTEX_WAIT ? 0 : now_ns(clock)));
    printf("FUTEX_TIMEOUT_LANE_ENTER name=%s delay_ns=%lld\n", name, (long long)delay);
    long rc = futex_call(op, 0, &t, FUTEX_BITSET_MATCH_ANY);
    int error = errno;
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    check(name, rc == -1 && error == ETIMEDOUT && elapsed >= delay &&
          elapsed < delay + 1000000000, rc, error, elapsed);
    rc = futex_call(FUTEX_WAKE, 1, NULL, 0);
    check("expiry-removal", rc == 0, rc, errno, 0);
}

static void *waker(void *unused)
{
    (void)unused;
    while (!atomic_load_explicit(&ready, memory_order_acquire))
        sched_yield();
    struct timespec delay = timespec_ns(40000000);
    if (nanosleep(&delay, NULL))
        return (void *)(intptr_t)-2;
    long rc = futex_call(FUTEX_WAKE_BITSET, 1, NULL, 2);
    return (void *)(intptr_t)rc;
}

static void wake_case(int op, clockid_t clock, const char *name)
{
    pthread_t thread;
    atomic_store(&ready, 0);
    int rc = pthread_create(&thread, NULL, waker, NULL);
    if (rc) {
        check("pthread-create", 0, rc, 0, 0);
        return;
    }
    struct timespec t = timespec_ns(200000000 +
        ((op & FUTEX_CMD_MASK) == FUTEX_WAIT ? 0 : now_ns(clock)));
    int64_t start = now_ns(CLOCK_MONOTONIC);
    atomic_store_explicit(&ready, 1, memory_order_release);
    long result = futex_call(op, 0, &t, 2);
    int error = errno;
    int64_t elapsed = now_ns(CLOCK_MONOTONIC) - start;
    void *wakes = NULL;
    rc = pthread_join(thread, &wakes);
    check(name, result == 0 && rc == 0 && (intptr_t)wakes == 1 &&
          elapsed < 200000000, result, error, elapsed);
    struct timespec delay = timespec_ns(250000000);
    rc = nanosleep(&delay, NULL);
    check("post-wake-timer-cancel", rc == 0, rc, errno, 0);
    result = futex_call(FUTEX_WAKE, 1, NULL, 0);
    check("wake-removal", result == 0, result, errno, 0);
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("FUTEX_TIMEOUT_LANE_START mode=%d syscall=%d\n",
           FUTEX_TIMEOUT_LANE_MODE, SYS_futex);
    if (FUTEX_TIMEOUT_LANE_MODE) {
        int op = FUTEX_TIMEOUT_LANE_MODE == 1 ? FUTEX_WAIT : FUTEX_WAIT_BITSET;
        clockid_t clock = FUTEX_TIMEOUT_LANE_MODE == 3 ? CLOCK_REALTIME : CLOCK_MONOTONIC;
        if (clock == CLOCK_REALTIME)
            op |= FUTEX_CLOCK_REALTIME;
        expiry(op, clock, "baseline-expiry", 50000000);
    } else {
        struct timespec valid = timespec_ns(50000000);
        struct timespec invalid_ns = { 0, 1000000000 };
        struct timespec invalid_sec = { -1, 0 };
        struct timespec negative_ns = { 0, -1 };
        struct timespec zero = { 0, 0 };
        error_case("wait-eagain", FUTEX_WAIT, 1, &valid, 0, EAGAIN);
        error_case("bitset-eagain", FUTEX_WAIT_BITSET, 1, &zero, 1, EAGAIN);
        error_case("invalid-nsec", FUTEX_WAIT, 0, &invalid_ns, 0, EINVAL);
        error_case("invalid-sec", FUTEX_WAIT_BITSET, 0, &invalid_sec, 1, EINVAL);
        error_case("negative-nsec", FUTEX_WAIT, 0, &negative_ns, 0, EINVAL);
        error_case("timeout-efault", FUTEX_WAIT, 0, (void *)(uintptr_t)1, 0, EFAULT);
        error_case("wait-zero-bitset", FUTEX_WAIT_BITSET, 0, &valid, 0, EINVAL);
        error_case("wake-zero-bitset", FUTEX_WAKE_BITSET, 1, NULL, 0, EINVAL);
        error_case("wait-realtime-unsupported", FUTEX_WAIT | FUTEX_CLOCK_REALTIME,
                   0, &valid, 0, ENOSYS);
        error_case("zero-relative", FUTEX_WAIT, 0, &zero, 0, ETIMEDOUT);
        error_case("past-absolute", FUTEX_WAIT_BITSET, 0, &zero, 1, ETIMEDOUT);
        for (int i = 0; i < 20; i++) {
            printf("FUTEX_TIMEOUT_LANE_REPEAT iteration=%d\n", i);
            expiry(FUTEX_WAIT, CLOCK_MONOTONIC, "relative-monotonic", 25000000);
            expiry(FUTEX_WAIT_BITSET, CLOCK_MONOTONIC, "absolute-monotonic", 35000000);
            expiry(FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME, CLOCK_REALTIME,
                   "absolute-realtime", 45000000);
            wake_case(FUTEX_WAIT, CLOCK_MONOTONIC, "wake-relative");
            wake_case(FUTEX_WAIT_BITSET, CLOCK_MONOTONIC, "wake-absolute");
            wake_case(FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME, CLOCK_REALTIME,
                      "wake-realtime");
        }
    }
    printf("FUTEX_TIMEOUT_LANE_DONE failures=%d\n", failures);
    return failures ? 1 : 0;
}
