// x86-64 user-space transitions, context switch and setjmp.
//
// The companion to user.s on aarch64. It provides the functions that must be
// in assembly because they manipulate the stack or registers across a boundary
// C and Rust cannot express:
//
//   * nk_setjmp / nk_longjmp: LKL's host unwinding operations
//   * cpu_switch: context switch between two kernel tasks. Five arguments:
//       rdi = &outgoing TASKS[cur].on_cpu   (cleared; the release store)
//       rsi = &outgoing TASKS[cur].sp       (the saved context)
//       rdx = incoming saved SP             (TASKS[next].sp)
//       rcx = &CPUS[cpu].current            (published as `next`)
//       r8  = incoming task id
//   * enter_user / resume_user: drop from ring 0 to ring 3
//
// AT&T syntax, matching boot-x86_64.s and the options(att_syntax) flag.

.code64
.section ".text", "ax"

// ---------------------------------------------------- nk_setjmp / longjmp --
//
// System V AMD64 callee-saved registers: rbx, rsp, rbp, r12, r13, r14, r15,
// and the return address. Total 8 words = 64 bytes, which fits inside LKL's
// jmp_buf.
//
// rdi = jmp_buf pointer
// rsi = return value (for longjmp)
.global nk_setjmp
nk_setjmp:
    mov     %rbx, 0(%rdi)
    mov     %rsp, 8(%rdi)
    mov     %rbp, 16(%rdi)
    mov     %r12, 24(%rdi)
    mov     %r13, 32(%rdi)
    mov     %r14, 40(%rdi)
    mov     %r15, 48(%rdi)
    mov     (%rsp), %rax                    # return address off stack
    mov     %rax, 56(%rdi)
    xor     %eax, %eax                      # returns 0 on direct call
    ret

.global nk_longjmp
nk_longjmp:
    mov     0(%rdi), %rbx
    mov     8(%rdi), %rsp
    mov     16(%rdi), %rbp
    mov     24(%rdi), %r12
    mov     32(%rdi), %r13
    mov     40(%rdi), %r14
    mov     48(%rdi), %r15
    mov     56(%rdi), %rdx                  # target return address
    mov     %rdx, (%rsp)
    mov     %esi, %eax                      # val
    test    %eax, %eax
    jnz     1f
    mov     $1, %eax                        # longjmp(buf, 0) must return 1
1:  ret

// ----------------------------------------------------------- cpu_switch --
//
// Context switch between two kernel tasks. Saves the outgoing task's
// callee-saved state onto its stack, records the new stack pointer, switches
// to the incoming task's stack, and restores.
//
// rdi = *mut usize (where to store outgoing sp)
// rsi = usize      (incoming sp to load)
.global cpu_switch
cpu_switch:
    push    %rbp
    push    %rbx
    push    %r12
    push    %r13
    push    %r14
    push    %r15

    mov     %rsp, (%rsi)
    mov     %rdx, %rsp
    # Ownership first, publication second -- see the aarch64 `cpu_switch`
    # for why the order matters. On x86-64 TSO the plain stores below are
    # already the release the protocol needs.
    mov     %r8, (%rcx)             # CPUS[cpu].current = next_id
    movq    $-1, (%rdi)             # owner = NO_OWNER: the outgoing context
                                    # is unowned (full-width aligned store;
                                    # x86 TSO keeps the ordering release).

    pop     %r15
    pop     %r14
    pop     %r13
    pop     %r12
    pop     %rbx
    pop     %rbp
    ret

// ----------------------------------------------------------- task_start --
//
// Where cpu_switch lands on the first switch to a newly spawned task.
// r12 holds the entry function pointer and r13 its argument, popped off
// the initial stack frame by cpu_switch.
.global task_start
task_start:
    call    switch_complete_entry   # consume the completed-switch record
    sti                             # unmask interrupts for the new task
    mov     %r13, %rdi              # System V AMD64 first parameter is rdi
    call    *%r12                   # entry(arg)
    call    task_exit               # if entry returns, end the task cleanly
