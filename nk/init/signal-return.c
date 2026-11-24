/* SPDX-License-Identifier: GPL-2.0-or-later
 * Freestanding Linux arm64/x86-64 signal return-order regression, no libc needed.
 */
typedef unsigned long u64;
static volatile int hits, last_signal;
static volatile u64 handler_mask;
#if defined(__aarch64__)
#define PREFIX "ARM_SIGNAL_"
#define WRITE 64
#define EXIT_GROUP 94
#define GETPID 172
#define KILL 129
#define SIGACTION 134
#define SIGPROCMASK 135
static long call(long nr,long a,long b,long c,long d,long e,long f)
{
    register long x0 __asm__("x0")=a,x1 __asm__("x1")=b,x2 __asm__("x2")=c;
    register long x3 __asm__("x3")=d,x4 __asm__("x4")=e,x5 __asm__("x5")=f;
    register long x8 __asm__("x8")=nr;
    __asm__ volatile("svc #0":"+r"(x0):"r"(x1),"r"(x2),"r"(x3),"r"(x4),"r"(x5),"r"(x8):"memory","cc");
    return x0;
}
#else
#define PREFIX "X86_SIGNAL_"
#define WRITE 1
#define EXIT_GROUP 231
#define GETPID 39
#define KILL 62
#define SIGACTION 13
#define SIGPROCMASK 14
static long call(long nr,long a,long b,long c,long d,long e,long f)
{
    register long r10 __asm__("r10")=d,r8 __asm__("r8")=e,r9 __asm__("r9")=f;
    long result;
    __asm__ volatile("syscall":"=a"(result):"a"(nr),"D"(a),"S"(b),"d"(c),
        "r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");
    return result;
}
#endif
static void say(const char *s)
{
    long n=0;while(s[n])++n;call(WRITE,1,(long)s,n,0,0,0);
}
static void check(int ok,const char *message)
{
    if(ok)return;
    say(PREFIX "FAIL ");say(message);say("\n");
    call(EXIT_GROUP,1,0,0,0,0,0);for(;;){}
}
static void handler(int sig)
{
    u64 mask=0;
    call(SIGPROCMASK,0,0,(long)&mask,8,0,0);
    handler_mask=mask;
    last_signal=sig;++hits;
}
extern void restore(void);
void probe(void)
{
    say(PREFIX "BEGIN\n");
    u64 preserved_mask=1UL<<11; /* SIGUSR2 stays blocked across every handler. */
    check(call(SIGPROCMASK,0,(long)&preserved_mask,0,8,0,0)==0,"initial mask");
    struct {u64 handler,flags,restorer,mask;} action={(u64)handler,0x04000000,(u64)restore,1UL<<14};
    check(call(SIGACTION,10,(long)&action,0,8,0,0)==0,"sigaction");
    long pid=call(GETPID,0,0,0,0,0,0);check(pid>0,"getpid");
    /* kill's first argument is pid, but its return is zero. The handler
     * runs before returning to this comparison. Saving x0 too early makes
     * rt_sigreturn restore pid as the syscall result. */
    check(call(KILL,pid,10,0,0,0,0)==0,"kill return was overwritten by saved argument");
    check(hits==1&&last_signal==10,"first handler");
    check(handler_mask==(preserved_mask|(1UL<<9)|(1UL<<14)),"temporary handler mask");
    u64 observed_mask=0;
    check(call(SIGPROCMASK,0,0,(long)&observed_mask,8,0,0)==0&&observed_mask==preserved_mask,"original mask restored");
    check(call(KILL,pid,10,0,0,0,0)==0&&hits==2,"second handler/sigreturn");
    u64 mask=1UL<<9;
    check(call(SIGPROCMASK,0,(long)&mask,0,8,0,0)==0,"block");
    check(call(KILL,pid,10,0,0,0,0)==0&&hits==2,"blocked signal stays pending");
    /* Unblocking also delivers during syscall return: SIG_UNBLOCK=1 must
     * not be restored as a successful rt_sigprocmask's return value. */
    check(call(SIGPROCMASK,1,(long)&mask,0,8,0,0)==0,"unblock return was overwritten by saved argument");
    check(hits==3&&last_signal==10,"pending handler");
    check(call(GETPID,0,0,0,0,0,0)==pid,"execution resumes after handlers");
    say(PREFIX "PASS\n");call(EXIT_GROUP,0,0,0,0,0,0);for(;;){}
}
#if defined(__aarch64__)
__asm__(".global _start\n_start:\n bl probe\n brk #0\n"
        ".global restore\nrestore:\n mov x8,#139\n svc #0\n brk #0\n");

#else
__asm__(".global _start\n_start:\n xor %ebp,%ebp\n and $-16,%rsp\n call probe\n ud2\n"
        ".global restore\nrestore:\n mov $15,%rax\n syscall\n ud2\n");
#endif
