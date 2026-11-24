# Entry from the boot loader, x86-64.
#
# The aarch64 port is handed a device tree by QEMU and learns the machine
# from it (see boot.s). There is no equivalent here: a PC is described by
# ACPI, by probing, and by a hundred years of convention, and none of it is
# a pointer in a register at entry. So this port is told about the machine
# by the Multiboot2 boot loader instead -- GRUB fills in a tag list with the
# memory map, the framebuffer and the modules, which is the same set of
# facts the FDT carries and is what a real PC actually offers.
#
# Multiboot2 rather than Multiboot1: the older protocol cannot describe a
# 64-bit kernel or a framebuffer request, and GRUB has supported the newer
# one for fifteen years. Multiboot2 also means this is loadable by GRUB on
# real hardware and by QEMU's -kernel, which takes the same path.

.code32
.section ".text.boot", "ax"
.global _start

# ---------------------------------------------------------------- header --
#
# The Multiboot2 header has to be in the first 32KB of the image and
# 8-byte aligned. GRUB scans for the magic; without it, nothing loads.
.align 8
multiboot_header:
    .long   0xe85250d6                      # magic
    .long   0                               # architecture: i386 (32-bit protected mode)
    .long   multiboot_header_end - multiboot_header
    .long   -(0xe85250d6 + 0 + (multiboot_header_end - multiboot_header))
    # One optional request: a framebuffer (type 5, flag 1 = optional),
    # with no preferred size and 32 bits per pixel. Under UEFI there is no
    # VGA text mode, so a PC with no serial port shows nk nothing without
    # it; GRUB answers with the GOP framebuffer in info tag 8 (fbcon.rs).
    # Optional, so a loader without graphics still boots nk.
    .align 8
    .short  5                               # type: framebuffer
    .short  1                               # flags: optional
    .long   20                              # size
    .long   0                               # width: no preference
    .long   0                               # height: no preference
    .long   32                              # depth
    .align 8
    .short  0                               # type: end
    .short  0                               # flags
    .long   8                               # size
multiboot_header_end:

# ------------------------------------------------------------- 32-bit entry --
#
# A Multiboot2 loader enters in 32-bit protected mode with paging off, which
# is the one thing about this port that is genuinely more work than the
# aarch64 one: long mode cannot be entered without page tables, so the
# tables have to be built here, in assembly, before any Rust runs.
#
# eax holds the Multiboot2 magic and ebx the address of the tag list. Both
# are preserved through the transition and handed to rust_main.
_start:
_start_common:
    cld
    # A stack, before anything that could possibly need one.
    mov     $__stack_top, %esp

    # Stash what the loader told us in .data before the paging loop
    # clobbers every register.
    mov     %eax, boot_magic
    mov     %ebx, boot_info

    # Zero the BSS. Rust assumes it, and a Multiboot loader does not
    # guarantee it -- the same reason boot.s does this on aarch64.
    mov     $__bss_start, %edi
    mov     $__bss_end, %ecx
    sub     %edi, %ecx
    xor     %al, %al
    rep     stosb

    # ---- page tables for the jump to long mode ----
    #
    # Identity-map the first 4GB with 2MB pages. That covers RAM, the APIC
    # at 0xfee00000 and everything else a PC puts below 4GB, and it is the
    # minimum that makes long mode reachable. paging.rs replaces all of this
    # once Rust is running and knows where memory actually is.
    #
    # Four levels: PML4 -> PDPT -> four page directories. 2MB pages stop the
    # walk at the directory, so no page tables are needed at all.
    mov     $pml4, %eax
    mov     $pdpt, %ebx
    or      $0x03, %ebx                     # present | writable
    mov     %ebx, (%eax)

    mov     $pdpt, %eax
    mov     $pd0, %ebx
    or      $0x03, %ebx
    mov     %ebx, 0(%eax)
    mov     $pd1, %ebx
    or      $0x03, %ebx
    mov     %ebx, 8(%eax)
    mov     $pd2, %ebx
    or      $0x03, %ebx
    mov     %ebx, 16(%eax)
    mov     $pd3, %ebx
    or      $0x03, %ebx
    mov     %ebx, 24(%eax)

    # Fill the four directories: 2048 entries of 2MB = 4GB, each mapping its
    # own index. Bit 7 is PS, which is what makes the entry a 2MB page
    # rather than a pointer to a page table.
    mov     $pd0, %edi
    xor     %ecx, %ecx
    xor     %eax, %eax                      # physical address, low half