1:  hlt
    jmp     1b

// ---------------------------------------------------- FPSIMD save/restore --
//
// fxsave / fxrstor: 512 bytes, 16-byte aligned. Saves x87, MMX, and XMM0-15,
// which is the x86-64 equivalent of the q0-q31 save/restore on aarch64 that
// fixed the Skia gradient renderer crash.
//
// rdi = *mut FpSave (must be 16-byte aligned)
.global nk_fp_save_asm
nk_fp_save_asm:
    fxsave  (%rdi)
    ret

.global nk_fp_restore_asm
nk_fp_restore_asm:
    fxrstor (%rdi)
    ret

// ------------------------------------------------ enter_user / fresh --
//
// Drop to ring 3 via iretq.
//
// rdi = entry point (rip)
// rsi = user stack (rsp)
// rdx = page table root (cr3)
//
// GDT layout from boot-x86_64.s:
//   0x08 = kernel code
//   0x10 = kernel data
//   0x18 = user data  -> selector 0x18 | 3 = 0x1b
//   0x20 = user code  -> selector 0x20 | 3 = 0x23
.global enter_user
enter_user:
    // Switch address space
    mov     %rdx, %cr3

    // The entry point and the user stack are the arguments, in %rdi and
    // %rsi -- and both are call-clobbered. The two calls below therefore
    // destroy them before the iretq frame is built: `nk_fp_addr` returns
    // into %rax and takes %rdi as its own first argument, so the process
    // was entered at whatever the FP save area happened to be. It faulted
    // in ring 3 at a kernel address, which reads as a broken transition
    // rather than as two registers quietly overwritten.
    //
    // Callee-saved registers are the fix, since these calls respect them.
    push    %rbx
    push    %r12
    mov     %rdi, %rbx                      # entry
    mov     %rsi, %r12                      # user stack

    // Restore FP/SIMD state
    call    nk_fp_addr
    mov     %rax, %rdi
    call    nk_fp_restore_asm

    mov     %rbx, %rdi                      # entry, back where it belongs
    mov     %r12, %rsi                      # user stack
    pop     %r12
    pop     %rbx

    // Set up the iretq stack frame: SS, RSP, RFLAGS, CS, RIP
    mov     $0x1b, %ax                      # user data selector (RPL 3)
    mov     %ax, %ds
    mov     %ax, %es
    mov     %ax, %fs
    mov     %ax, %gs

    pushq   $0x1b                           # SS
    pushq   %rsi                            # RSP
    pushq   $0x202                          # RFLAGS (IF enabled)
    pushq   $0x23                           # CS (user code, RPL 3)
    pushq   %rdi                            # RIP (entry point)

    // Clear every general-purpose register so no kernel state leaks to EL0
    xor     %rax, %rax
    xor     %rbx, %rbx
    xor     %rcx, %rcx
    xor     %rdx, %rdx
    xor     %rsi, %rsi
    xor     %rdi, %rdi
    xor     %rbp, %rbp
    xor     %r8,  %r8
    xor     %r9,  %r9
    xor     %r10, %r10
    xor     %r11, %r11
    xor     %r12, %r12
    xor     %r13, %r13
    xor     %r14, %r14
    xor     %r15, %r15

    iretq

// enter_user_fresh(entry: rdi, stack: rsi, cr3: rdx, kernel_sp: rcx)
// Used by execve: wind the kernel stack back to its top first.
.global enter_user_fresh
enter_user_fresh:
    mov     %rcx, %rsp
    jmp     enter_user

