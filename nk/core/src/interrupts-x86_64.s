// x86-64 interrupt entry stubs.
//
// The IDT cannot point at an ordinary function. An `extern "C"` Rust function
// ends in `ret`, which pops the return address and nothing else; an interrupt
// returns with `iretq`, which pops RIP, CS, RFLAGS, RSP and SS together and
// restores the interrupt flag. Pointing a gate at a `ret` function leaves four
// words of CPU-pushed frame on the stack, jumps to whatever CS happened to be,
// and never re-enables interrupts -- the machine wedges after the first tick.
//
// A second reason these must be assembly: an interrupt can arrive between any
// two instructions, so the handler has to preserve *every* register, not just
// the callee-saved ones the C ABI names. Rust cannot express that.
//
// So each vector gets a stub that normalises the frame and jumps to common
// code. The frame the common code builds is:
//
//     rsp+0    r15                 <- lowest address, `*mut TrapFrame`
//     +8       r14
//     +16      r13
//     +24      r12
//     +32      r11
//     +40      r10
//     +48      r9
//     +56      r8
//     +64      rbp
//     +72      rdi
//     +80      rsi
//     +88      rdx
//     +96      rcx
//     +104     rbx
//     +112     rax
//     +120     vector
//     +128     error code          (real, or a zero the stub pushed)
//     +136     rip                 \
//     +144     cs                   |
//     +152     rflags               |  pushed by the CPU
//     +160     rsp                  |
//     +168     ss                  /
//
// Alignment: on an interrupt in long mode the CPU aligns RSP to 16 before
// pushing its five words. Five words plus an error code is 48 bytes, so every
// vector reaches the common path 16-byte aligned -- which is why the stubs for
// vectors that carry no error code push a zero rather than skipping it. The
// vector number and fifteen registers add 128 more, keeping the total a
// multiple of 16, so `call` sees the stack the System V ABI requires.

.code64
.section ".text", "ax"

// A vector that does not push an error code: supply a zero so every frame has
// the same shape.
.macro STUB_NOERR vec
.global isr_\vec
isr_\vec:
    pushq   $0
    pushq   $\vec
    jmp     isr_common
.endm

// A vector whose error code the CPU has already pushed.
.macro STUB_ERR vec
.global isr_\vec
isr_\vec:
    pushq   $\vec
    jmp     isr_common
.endm

isr_common:
    push    %rax
    push    %rbx
    push    %rcx
    push    %rdx
    push    %rsi
    push    %rdi
    push    %rbp
    push    %r8
    push    %r9
    push    %r10
    push    %r11
    push    %r12
    push    %r13
    push    %r14
    push    %r15

    // The data segment selectors are not reloaded here. Long mode ignores
    // DS/ES bases, and nk does not yet run ring 3, so there is no ring-3
    // selector to displace. When ring 3 arrives this needs `swapgs` on entry
    // and exit, guarded on the saved CS's low two bits.

    mov     %rsp, %rdi                      // first argument: *mut TrapFrame
    mov     %rsp, %rbx
    sub     $640, %rsp
    fxsave  (%rsp)
    mov     %rsp, %rsi                      // captured FP/SIMD state
    cld                                     // SysV requires DF clear on entry
    call    x86_trap
    fxrstor (%rsp)
    mov     %rbx, %rsp

    pop     %r15
    pop     %r14
    pop     %r13
    pop     %r12
    pop     %r11
    pop     %r10
    pop     %r9
    pop     %r8
    pop     %rbp
    pop     %rdi
    pop     %rsi
    pop     %rdx
    pop     %rcx
    pop     %rbx
    pop     %rax

    add     $16, %rsp                       // drop vector and error code
    iretq

// The vectors that push an error code are 8, 10-14, 17 and 21; every other
// one does not. Getting this wrong shifts the whole frame by one word, so the
// handler would read RIP as the error code and `iretq` would return to CS.
STUB_NOERR 0
STUB_NOERR 1
STUB_NOERR 2
STUB_NOERR 3
STUB_NOERR 4
STUB_NOERR 5
STUB_NOERR 6
STUB_NOERR 7
STUB_ERR   8
STUB_NOERR 9
STUB_ERR   10
STUB_ERR   11
STUB_ERR   12
STUB_ERR   13
STUB_ERR   14
STUB_NOERR 15
STUB_NOERR 16
STUB_ERR   17
STUB_NOERR 18
STUB_NOERR 19
STUB_NOERR 20
STUB_ERR   21
STUB_NOERR 22
STUB_NOERR 23
STUB_NOERR 24
STUB_NOERR 25
STUB_NOERR 26
STUB_NOERR 27
STUB_NOERR 28
STUB_NOERR 29
STUB_NOERR 30
STUB_NOERR 31

// The IRQ and software vectors. 32-47 are the remapped PIC lines; the rest
// exist so a spurious or unexpected vector lands somewhere that reports
// itself instead of executing whatever byte was at offset zero.
//
// `.altmacro` is what makes `%v` expand to the *value* of v rather than the
// three characters "%v" -- but only where a macro argument is expected. That
// is why the table below goes through a macro too: a bare `.quad isr_%v` in
// ordinary statement position does not expand and the assembler reports it as
// a non-relocatable expression.
.altmacro
.set v, 32
.rept 224
    STUB_NOERR %v
    .set v, v + 1
.endr

// A table of the stub addresses, so the Rust side can install them in a loop
// rather than naming 256 symbols.
.macro ENTRY vec
    .quad isr_\vec
.endm

.section ".rodata"
.align 8
.global isr_table
isr_table:
.set v, 0
.rept 256
    ENTRY %v
    .set v, v + 1
.endr