1:
    mov     %eax, %ebx
    or      $0x83, %ebx                     # present | writable | page-size
    mov     %ebx, (%edi)
    movl    $0, 4(%edi)                     # high half: all below 4GB
    add     $0x200000, %eax
    add     $8, %edi
    inc     %ecx
    cmp     $2048, %ecx
    jb      1b

    # ---- enter long mode ----
    mov     %cr4, %eax
    or      $(1 << 5), %eax                 # PAE: required for long mode
    # OSFXSR and OSXMMEXCPT: SSE on.
    #
    # Not optional on x86-64. SSE2 is part of the architecture, so the
    # compiler emits it without being asked -- `pxor %xmm0, %xmm0` to zero a
    # structure, `movdqu` to copy one -- and every such instruction raises
    # #UD until OSFXSR says the OS is prepared to save the state. Linux's
    # vprintk_store zeroes a struct that way, so the first thing the kernel
    # did after taking the machine was fault on its own logging.
    #
    # OSFXSR (bit 9) also selects the fxsave/fxrstor layout that
    # nk_fp_save_asm already uses; OSXMMEXCPT (bit 10) routes SIMD
    # exceptions to vector 19 rather than making them #UD too.
    or      $(1 << 9), %eax
    or      $(1 << 10), %eax
    mov     %eax, %cr4

    mov     $pml4, %eax
    mov     %eax, %cr3

    mov     $0xc0000080, %ecx               # EFER
    rdmsr
    or      $(1 << 8), %eax                 # LME: long mode enable
    # NXE too: the page tables Rust builds set the no-execute bit, and
    # setting NX in an entry while EFER.NXE is clear is a reserved-bit
    # fault rather than a no-op.
    or      $(1 << 11), %eax
    wrmsr

    mov     %cr0, %eax
    or      $(1 << 31), %eax                # PG: paging on -- long mode is now active
    or      $(1 << 16), %eax                # WP: honour read-only in ring 0
    # EM must be *clear* for SSE: set, it says "no FPU, emulate it", and every
    # SSE instruction traps. MP set is the pair it is normally used with.
    and     $~(1 << 2), %eax                # EM: clear
    or      $(1 << 1), %eax                 # MP: monitor coprocessor
    mov     %eax, %cr0

    # A 64-bit code segment, and a far jump to load it. Until this jump the
    # CPU is in compatibility mode; after it, real 64-bit.
    lgdt    gdt_pointer
    ljmp    $0x08, $long_mode

# ------------------------------------------------------------- 64-bit entry --
.code64
long_mode:
    # The data segments. In long mode these are mostly ignored, but they
    # must hold a valid selector or the first stack access faults.
    mov     $0x10, %ax
    mov     %ax, %ds
    mov     %ax, %es
    mov     %ax, %ss
    mov     %ax, %fs
    mov     %ax, %gs

    mov     $__stack_top, %rsp
    # The ABI wants the stack 16-byte aligned at a call, and a clean frame
    # pointer chain ends at zero rather than at whatever was here.
    and     $~0xf, %rsp
    xor     %rbp, %rbp

    # rust_main(magic: u64, info: u64) -- read back from .data where 32-bit
    # entry saved them before the page-table setup clobbered every register.
    movl    boot_magic, %edi
    movl    boot_info, %esi
    call    rust_main

park:
    hlt
    jmp     park