// -------------------------------------------------------- syscall entry --
//
// Where the CPU lands on `syscall` from ring 3:
//   %rcx = return RIP
//   %r11 = saved RFLAGS
//   %rax = syscall number
//   %rdi, %rsi, %rdx, %r10, %r8, %r9 = arguments 0..5
//
// The stack is still the user's stack. We switch to the kernel stack,
// build a Frame on it, and call rust_syscall.
.global syscall_entry
syscall_entry:
    // Stash user RSP in this CPU's scratch slot, load this CPU's kernel stack.
    //
    // Per-CPU because two CPUs in `syscall` at once share nothing else here:
    // each names its slot by APIC ID (EBX bits 24..31 from CPUID.1, masked
    // to 8 slots), so CPU 1's entry cannot clobber CPU 0's saved RSP. IF is
    // still masked by FMASK across the sequence, so no interrupt can land
    // between the stash and the reload on the same CPU either.
    // Stash first, index second. The pushes below move RSP, so the value
    // stored must be the entry RSP plus the 24 bytes pushed after it.
    //
    // CPUID clobbers rax/rbx/rcx/rdx. rcx (return RIP) and rdx (arg 2) are
    // live here and are preserved in the 32 bytes below RSP -- safe to use
    // because this is the kernel's own stack region, IF is masked by FMASK,
    // and nothing else runs on this stack until the switch below.
    //
    // The CPU index lives in rbx only until rbx is restored, so the kernel
    // stack pointer is read out *first* and parked in a fifth slot; the stack
    // switch then loads it from there. Indexing kernel_sp_scratch with rbx
    // after restoring it indexed with the *user's* rbx: harmless while every
    // test binary entered its first syscall with rbx = 0 (CPU 0's slot by
    // luck), and a zero stack pointer -- push to 0xff..fef0, #PF, #DF -- the
    // first time busybox did not.
    sub     $40, %rsp
    mov     %rax, 24(%rsp)
    mov     %rcx, 0(%rsp)
    mov     %rdx, 8(%rsp)
    mov     %rbx, 16(%rsp)
    mov     $1, %eax
    cpuid
    shr     $24, %ebx
    and     $7, %ebx
    lea     40(%rsp), %rax
    mov     %rax, user_rsp_scratch(,%rbx,8)
    mov     kernel_sp_scratch(,%rbx,8), %rax
    mov     %rax, 32(%rsp)
    // Restore the saved registers BEFORE switching stacks: after the mov
    // below, RSP names the kernel stack and U-40 is unreachable.
    mov     24(%rsp), %rax
    mov     16(%rsp), %rbx
    mov     8(%rsp), %rdx
    mov     0(%rsp), %rcx
    mov     32(%rsp), %rsp
    // RSP now names the kernel stack; rax is the syscall number again, rcx
    // the return RIP, rdx arg 2 -- exactly what entry provided.

    // Build the Frame on the kernel stack:
    //   Frame layout:
    //     x: [u64; 31]
    //     elr: u64   (offset 31*8 = 248) -> return rip
    //     spsr: u64  (offset 32*8 = 256) -> saved rflags
    //     sp: u64    (offset 33*8 = 264) -> user rsp
    // Total size = 34 * 8 = 272 bytes.
    //
    // 272 is a multiple of 16 and the kernel stack top is page-aligned, so
    // after the `call` below pushes its return address the callee sees the
    // 16-byte-minus-8 alignment the System V ABI requires.
    sub     $272, %rsp

    // Clear the frame before filling it. Only about half the slots are written
    // below, and the rest are read: fork copies the whole frame and
    // resume_user restores from it, so anything left unwritten is whatever the
    // kernel stack happened to hold -- kernel addresses handed to ring 3 in
    // registers, and not reproducible between runs.
    //
    // `rep stosq` counts upward only with DF clear. That is guaranteed here
    // because FMASK (set in x86_syscall_init below) includes DF, so the CPU
    // clears it on every `syscall`; ring 3 cannot set DF and make this run
    // backwards over the kernel stack.
    push    %rdi
    push    %rcx
    push    %rax
    lea     24(%rsp), %rdi
    xor     %eax, %eax
    mov     $34, %ecx
    rep stosq
    pop     %rax
    pop     %rcx
    pop     %rdi

    // Save arguments and registers into frame.x[]
    mov     %rdi, 0(%rsp)                   # x[0] = arg 0 / return value
    mov     %rsi, 8(%rsp)                   # x[1] = arg 1
    mov     %rdx, 16(%rsp)                  # x[2] = arg 2
    mov     %r10, 24(%rsp)                  # x[3] = arg 3 (Linux x86-64 passes arg 3 in r10)
    mov     %r8,  32(%rsp)                  # x[4] = arg 4
    mov     %r9,  40(%rsp)                  # x[5] = arg 5
    mov     %rax, 64(%rsp)                  # x[8] = syscall number
    mov     %rcx, 248(%rsp)                 # elr = return rip
    mov     %r11, 256(%rsp)                 # spsr = saved rflags
    // Same CPU's slot as entry stored to. At this point rcx (return RIP)
    // and r11 (flags) are already saved in the frame, rax (syscall number)
    // is about to be overwritten below, and rbx is saved into the frame
    // just after -- so only rbx needs preserving across CPUID, and the
    // push is safe: this is the kernel stack, IF is masked, and the frame
    // build owns everything below RSP.
    push    %rbx
    mov     $1, %eax
    cpuid
    shr     $24, %ebx
    and     $7, %ebx
    mov     user_rsp_scratch(,%rbx,8), %rax
    pop     %rbx
    mov     %rax, 264(%rsp)                 # sp = user rsp

    // Save callee-saved registers into the frame
    mov     %rbx, 48(%rsp)                  # x[6]
    mov     %rbp, 56(%rsp)                  # x[7]
    mov     %r12, 72(%rsp)                  # x[9]
    mov     %r13, 80(%rsp)                  # x[10]
    mov     %r14, 88(%rsp)                  # x[11]
    mov     %r15, 96(%rsp)                  # x[12]
    mov     %rdi, 104(%rsp)

    // Call Rust syscall handler: rdi = &mut Frame
    mov     %rsp, %rdi
    call    rust_syscall

    // Restore callee-saved registers
    mov     48(%rsp), %rbx
    mov     56(%rsp), %rbp
    mov     72(%rsp), %r12
    mov     80(%rsp), %r13
    mov     88(%rsp), %r14
    mov     96(%rsp), %r15

    // Return value into rax
    mov     0(%rsp), %rax

    // Return RIP into rcx, RFLAGS into r11
    mov     248(%rsp), %rcx
    mov     256(%rsp), %r11

    // `sysretq` requires a canonical RIP in %rcx. If it is not canonical the
    // instruction raises #GP *after* switching to the user stack pointer but
    // while still in ring 0 -- a ring-0 fault running on a stack ring 3
    // chose, which is the classic sysret escalation. RIP comes from the
    // frame, and the frame is writable by anything that edits it (signal
    // return, fork, ptrace later), so it cannot be assumed canonical.
    //
    // Canonical means bits 63:47 are all equal. Shifting left by 16 and
    // arithmetic-shifting back reproduces the address exactly when that
    // holds, and changes it when it does not.
    mov     %rcx, %r10
    shl     $16, %r10
    sar     $16, %r10
    cmp     %rcx, %r10
    jne     3f

    mov     104(%rsp), %rdi
    mov     8(%rsp), %rsi
    mov     16(%rsp), %rdx
    mov     24(%rsp), %r10
    mov     32(%rsp), %r8
    mov     40(%rsp), %r9
    mov     264(%rsp), %rsp                 # restore user stack
    sysretq

    // Not canonical: return the slow way. `iretq` takes the address from the
    // stack frame it pops and faults *before* changing privilege, so a bad
    // address produces an ordinary #GP attributable to the user task rather
    // than a fault in ring 0.