# ------------------------------------------------------------------ GDT --
#
# Long mode ignores segment bases and limits, so these descriptors carry
# almost no information: what matters is the L bit (64-bit code) on the code
# segment and that the selectors exist at all. The user segments are here
# because `sysret` requires a specific layout -- kernel code, kernel data,
# user data, user code, in that order, at the offsets STAR names.
# The GDT is in .data rather than .rodata because the TSS descriptor at 0x28
# is filled in at run time: its base is the address of a structure that does
# not exist until the kernel is up, and a read-only GDT cannot be patched.
.section ".data"
.align 16
.global gdt
gdt:
    .quad   0                               # 0x00: null
    .quad   0x00af9a000000ffff              # 0x08: kernel code, 64-bit
    .quad   0x00cf92000000ffff              # 0x10: kernel data
    .quad   0x00cff2000000ffff              # 0x18: user data (ring 3)
    .quad   0x00affa000000ffff              # 0x20: user code, 64-bit (ring 3)
    # 0x28: the TSS descriptors, one per CPU. In long mode this is a
    # *system* descriptor and is sixteen bytes, not eight -- the one place
    # the GDT stops being a list of quads. CPU n's descriptor is at
    # 0x28 + n*16; zeros until tss_x86 writes the base and limit.
.global gdt_tss
gdt_tss:
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .quad   0
gdt_end:

.global gdt_pointer
gdt_pointer:
    .short  gdt_end - gdt - 1
    .quad   gdt

# The boot page tables. In .bss so they cost nothing in the image, and
# 4KB-aligned because cr3 and every table entry ignore the low twelve bits.
.section ".bss", "aw", @nobits
.align 4096
.global pml4
pml4:   .space 4096
pdpt:   .space 4096
pd0:    .space 4096
pd1:    .space 4096
pd2:    .space 4096
pd3:    .space 4096

.section ".data"
.align 8
boot_magic: .long 0
boot_info:  .long 0

// ------------------------------------------------------- AP bring-up --
#
# Secondaries start in 16-bit real mode at a page chosen by the BSP (the
# STARTUP vector << 12), with no stack and no paging. The trampoline below
# is copied there by Rust before the STARTUP IPI: it enables protected mode,
# loads the same GDT the BSP used, enters long mode with the same page
# tables, then jumps to ap_entry_64 with the APIC ID in %edi.
#
# Handoff protocol, all in .data so both sides see it:
#   ap_stack[cpu]  - stack top the AP loads before calling Rust
#   ap_ready[cpu]  - set by the AP once its scheduler state is live
#   ap_go[cpu]     - set by the BSP to release the AP into Rust
# The BSP spins on ap_ready with a timeout; a silent AP is a failed boot,
// not a hang.
.align 8
.global ap_stack
ap_stack:      .quad 0, 0, 0, 0, 0, 0, 0, 0
.global ap_ready
ap_ready:      .quad 0, 0, 0, 0, 0, 0, 0, 0
.global ap_go
ap_go:         .quad 0, 0, 0, 0, 0, 0, 0, 0
// The logical CPU index the BSP is currently bringing up. Written before
// the STARTUP IPI and read by the AP as its own slot; one at a time, so a
// single word is enough and the BSP's serialised loop is what makes it safe.
.global ap_slot
ap_slot:       .quad 0
.global ap_trampoline_src
ap_trampoline_src: .quad ap_trampoline
.global ap_trampoline_len
ap_trampoline_len: .quad ap_trampoline_end - ap_trampoline

// The trampoline lives in .text.boot, not .data: its 16-bit operands emit
// 16-bit relocations, and the linker can only satisfy those within ±32KB.
// In .text.boot code and data sit side by side (tiny distance, links fine),
// and the copy preserves their adjacency (runs fine below 1MB). An earlier
// revision had this in .data after the handoff words, where the same
// relocations spanned megabytes and rust-lld refused with R_X86_64_16 out
// of range -- a link-time distance for a runtime adjacency.
//
// Position-independent all the same: STARTUP sets CS to the page base and
// IP to 0, DS follows CS, and every slot travels inside the copied span.
.section ".text.boot", "ax"
.code16
.global ap_trampoline
ap_trampoline:
    cli
    cld
    // Stage markers on port 0xE9, the QEMU debug console. The AP has no
    // other voice until it reaches 64-bit Rust: everything before that is
    // pre-console, pre-IDT, and a fault there is a silent triple fault and
    // a reset. One byte per milestone turns "the AP hung" into "the AP got
    // as far as D", which is the difference between reading the trace and
    // guessing at the assembly.
    movw    $0xe9, %dx
    movb    $0x41, %al                // 'A': trampoline entry reached
    outb    %al, %dx

    // No memory operands with link-time addresses anywhere in this section.
    // A 16-bit absolute relocation (R_X86_64_16) stores the *link* address
    // truncated to 16 bits, and the kernel links at 1MB -- every such reloc
    // fails no matter how close code and data sit. Instead the copy base is
    // computed at runtime (CS<<4: STARTUP sets CS to the page) and every
    // slot address is base + a same-section difference, which the assembler
    // resolves to a constant with no relocation at all.
    //
    // 32-bit registers in 16-bit code select 32-bit addressing implicitly
    // (GAS emits the 0x67 prefix itself); no explicit addr32, which LLVM
    // rejects on lgdt/ljmp.
    // Explicit operand suffixes throughout. Without them the assembler
    // picks a width from context and picked wrong here more than once:
    // `mov %cs, %ax` became a 32-bit move that left the top half of %esi
    // holding whatever %eax had, so the copy base was garbage above bit 16.
    xorl    %eax, %eax
    movw    %cs, %ax
    shll    $4, %eax
    movl    %eax, %esi                // %esi = copy base, a real address

    movb    $0x42, %al                // 'B': copy base computed
    outb    %al, %dx

    // LGDT with a 32-bit base, encoded by hand for the same reason as the
    // far jump below: `lgdtl` assembles to `lgdtq` in `.code16`, and the
    // 16-bit form would load only 24 bits of the base.
    //
    // 67 66 0F 01 /2 is LGDT m16:32 with 32-bit addressing.
    //
    // Both prefixes are load-bearing. Without 0x67 the ModRM below is
    // decoded with 16-bit addressing, where mod=10 rm=110 means [bp+disp16]
    // rather than [esi+disp32] -- a read from the wrong place entirely,
    // which faulted at 0x802f with a #GP and triple-faulted from there.
    .byte   0x67, 0x66, 0x0f, 0x01            // opcode: LGDT m16:32
    .byte   0x96                              // ModRM: mod=10 reg=2 rm=110 (esi+disp32)
    .long   ap_gdt_ptr - ap_trampoline

    movb    $0x43, %al                // 'C': 32-bit GDT loaded
    outb    %al, %dx

    // 0F 20 C0 / 0F 22 C0 are the 32-bit CR0 moves; in `.code16` the
    // assembler still accepts `movl %cr0, %eax` and emits them, but the
    // `orl $1, %eax` in between assembles as `orw` -- a 16-bit OR that
    // leaves the upper half of the register untouched. That is harmless
    // for PE itself (bit 0), so it is not the fault; spelled out here as
    // bytes anyway, because every other width in this file has been
    // guessed wrong once and the CR0 write is the one that cannot be
    // retried.
    .byte   0x0f, 0x20, 0xc0                  // mov %cr0, %eax
    .byte   0x66, 0x83, 0xc8, 0x01            // or  $1, %eax  (32-bit)
    .byte   0x0f, 0x22, 0xc0                  // mov %eax, %cr0

    movb    $0x44, %al                // 'D': CR0.PE set, about to far jump
    outb    %al, %dx

    // The far jump, encoded by hand.
    //
    // `ljmpl *mem` in `.code16` assembles to `ljmpw` however it is spelled:
    // LLVM applies the 0x66 prefix to the *address* size here rather than
    // the operand size, and the result reads a 16:16 pointer. The selector
    // then comes out of the offset's high half -- 0x0000, the null
    // descriptor -- and the AP stops dead with no fault to show for it,
    // which is exactly what the marker trace showed: D, then nothing.
    //
    // An *immediate* far jump: 66 EA <off32> <sel16>.
    //
    // The memory-indirect form (FF /5) that stood here reached marker D and
    // then went nowhere -- no exception, no triple fault, and `ap_prot`
    // never entered. Rather than keep adjusting prefixes and pointer
    // layouts around it, this removes the operand entirely: no ModRM, no
    // address-size override, no 16:16-versus-16:32 ambiguity about how many
    // bytes of offset the CPU reads. The target is in the instruction.
    //
    // The offset is absolute and the copy address is not known until the
    // BSP chooses it, so `ap_far_imm` names the four bytes for Rust to
    // patch before the STARTUP IPI. The selector is fixed: 0x08, the
    // 32-bit code descriptor in the trampoline's own GDT.
    .byte   0x66, 0xea                        // opcode: JMP FAR ptr16:32