3:  mov     264(%rsp), %r10                 # user rsp
    pushq   $0x1b                           # SS
    push    %r10                            # RSP
    push    %r11                            # RFLAGS
    pushq   $0x23                           # CS
    push    %rcx                            # RIP
    iretq

// Initialise the MSRs for the `syscall` / `sysret` instruction pair.
// Called once during x86 CPU initialisation.
.global x86_syscall_init
x86_syscall_init:
    // EFER.SCE (bit 0) = System Call Extensions enable
    mov     $0xc0000080, %ecx               # IA32_EFER
    rdmsr
    or      $1, %eax
    wrmsr

    // STAR MSR (0xc0000081):
    //   bits 47:32 = kernel CS/SS (0x08)
    //   bits 63:48 = user CS/SS (0x10 -> sysret loads CS=0x23, SS=0x1b)
    mov     $0xc0000081, %ecx
    xor     %eax, %eax
    mov     $0x00130008, %edx               # user 0x13 (0x23/0x1b), kernel 0x08
    wrmsr

    // LSTAR MSR (0xc0000082): target RIP on syscall
    mov     $0xc0000082, %ecx
    mov     $syscall_entry, %rax
    mov     %rax, %rdx
    shr     $32, %rdx
    wrmsr

    // FMASK MSR (0xc0000084): RFLAGS mask on syscall entry
    // Mask IF (0x200), TF (0x100), DF (0x400)
    mov     $0xc0000084, %ecx
    mov     $0x700, %eax                    # IF | TF | DF
    xor     %edx, %edx
    wrmsr

    ret