.global ap_far_imm
ap_far_imm:
    .long   0                                 // offset, patched by Rust
    .short  0x08                              // selector: 32-bit kernel code
.global ap_gdt_ptr
ap_gdt_ptr:
    .short  ap_gdt32_end - ap_gdt32 - 1
ap_gdt_base:
    .long   0
.global ap_gdt32
ap_gdt32:
    .long   0, 0
    .long   0x0000ffff, 0x00cf9a00
    .long   0x0000ffff, 0x00cf9200
ap_gdt32_end:
ap_cr3_slot:
    .long   0
ap_gdt64_ptr:
    // LGDT's operand in long mode: a 16-bit limit and a 64-bit base, ten
    // bytes with no padding between them. Spelled as short + quad so the
    // layout cannot drift from what Rust writes into it.
    .short  0
    .quad   0
.global ap_prot
ap_prot:
    .code32
    // 'E' is the first instruction here, before the segment loads: a fault
    // in those would otherwise be indistinguishable from never arriving.
    // %dx is reloaded rather than inherited -- the far jump does not
    // preserve anything this code should rely on.
    movw    $0xe9, %dx
    movb    $0x45, %al                // 'E': far jump landed, 32-bit code live
    outb    %al, %dx

    // A full 32-bit load, not `movw $0x10, %ax`.
    //
    // The 16-bit form leaves the top half of %eax holding whatever the
    // real-mode half left there -- the CR0 value, here -- and the segment
    // loads below take a 32-bit source. The selector then came out as
    // 0x0044 rather than 0x0010: past the three-entry GDT limit, #GP with
    // no IDT behind it, triple fault. The marker trace pinned it exactly:
    // EAX=60000044 at the faulting `mov %eax, %ds`.
    movl    $0x10, %eax
    movl    %eax, %ds
    movl    %eax, %es
    movl    %eax, %ss

    movb    $0x46, %al                // 'F': segment registers loaded
    outb    %al, %dx

    // CR4.PAE before EFER.LME before CR0.PG -- the order is architectural,
    // not stylistic: LME set without PAE makes the PG write a #GP.
    mov     %cr4, %eax
    or      $(1 << 5), %eax
    or      $(1 << 9), %eax
    or      $(1 << 10), %eax
    mov     %eax, %cr4

    movb    $0x47, %al                // 'G': CR4.PAE enabled
    outb    %al, %dx
    // CR3: patched by Rust -- the BSP's live tables, not the boot tables.
    // %esi still holds the copy base from the 16-bit half; the mode switch
    // does not touch it, so the same base + offset form works here.
    mov     (ap_cr3_slot - ap_trampoline)(%esi), %eax
    mov     %eax, %cr3

    movb    $0x48, %al                // 'H': CR3 loaded
    outb    %al, %dx

    // EFER: LME + NXE.
    mov     $0xc0000080, %ecx
    rdmsr
    or      $(1 << 8), %eax
    or      $(1 << 11), %eax
    wrmsr

    movb    $0x49, %al                // 'I': EFER.LME enabled
    outb    %al, %dx
    // CR0: PG + WP + MP, EM clear.
    mov     %cr0, %eax
    or      $(1 << 31), %eax
    or      $(1 << 16), %eax
    or      $(1 << 1), %eax
    and     $~(1 << 2), %eax
    mov     %eax, %cr0

    movb    $0x4a, %al                // 'J': CR0.PG enabled, long mode live
    outb    %al, %dx

    lgdt    (ap_gdt64_ptr - ap_trampoline)(%esi)

    // ap_entry_64 is outside the copy and runs at its link address, so this
    // one *is* an ordinary 32-bit relocation -- which links fine.
    ljmp    $0x08, $ap_entry_64