.section ".data"
.align 8
// Per-CPU syscall scratch: word 0 of each 8-byte slot is the saved user RSP.
// Indexed by (APIC ID & 7), matching arch::cpu_id. One global word was safe
// with one CPU; two CPUs in `syscall` at once would store to the same word
// and return to each other's stacks.
.global user_rsp_scratch
user_rsp_scratch:  .quad 0, 0, 0, 0, 0, 0, 0, 0
.global kernel_sp_scratch
kernel_sp_scratch: .quad 0, 0, 0, 0, 0, 0, 0, 0

// ------------------------------------------------------------ resume_user --
//
// Return to ring 3 with a saved Frame, used by fork and by signal return.
//   rdi = *const Frame
//   rsi = cr3 (address space root)
//   rdx = kernel stack top to switch to
//
// Frame layout (34 u64 = 272 bytes):
//   x[0..31] at offset 0..247
//   elr      at offset 248  -> return RIP
//   spsr     at offset 256  -> RFLAGS
//   sp       at offset 264  -> user RSP
//
// The `.text` below is not decoration: this function used to follow the
// `.section ".data"` holding the scratch words, so it was assembled into the
// data section. It would have been the only executable code nk placed in a
// non-executable section -- a fault on the first instruction of every fork
// return and signal return.
.section ".text", "ax"
.global resume_user
resume_user:
    mov     %rdx, %rsp                      # kernel stack
    mov     %rsi, %cr3                      # address space

    // Load the frame fields into the right registers for iretq
    mov     248(%rdi), %rcx                 # rip
    mov     256(%rdi), %r11                 # rflags
    mov     264(%rdi), %r10                 # user rsp

    // Restore the general registers the frame saved
    mov     0(%rdi),  %rax
    mov     8(%rdi),  %rsi
    mov     16(%rdi), %r9                   # careful: rdx is needed below
    mov     24(%rdi), %r8
    mov     48(%rdi), %rbx
    mov     56(%rdi), %rbp
    mov     72(%rdi), %r12
    mov     80(%rdi), %r13
    mov     88(%rdi), %r14
    mov     96(%rdi), %r15

    // Build the iretq frame
    pushq   $0x1b                           # SS (user data, RPL 3)
    pushq   %r10                            # RSP
    pushq   %r11                            # RFLAGS
    pushq   $0x23                           # CS (user code, RPL 3)
    pushq   %rcx                            # RIP

    // rdi and rdx last, since rdi held the frame pointer
    mov     16(%rdi), %rdx
    mov     0(%rdi),  %rdi

    iretq