ap_trampoline_end:
    .code32

// 64-bit AP entry.
//
// Deliberately minimal: stack, marker, halt. Nothing per-CPU, nothing
// shared, no Rust. The milestone is "one AP reaches 64-bit mode on its own
// stack and says so" -- every further step is layered on only once this is
// reproducible, because a fault anywhere before here is a silent triple
// fault and a reset.
.code64
.global ap_entry_64
ap_entry_64:
    // 'K' first: this is the other side of the long-mode far jump, and
    // proving the transition happened is worth more than any register.
    movw    $0xe9, %dx
    movb    $0x4b, %al                // 'K': 64-bit code entered
    outb    %al, %dx

    // Segments: a stack access with a stale selector faults before
    // anything can report it.
    mov     $0x10, %ax
    mov     %ax, %ds
    mov     %ax, %es
    mov     %ax, %ss
    mov     %ax, %fs
    mov     %ax, %gs

    // The logical CPU slot, written by the BSP before the STARTUP IPI.
    //
    // *Not* CPUID.1 EBX[31:24]. That field is the initial APIC ID, which is
    // only densely numbered 0..N-1 by accident of topology: with threads
    // per core, or a sparse enumeration, two CPUs can mask to the same
    // low bits and share a TSS, a syscall stack and a scheduler slot. The
    // BSP knows which index it is bringing up, so it says so, and the
    // APIC ID stays a hardware address rather than an array subscript.
    mov     ap_slot(%rip), %rbx

    movw    $0xe9, %dx
    movb    $0x4d, %al                // 'M': per-CPU slot resolved
    outb    %al, %dx

    // This CPU's own stack, allocated by the BSP and touched by nobody else.
    lea     ap_stack(%rip), %rax
    mov     (%rax,%rbx,8), %rsp
    and     $~0xf, %rsp
    xor     %rbp, %rbp

    movb    $0x4c, %al                // 'L': 64-bit stack installed
    outb    %al, %dx

    // Tell the BSP this CPU is alive. Written before the Rust call, not
    // after: if Rust faults, the BSP still learns the AP got this far,
    // which is the difference between "trampoline broken" and "Rust entry
    // broken" in a silent-fault world.
    lea     ap_ready(%rip), %rax
    movq    $1, (%rax,%rbx,8)

    movb    $0x4e, %al                // 'N': entering Rust
    outb    %al, %dx

    // cpu index already in %rdi from the slot load... except it is in %rbx.
    // Move it: rust_ap_main(cpu: u64).
    mov     %rbx, %rdi
    call    rust_ap_main

    // Only reached if Rust returns, which it never does. Kept as the last
    // resort rather than removed: a return here means the impossible
    // happened, and halting is still the right answer.
1:  cli
    hlt
    jmp     1b

# ------------------------------------------------------------- PVH note --
#
# QEMU's -kernel refuses a plain Multiboot2 ELF: it loads either a Linux
# bzImage or an ELF carrying an "XEN_ELFNOTE_PHYS32_ENTRY" note, and says so
# ("Error loading uncompressed kernel without PVH ELF Note"). The note is
# four lines and names a 32-bit entry point, so the same image boots both
# ways: GRUB and real hardware take the Multiboot2 header above, QEMU's
# -kernel takes this.
#
# The PVH entry arrives in 32-bit protected mode with paging off -- the same
# state Multiboot2 promises -- but with %ebx pointing at an hvm_start_info
# rather than a Multiboot2 tag list. They are told apart by the magic in
# %eax, which the shared entry path preserves.
.section .note.Xen, "a", @note
    .align 4
    .long   2f - 1f                 # namesz
    .long   4f - 3f                 # descsz
    .long   18                      # type: XEN_ELFNOTE_PHYS32_ENTRY
1:  .asciz  "Xen"
2:  .align 4
3:  .long   pvh_start
4:  .align 4

.section ".text.boot", "ax"
.code32
pvh_start:
    # PVH hands over no magic in %eax, so one is invented here to mark the
    # path: rust_main only needs to know which structure %ebx points at.
    mov     $0x50564800, %eax       # "PVH\0", little-endian
    jmp     _start_common
