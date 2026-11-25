#!/usr/bin/env python3
"""What nk needs changed in arch/lkl, and why.

Run inside ldk's container with the LKL tree as the working directory. Every
patch is idempotent, because the tree is a docker volume that survives between
builds and this runs on every one of them.

These are all the same kind of change: `arch/lkl` was written for a host that
is a Unix process, and nk is a host that is a kernel with hardware. Where the
two disagree the fix belongs here, in the architecture port, and not in a
translation layer on nk's side -- a translation layer is a table with an entry
per system call, which is the thing this project exists to avoid writing.
"""
import os
import re
import shutil
import sys

# Exactly one retained reference; do not fget_raw a second reference on the
# success path, and do not emit literal backslash-t tokens into C source.
C_LINES = """
#include <linux/fs.h>
#include <linux/file.h>

/* Retain the open file, independently of later close/dup/reuse of its fd. */
SYSCALL_DEFINE1(nk_file_get, long, fd)
{
    struct file *f = fget((unsigned)fd);
    return f ? (long)f : -EBADF;
}

SYSCALL_DEFINE4(nk_file_pread, unsigned long, handle, unsigned long, buf,
               unsigned long, len, unsigned long long, off)
{
    struct file *f = (struct file *)handle;
    loff_t pos = (loff_t)off;
    if (!f)
        return -EINVAL;
    return kernel_read(f, (void *)buf, len, &pos);
}

SYSCALL_DEFINE1(nk_file_put, unsigned long, handle)
{
    if (handle)
        fput((struct file *)handle);
    return 0;
}
""".splitlines()


# Four gigabytes of virtual address space for Linux's vmalloc arena, starting
# at four. nk's paging reserves the same window; the two have to agree.
VMALLOC_BASE = '0x100000000UL'
VMALLOC_TOP = '0x17fffffffUL'
# Linux's linear map at six gigabytes and its user mmap base at seven, both
# inside the window nk reserves and clear of its vmalloc arena.
# The linear map is a real physical address: nk reserves this range and hands
# it back from shmem_init, so Linux's identity __pa() tells the truth.
MEMORY_START = '0x50000000'
# The stack top, which arch/lkl otherwise puts just below the linear map --
# and therefore inside nk's identity map of RAM.
STACK_TOP = '0x1e0000000UL'
TASK_BASE = '0x1c0000000'

UACCESS_H = '''/* SPDX-License-Identifier: GPL-2.0 */
/*
 * User access for a host that has a real user address space.
 *
 * The stock arch/lkl assumes kernel and user memory are the same memory and
 * makes copy_from_user a memcpy. That is true of every host LKL was written
 * for and false of a host that runs its processes at EL0 with their own
 * translation tables. The host provides the three primitives below; every
 * other user accessor in the kernel is built out of them.
 */
#ifndef _ASM_LKL_UACCESS_H
#define _ASM_LKL_UACCESS_H

#include <linux/string.h>
#include <asm-generic/access_ok.h>

/* All three return the number of bytes NOT transferred, as Linux expects. */
unsigned long lkl_copy_from_user(void *to, const void *from, unsigned long n);
unsigned long lkl_copy_to_user(void *to, const void *from, unsigned long n);
unsigned long lkl_clear_user(void *to, unsigned long n);

static inline unsigned long
raw_copy_from_user(void *to, const void __user *from, unsigned long n)
{
\treturn lkl_copy_from_user(to, (__force const void *)from, n);
}
#define raw_copy_from_user raw_copy_from_user

static inline unsigned long
raw_copy_to_user(void __user *to, const void *from, unsigned long n)
{
\treturn lkl_copy_to_user((__force void *)to, from, n);
}
#define raw_copy_to_user raw_copy_to_user

static inline unsigned long __clear_user(void __user *to, unsigned long n)
{
\treturn lkl_clear_user((__force void *)to, n);
}
#define __clear_user __clear_user

#include <asm-generic/uaccess.h>

#endif /* _ASM_LKL_UACCESS_H */
'''


def say(what):
    print(f"  patched: {what}")


def edit(path, old, new, marker):
    """Replace `old` with `new` in `path`, once, unless `marker` is present."""
    s = open(path).read()
    if marker in s:
        return False
    if old not in s:
        sys.exit(f"patch-lkl: {path} no longer contains:\n{old}")
    open(path, 'w').write(s.replace(old, new, 1))
    return True


def arm64_open_flags():
    """arm64's open flags, not asm-generic's.

    arm64 overrides four of them, and the collision is the worst kind:
    O_DIRECTORY is 1<<14 on arm64 and 1<<14 is O_DIRECT in asm-generic. So a
    program opening a directory asks LKL for direct I/O, gets EINVAL from a
    filesystem that has none, and busybox reports "can't open '/': Invalid
    argument" with nothing anywhere mentioning a flag.

    Only the file arm64 actually overrides. Its other uapi/asm headers describe
    structures LKL defines for itself -- ptrace, sigcontext -- and copying those
    would replace working definitions with ones for hardware LKL does not have.
    """
    dst = 'arch/lkl/include/uapi/asm/fcntl.h'
    if os.path.exists(dst):
        return
    shutil.copy('arch/arm64/include/uapi/asm/fcntl.h', dst)
    # The generated wrapper around asm-generic is only created when there is no
    # real header, and it is already there from the last build.
    gen = 'arch/lkl/include/generated/uapi/asm/fcntl.h'
    if os.path.exists(gen):
        os.remove(gen)
    say("arch/lkl uses arm64's open flags")


def host_user_access():
    """Let Linux do its own user access.

    arch/lkl selects UACCESS_MEMCPY: kernel and user share one flat address
    space, so copy_from_user is a memcpy. On nk they do not share one, so
    every pointer-bearing syscall had to be described in a table on nk's side
    and its buffers bounced across.

    Linux already knows which arguments are user pointers -- it marks them
    __user -- so asking the host makes every system call work for the same
    reason it works on real hardware, and there is nothing left to enumerate.
    """
    dst = 'arch/lkl/include/asm/uaccess.h'
    if os.path.exists(dst):
        return
    edit('arch/lkl/Kconfig',
         '\tselect UACCESS_MEMCPY\n',
         '\t# UACCESS_MEMCPY dropped: see arch/lkl/include/asm/uaccess.h\n',
         'UACCESS_MEMCPY dropped')
    open(dst, 'w').write(UACCESS_H)
    say('arch/lkl asks the host for user access')


def host0_signals():
    """host0 must not become permanently unable to create tasks.

    LKL makes every host task with kernel_thread(), and switches to host0 --
    its own init -- to do it. kernel_thread() returns -ERESTARTNOINTR when a
    signal is pending on the caller, which is right for a task that will handle
    the signal and retry. host0 never handles anything: it is a kernel thread
    with no signal handling at all, so one pending signal is pending for ever
    and no host task can be created again. A shell that forks a few times meets
    this and reports "can't fork: Unknown error 513".

    Dropping them is safe for the reason it is necessary: nothing reads them.
    This is where LKL's task model shows through -- a real fork would clone the
    caller, and then the caller's signal state would be the one that mattered.
    """
    if edit('arch/lkl/kernel/syscalls.c',
            '\tswitch_to_host_task(host0);\n',
            '\tswitch_to_host_task(host0);\n'
            '\t/* nk: host0 never dequeues a signal, and one pending would\n'
            '\t * stop kernel_thread() making any further host task. */\n'
            '\tflush_signals(current);\n',
            'flush_signals'):
        say('host0 drops pending signals before creating a task')


def linux_address_space():
    """Put Linux's virtual addresses somewhere nk is not.

    With CONFIG_MMU, arch/lkl wants an address space to itself: its vmalloc
    arena is VMALLOC_START..VMALLOC_END, which it declares as 0..0xffffffff,
    and its linear map starts at CONFIG_LKL_MEMORY_START, which defaults to
    0x50000000. On every other LKL host that is a Unix process's address space
    and the low four gigabytes are free. On nk they are not: nk's image, the
    devices and the identity map of RAM are all in them, and Linux mapping
    over them would be a kernel overwriting itself.

    So Linux is moved above all of it -- its vmalloc arena at four gigabytes
    and its linear map at six. Both are empty on this machine and both are
    well under the 512GB a three-level page table can address, which matters
    because arch/lkl uses three levels and a virtual address that does not fit
    is one Linux indexes its own tables with out of range.

    nk reserves the same window in `paging::reserve_linux_window`, and the two
    have to agree.

    Matched by tokens rather than by exact text: the file separates a #define
    from its value with tabs, and a patch that has to reproduce them exactly
    is a patch that breaks on whitespace nobody can see.
    """
    _vmalloc_arena()
    _linear_map()


def define(path, values):
    """Set `#define NAME ...` to the value wanted, for each name given.

    Every run, and only rewriting when something differs. The tree is a docker
    volume that survives between builds, so a patch here is a migration rather
    than an edit: one that only knows how to apply itself can never be changed
    afterwards, and deleting the line that set an address does not put the old
    one back.

    Matched by tokens, because these headers separate a name from its value
    with tabs and a patch that has to reproduce invisible whitespace is one
    that breaks on whitespace nobody can see.
    """
    s = open(path).read()
    out, seen, changed = [], set(), False
    for line in s.split(chr(10)):
        word = line.split()
        if len(word) >= 3 and word[0] == '#define' and word[1] in values:
            seen.add(word[1])
            fixed = '#define ' + word[1] + ' ' + values[word[1]]
            changed = changed or fixed != line
            out.append(fixed)
        else:
            out.append(line)
    missing = set(values) - seen
    if missing:
        sys.exit('patch-lkl: ' + path + ' has no ' + ', '.join(sorted(missing)))
    if changed:
        open(path, 'w').write(chr(10).join(out))
    return changed


def _vmalloc_arena():
    """Move everything Linux places by address into the window nk reserved.

    The definitions that matter are the MMU ones. arch/lkl's pgtable.h has a
    VMALLOC_START of its own, and it is in the `#ifndef CONFIG_MMU` half --
    patching that one changes nothing and looks like it worked. With MMU the
    values come from pgtable-mmu-3level.h, where VMALLOC_START is
    `memory_end + VMALLOC_OFFSET`: directly on top of Linux's memory, which on
    nk is inside the identity map of RAM.

    STACK_TOP is the same problem from the other side -- it is
    CONFIG_LKL_MEMORY_START minus a little, so it lands just *below* the linear
    map, also in nk's RAM.
    """
    changed = define('arch/lkl/include/asm/pgtable-mmu-3level.h', {
        'VMALLOC_START': VMALLOC_BASE,
        'VMALLOC_END': VMALLOC_TOP,
    })
    changed |= define('arch/lkl/include/asm/processor.h', {
        'STACK_TOP': STACK_TOP,
        'STACK_TOP_MAX': STACK_TOP,
    })
    if changed:
        say("Linux's vmalloc arena and stack moved into nk's window")


def _linear_map():
    """Set arch/lkl's memory-layout defaults to the ones nk agrees with.

    Both are hex symbols with no prompt, so Kconfig takes their value from the
    `default` line and ignores anything .config says -- silently, which looks
    exactly like an assignment that worked.

    Written every run rather than once, because the tree is a docker volume
    that survives between builds: a patch here is a migration, not an edit, and
    one that only knows how to apply itself cannot be changed later. Deleting
    the line that set an address does not put the old one back, and the symptom
    of that is a device programmed with an address that used to be right.

    LKL_MEMORY_START is the linear map, and it is a *physical* address because
    LKL's __pa() is the identity -- a physical address is the virtual address
    it was mapped at. On a host that is a Unix process nothing notices, since
    nothing does real DMA. nk hands Linux real hardware, so this has to be
    memory that is really there, and nk reserves exactly this range.
    """
    path = 'arch/lkl/Kconfig'
    s = open(path).read()
    want = {'LKL_MEMORY_START': MEMORY_START, 'LKL_TASK_UNMAPPED_BASE': TASK_BASE}
    out, seen, changed, symbol = [], 0, False, None
    for line in s.split(chr(10)):
        word = line.split()
        if len(word) == 2 and word[0] == 'config':
            symbol = word[1]
        if len(word) == 2 and word[0] == 'default' and symbol in want:
            seen += 1
            fixed = line.split('default')[0] + 'default ' + want[symbol]
            changed = changed or fixed != line
            out.append(fixed)
        else:
            out.append(line)
    if seen != len(want):
        sys.exit('patch-lkl: expected ' + str(len(want)) + ' defaults in ' + path)
    if changed:
        open(path, 'w').write(chr(10).join(out))
        say("Linux's memory layout set to nk's")


def console_driver(shim):
    """A console Linux owns.

    nk answered writes to descriptors 1 and 2 itself, which works for a program
    that prints and is exactly wrong for a shell: `ls > file` is a dup2 of a
    file onto descriptor 1, and nk would have gone on writing to the UART.
    """
    dst = 'arch/lkl/drivers/nk-console.c'
    shutil.copy(f'{shim}/lkl/nk-console.c', dst)
    mk = 'arch/lkl/drivers/Makefile'
    if 'nk-console' not in open(mk).read():
        open(mk, 'a').write('obj-y += nk-console.o\n')
        say('arch/lkl builds nk-console.c')


def file_backing(shim):
    """A retained reference to an open file, for nk's file-backed mappings.

    A mapping that reads its pages through a descriptor number is reading
    through something the process can close, dup over, or recycle; the fault
    after that reads the wrong file and the page still looks mapped. The
    mapping holds a real `struct file` reference instead, taken at mmap and
    read through at fault time.

    These are private syscalls rather than plain exported functions because
    the LKL CPU has to be acquired around kernel code: `lkl_syscall` is the
    one entry that does that, and a direct `kernel_read` from an nk context
    runs Linux code without owning the CPU -- measured as the IRQ pump left
    spinning with every task blocked.
    """
    uapi = "arch/lkl/include/uapi/asm/unistd.h"
    s = open(uapi).read()
    # exec_files already owns +4. Normalize both fresh and previously
    # patched trees in one pass: the old code inserted +4/+5/+6 on a fresh
    # tree and only corrected them on the *next* build, overwriting exec's
    # syscall slot in the first archive.
    for name, offset in [("nk_file_get", 5), ("nk_file_pread", 6), ("nk_file_put", 7)]:
        define = f"#define __NR_{name}\t(__NR_arch_specific_syscall + {offset})"
        pattern = rf"^#define[ \t]+__NR_{name}[^\n]*$"
        if re.search(pattern, s, re.MULTILINE):
            s = re.sub(pattern, lambda _: define, s, flags=re.MULTILINE)
        else:
            s += "\n" + define + "\n"
    open(uapi, "w").write(s)
    tbl = "arch/lkl/include/asm/unistd.h"
    s = open(tbl).read()
    if "sys_nk_file_get" not in s:
        s = s.replace("__SYSCALL(__NR_share_files, sys_share_files)",
                      "__SYSCALL(__NR_share_files, sys_share_files)\n"
                      "__SYSCALL(__NR_nk_file_get, sys_nk_file_get)\n"
                      "__SYSCALL(__NR_nk_file_pread, sys_nk_file_pread)\n"
                      "__SYSCALL(__NR_nk_file_put, sys_nk_file_put)", 1)
        open(tbl, "w").write(s)
    c = "arch/lkl/kernel/syscalls.c"
    s = open(c).read()
    # Keep the prototype and the definition in step across edits.
    s = s.replace("static asmlinkage long sys_nk_file_get(int fd);",
                  "static asmlinkage long sys_nk_file_get(long fd);")
    s = s.replace("static asmlinkage long sys_nk_file_pread(unsigned long handle, unsigned long buf, unsigned long len, unsigned long off);",
                  "static asmlinkage long sys_nk_file_pread(unsigned long handle, unsigned long buf, unsigned long len, unsigned long long off);")
    open(c, "w").write(s)
    s = open(c).read()
    if "sys_nk_file_get" not in s:
        # The syscall table in asm/unistd.h names these before the
        # definitions below, so they need prototypes first, the same shape
        # `sys_share_files` already has.
        s = s.replace(
            "static asmlinkage long sys_share_files(long pid);",
            "static asmlinkage long sys_share_files(long pid);\n"
            "static asmlinkage long sys_nk_file_get(long fd);\n"
            "static asmlinkage long sys_nk_file_pread(unsigned long handle, unsigned long buf, unsigned long len, unsigned long long off);\n"
            "static asmlinkage long sys_nk_file_put(unsigned long handle);",
            1)
        open(c, "w").write(s)
    s = open(c).read()
    marker = "SYSCALL_DEFINE1(nk_file_get"
    if marker in s:
        # Refresh the block in place so edits to C_LINES take effect.
        i = s.index("\n#include <linux/fs.h>\n", s.index(marker) - 2000)
        head = s[:i]
        open(c, "w").write(head + "\n".join(C_LINES) + "\n")
        say("arch/lkl: retained-file syscalls (updated)")
    else:
        open(c, "a").write("\n".join(C_LINES) + "\n")
        say("arch/lkl: retained-file syscalls")
    # Two of the three operations are also needed *directly* when the caller
    # is already inside an LKL syscall (a kernel copy whose destination is a
    # lazy file page): a nested lkl_syscall would re-acquire the CPU and trip
    # LKL's owner accounting -- measured as "bad count while changing owner".
    # The direct helpers run in the caller's existing LKL context.
    dst = "arch/lkl/drivers/nk-file.c"
    shutil.copy(f"{shim}/lkl/nk-file.c", dst)
    mk = "arch/lkl/drivers/Makefile"
    if "nk-file" not in open(mk).read():
        open(mk, "a").write("obj-y += nk-file.o\n")
        say("arch/lkl builds nk-file.c")
    top = "arch/lkl/Makefile"
    t = open(top).read()
    if "nk_file_pread_here" not in t:
        marker = "LKL_ENTRY_POINTS := lkl_start_kernel lkl_sys_halt lkl_syscall lkl_trigger_irq"
        if marker in t:
            t = t.replace(marker, marker + " \\\n\tnk_file_pread_here nk_file_put_here", 1)
        else:
            t = t.replace("LKL_ENTRY_POINTS +=", "LKL_ENTRY_POINTS += nk_file_pread_here nk_file_put_here\nLKL_ENTRY_POINTS +=", 1)
        open(top, "w").write(t)
        say("arch/lkl exports the direct retained-file reads")
    # The generated syscall table carries the old signature otherwise, and
    # the build does not notice a header it generated last time.
    gen = "arch/lkl/include/generated/uapi/asm/syscall_defs.h"
    if os.path.exists(gen):
        os.remove(gen)


def thread_files():
    """Let a thread share its creator's descriptor table.

    `new_host_task()` clones every host task from host0, never from the
    caller, so nk has to `unshare(CLONE_FILES)` to give a *process* a table of
    its own. A thread needs the opposite and cannot get it the same way:
    CLONE_FILES is part of what `pthread_create` asks for, and by the time nk
    knows whose thread this is, the task already exists -- there is no clone
    left to pass a flag to.

    Two failures come out of a thread holding a private copy, and they look
    nothing like each other. A descriptor opened after the thread started is
    invisible to it (EBADF). And a close in one thread is not a close: the
    creator's duplicate keeps the file open, so a socket a worker finished
    with never sends FIN and the peer waits for an EOF that cannot arrive --
    which is a hung HTTP response, and is what stopped a ThreadingHTTPServer
    on nk while the identical single-threaded sequence completed.

    So: adopt the table rather than copy it. One syscall, taking the pid to
    share with, called by the new thread on its own way up.
    """
    if 'sys_share_files' in open('arch/lkl/kernel/syscalls.c').read():
        return
    edit('arch/lkl/include/uapi/asm/unistd.h',
         '#define __NR_new_thread_group_leader\t(__NR_arch_specific_syscall + 1)',
         '#define __NR_new_thread_group_leader\t(__NR_arch_specific_syscall + 1)\n'
         '#define __NR_share_files\t\t(__NR_arch_specific_syscall + 2)',
         'share_files syscall number')
    edit('arch/lkl/include/asm/unistd.h',
         '__SYSCALL(__NR_new_thread_group_leader, sys_new_thread_group_leader)',
         '__SYSCALL(__NR_new_thread_group_leader, sys_new_thread_group_leader)\n'
         '__SYSCALL(__NR_share_files, sys_share_files)',
         'share_files in the syscall table')
    edit('arch/lkl/kernel/syscalls.c',
         'static asmlinkage long sys_new_thread_group_leader(void);',
         'static asmlinkage long sys_new_thread_group_leader(void);\n\n'
         'static asmlinkage long sys_share_files(long pid);',
         'share_files declared')
    edit('arch/lkl/kernel/syscalls.c',
         '#include <linux/task_work.h>',
         '#include <linux/task_work.h>\n#include <linux/fdtable.h>\n'
         '#include <linux/sched/task.h>',
         'share_files includes')
    open('arch/lkl/kernel/syscalls.c', 'a').write(SHARE_FILES_C)
    say('arch/lkl can share a descriptor table between threads')


SHARE_FILES_C = chr(10) + chr(10) + """
/*
 * nk: adopt another task's descriptor table.
 *
 * The thread this runs in was cloned from host0 and then given a table of
 * its own; what it needs is the one its creator is using. There is no clone
 * left to pass CLONE_FILES to, so take a reference to theirs and drop ours.
 *
 * Refcount first, swap second, release last: the old table must not be freed
 * while anything still points at it, and taking the reference before the swap
 * means a creator that exits between the two cannot take the table with it.
 */
SYSCALL_DEFINE1(share_files, long, pid)
{
	struct task_struct *t;
	struct files_struct *theirs, *mine;

	rcu_read_lock();
	t = find_task_by_pid_ns((pid_t)pid, &init_pid_ns);
	if (t)
		get_task_struct(t);
	rcu_read_unlock();
	if (!t)
		return -ESRCH;

	task_lock(t);
	theirs = t->files;
	if (theirs)
		atomic_inc(&theirs->count);
	task_unlock(t);
	put_task_struct(t);

	if (!theirs)
		return -ESRCH;
	if (theirs == current->files) {
		/* Already sharing: give back the reference just taken. */
		put_files_struct(theirs);
		return 0;
	}

	task_lock(current);
	mine = current->files;
	current->files = theirs;
	task_unlock(current);

	if (mine)
		put_files_struct(mine);
	return 0;
}
"""


DEVICE_PAGE_C = r'''
extern int nk_device_mmap_allowed(void);

SYSCALL_DEFINE2(device_page, unsigned long, addr, unsigned long, write)
{
    struct mm_struct *mm = current->mm;
    struct vm_area_struct *vma;
    pgd_t *pgd;
    p4d_t *p4d;
    pud_t *pud;
    pmd_t *pmd;
    pte_t *pte;
    spinlock_t *ptl;
    vm_fault_t fault;
    long ret = -EFAULT;

    if (!nk_device_mmap_allowed())
        return -EPERM;
    if (!mm || (addr & ~PAGE_MASK))
        return -EINVAL;
    mmap_read_lock(mm);
    vma = find_vma(mm, addr);
    if (!vma || addr < vma->vm_start || !vma->vm_file ||
        !(vma->vm_flags & VM_SHARED))
        goto out;
    if (!S_ISCHR(file_inode(vma->vm_file)->i_mode) &&
        !S_ISBLK(file_inode(vma->vm_file)->i_mode))
        goto out;
    if (write && !(vma->vm_flags & VM_WRITE))
        goto out;
    fault = handle_mm_fault(vma, addr, write ? FAULT_FLAG_WRITE : 0, NULL);
    if (fault & VM_FAULT_ERROR) {
        ret = (fault & VM_FAULT_OOM) ? -ENOMEM : -EFAULT;
        goto out;
    }
    if (fault & VM_FAULT_RETRY)
        goto out;
    pgd = pgd_offset(mm, addr);
    if (pgd_none(*pgd) || pgd_bad(*pgd))
        goto out;
    p4d = p4d_offset(pgd, addr);
    if (p4d_none(*p4d) || p4d_bad(*p4d))
        goto out;
    pud = pud_offset(p4d, addr);
    if (pud_none(*pud) || pud_bad(*pud))
        goto out;
    pmd = pmd_offset(pud, addr);
    if (pmd_none(*pmd) || pmd_bad(*pmd))
        goto out;
    pte = pte_offset_map_lock(mm, pmd, addr, &ptl);
    if (!pte)
        goto out;
    if (pte_present(*pte))
        ret = pfn_to_phys(pte_pfn(*pte));
    pte_unmap_unlock(pte, ptl);
out:
    mmap_read_unlock(mm);
    return ret;
}
'''


def device_pages():
    edit('arch/lkl/include/uapi/asm/unistd.h',
         '#define __NR_share_files\t\t(__NR_arch_specific_syscall + 2)',
         '#define __NR_share_files\t\t(__NR_arch_specific_syscall + 2)\n'
         '#define __NR_device_page (__NR_arch_specific_syscall + 3)',
         '__NR_device_page')
    edit('arch/lkl/include/asm/unistd.h',
         '__SYSCALL(__NR_share_files, sys_share_files)',
         '__SYSCALL(__NR_share_files, sys_share_files)\n'
         '__SYSCALL(__NR_device_page, sys_device_page)',
         '__NR_device_page')
    edit('arch/lkl/kernel/syscalls.c',
         'static asmlinkage long sys_share_files(long pid);',
         'static asmlinkage long sys_share_files(long pid);\n'
         'static asmlinkage long sys_device_page(unsigned long addr, unsigned long write);\n'
         '#include <linux/mm.h>\n#include <linux/mman.h>',
         'static asmlinkage long sys_device_page')
    path = 'arch/lkl/kernel/syscalls.c'
    s = open(path).read()
    marker = '\nextern int nk_device_mmap_allowed(void);'
    if marker in s:
        start = s.index(marker)
        end = s.index('\n}\n', start) + len('\n}\n')
        s = s[:start] + DEVICE_PAGE_C + s[end:]
    else:
        s += DEVICE_PAGE_C
    open(path, 'w').write(s)


def cpu_strand_fix():
    """Release LKL CPU on host task exit and prevent stranding on inert idle.

    del_host_task() takes the CPU with lkl_cpu_get() on entry. On exit handover,
    Linux's scheduler falls back to the idle task (swapper / host0) when no other
    task is runnable. host0 is inert on nk -- the boot task leaves LKL after
    lkl_start_kernel and never returns to execute LKL's idle loop. Stock
    __switch_to() transfers CPU ownership to host0 and longjmps back via
    TIF_SCHED_JB without del_host_task() calling lkl_cpu_put(), stranding
    cpu.sem at count 0 forever.

    Fix:
    1. In del_host_task(), wake idle_host_task so the handover has a live
       target, and release the CPU (lkl_cpu_drop()) after unregistration.
       drop() never parks in the scheduler the way lkl_cpu_put() does --
       this thread is going away and nobody will wake it.
    2. In __switch_to(), do not transfer cpu.owner to an unbacked host task
       (idle or host0) when a dying task unwinds via TIF_SCHED_JB. Live
       switches, including new_host_task's switch to host0, are untouched:
       they are told apart by prev->exit_state.
    """
    edit('arch/lkl/include/asm/cpu.h',
         'void lkl_cpu_change_owner(lkl_thread_t owner);',
         'void lkl_cpu_change_owner(lkl_thread_t owner);\n'
         'void lkl_cpu_drop(void);',
         'void lkl_cpu_drop(void);')

    edit('arch/lkl/kernel/cpu.c',
         'void lkl_cpu_change_owner(lkl_thread_t owner)\n'
         '{\n'
         '\tlkl_ops->mutex_lock(cpu.lock);\n'
         '\tif (cpu.count > 1)\n'
         '\t\tlkl_bug("bad count while changing owner\\n");\n'
         '\tcpu.owner = owner;\n'
         '\tlkl_ops->mutex_unlock(cpu.lock);\n'
         '}',
         'void lkl_cpu_change_owner(lkl_thread_t owner)\n'
         '{\n'
         '\tlkl_ops->mutex_lock(cpu.lock);\n'
         '\tif (cpu.count > 1)\n'
         '\t\tlkl_bug("bad count while changing owner\\n");\n'
         '\tcpu.owner = owner;\n'
         '\tlkl_ops->mutex_unlock(cpu.lock);\n'
         '}\n'
         '\n'
         '/* Release the CPU from an exiting host thread: like lkl_cpu_put()\n'
         ' * but never parks in the scheduler -- the caller is going away and\n'
         ' * nobody will wake it. A no-op when ownership already moved on. */\n'
         'void lkl_cpu_drop(void)\n'
         '{\n'
         '\tlkl_ops->mutex_lock(cpu.lock);\n'
         '\tif (!cpu.count || !cpu.owner ||\n'
         '\t    !lkl_ops->thread_equal(cpu.owner, lkl_ops->thread_self())) {\n'
         '\t\tlkl_ops->mutex_unlock(cpu.lock);\n'
         '\t\treturn;\n'
         '\t}\n'
         '\tif (--cpu.count > 0) {\n'
         '\t\tlkl_ops->mutex_unlock(cpu.lock);\n'
         '\t\treturn;\n'
         '\t}\n'
         '\tif (cpu.sleepers) {\n'
         '\t\tcpu.sleepers--;\n'
         '\t\tlkl_ops->sem_up(cpu.sem);\n'
         '\t}\n'
         '\tcpu.owner = 0;\n'
         '\tlkl_ops->mutex_unlock(cpu.lock);\n'
         '}',
         'void lkl_cpu_drop(void)')

    edit('arch/lkl/kernel/syscalls.c',
         '\tif (!need_resched() && idle_host_task)\n'
         '\t\twake_up_process(idle_host_task);',
         '\tif (idle_host_task)\n'
         '\t\twake_up_process(idle_host_task);',
         '\tif (idle_host_task)\n\t\twake_up_process(idle_host_task);')

    edit('arch/lkl/kernel/syscalls.c',
         '\nstatic struct task_struct *host0;',
         '\nstruct task_struct *host0;',
         '\nstruct task_struct *host0;')

    edit('arch/lkl/kernel/syscalls.c',
         '\tswitch_to_host_task(task);\n'
         '\thost_task_id--;\n'
         '\tset_ti_thread_flag(ti, TIF_SCHED_JB);\n'
         '\tlkl_ops->jmp_buf_set(&ti->sched_jb, exit_task);\n',
         '\tswitch_to_host_task(task);\n'
         '\thost_task_id--;\n'
         '\twakeup_idle_host_task();\n'
         '\tset_ti_thread_flag(ti, TIF_SCHED_JB);\n'
         '\tlkl_ops->jmp_buf_set(&ti->sched_jb, exit_task);\n'
         '\tlkl_cpu_drop();\n',
          'lkl_cpu_drop();\n}')

    edit('arch/lkl/kernel/threads.c',
         'static struct task_struct *abs_prev = &init_task;',
         'extern struct task_struct *host0;\n'
         'static struct task_struct *abs_prev = &init_task;',
         'extern struct task_struct *host0;')

    edit('arch/lkl/kernel/threads.c',
         '\tBUG_ON(!_next->tid);\n'
         '\tlkl_cpu_change_owner(_next->tid);\n'
         '\n'
         '\tif (test_bit(TIF_SCHED_JB, &_prev_flags)) {\n'
         '\t\t/* Atomic. Must be done before wakeup next */\n'
         '\t\tclear_ti_thread_flag(_prev, TIF_SCHED_JB);\n'
         '\t\t_prev_jb = _prev->sched_jb;\n'
         '\t}\n'
         '\n'
         '\tlkl_ops->sem_up(_next->sched_sem);',
         '\t/* A dying task unwinding via TIF_SCHED_JB must not hand the CPU\n'
         '\t * to an unbacked host task: nobody there will ever put() it.\n'
         '\t * Live switches (prev->exit_state == 0) are untouched. */\n'
         '\tbool orphaned = prev->exit_state != 0 &&\n'
         '\t\t(is_idle_task(next) || next == host0);\n'
         '\n'
         '\tif (!orphaned) {\n'
         '\t\tBUG_ON(!_next->tid);\n'
         '\t\tlkl_cpu_change_owner(_next->tid);\n'
         '\t}\n'
         '\n'
         '\tif (test_bit(TIF_SCHED_JB, &_prev_flags)) {\n'
         '\t\t/* Atomic. Must be done before wakeup next */\n'
         '\t\tclear_ti_thread_flag(_prev, TIF_SCHED_JB);\n'
         '\t\t_prev_jb = _prev->sched_jb;\n'
         '\t}\n'
         '\n'
         '\tif (!orphaned)\n'
         '\t\tlkl_ops->sem_up(_next->sched_sem);',
         'if (lkl_ops->switch_trace)')

    say('del_host_task releases CPU and __switch_to guards unbacked handover')

    # The guard above skipped the handover without releasing: the dying task
    # ran its final switch on a host thread that had entered Linux (count==1),
    # the owner label had already been moved to the dying task's -- dead --
    # tid by the switch that brought it in, and nothing ever put() the count
    # back. Every later lkl_cpu_get parked on cpu.sem behind an owner that no
    # longer existed: init blocked in read, the whole host pool idle, the
    # render wedge. So the orphaned switch releases the whole count (the
    # dying context consumes its nesting as it goes), and the idle loop's
    # put is made tolerant of arriving without the CPU -- which it now does,
    # because the release already cleared it.
    edit('arch/lkl/include/asm/cpu.h',
         'void lkl_cpu_drop(void);',
         'void lkl_cpu_drop(void);\n'
         'void lkl_cpu_release(void);\n'
         'void lkl_cpu_put_if_owned(void);',
         'void lkl_cpu_put_if_owned(void);')

    edit('arch/lkl/kernel/cpu.c',
         'void lkl_cpu_put(void)\n{',
         '/* Release the CPU from the last context that ran on it, whoever\n'
         ' * the owner label says. Only the dying task\'s final switch calls\n'
         ' * this: the host thread is about to park on a semaphore only\n'
         ' * stack-freeing can wake, and the count it entered Linux with has\n'
         ' * no other taker. Clearing the whole count is deliberate -- a\n'
         ' * dying context consumes any nesting it acquired on the way out.\n'
         ' * A no-op when nothing is held. */\n'
         'void lkl_cpu_release(void)\n'
         '{\n'
         '\tlkl_ops->mutex_lock(cpu.lock);\n'
         '\tif (cpu.count) {\n'
         '\t\tcpu.count = 0;\n'
         '\t\tif (cpu.sleepers) {\n'
         '\t\t\tcpu.sleepers--;\n'
         '\t\t\tlkl_ops->sem_up(cpu.sem);\n'
         '\t\t}\n'
         '\t\tcpu.owner = 0;\n'
         '\t}\n'
         '\tlkl_ops->mutex_unlock(cpu.lock);\n'
         '}\n'
         '\n'
         '/* lkl_cpu_put() that tolerates not owning the CPU: the idle loop\n'
         ' * is woken after lkl_cpu_release() cleared the owner, and a put\n'
         ' * then is the "unbalanced put" bug waiting to happen. */\n'
         'void lkl_cpu_put_if_owned(void)\n'
         '{\n'
         '\tlkl_ops->mutex_lock(cpu.lock);\n'
         '\tif (!cpu.count || !cpu.owner ||\n'
         '\t    !lkl_ops->thread_equal(cpu.owner, lkl_ops->thread_self())) {\n'
         '\t\tlkl_ops->mutex_unlock(cpu.lock);\n'
         '\t\treturn;\n'
         '\t}\n'
         '\tif (--cpu.count > 0) {\n'
         '\t\tlkl_ops->mutex_unlock(cpu.lock);\n'
         '\t\treturn;\n'
         '\t}\n'
         '\tif (cpu.sleepers) {\n'
         '\t\tcpu.sleepers--;\n'
         '\t\tlkl_ops->sem_up(cpu.sem);\n'
         '\t}\n'
         '\tcpu.owner = 0;\n'
         '\tlkl_ops->mutex_unlock(cpu.lock);\n'
         '}\n'
         '\n'
         'void lkl_cpu_put(void)\n{',
         'void lkl_cpu_release(void)')

    edit('arch/lkl/kernel/threads.c',
         '\tif (!orphaned) {\n'
         '\t\tBUG_ON(!_next->tid);\n'
         '\t\tlkl_cpu_change_owner(_next->tid);\n'
         '\t}',
         '\tif (orphaned) {\n'
         '\t\t/* Skipping the handover is not enough: the count this\n'
         '\t\t * host thread entered Linux with has no other taker, and\n'
         '\t\t * the owner label may already name the dead tid the\n'
         '\t\t * switch-in moved it to. Release before parking. */\n'
         '\t\tlkl_cpu_release();\n'
         '\t} else {\n'
         '\t\tBUG_ON(!_next->tid);\n'
         '\t\tlkl_cpu_change_owner(_next->tid);\n'
         '\t}',
         'if (lkl_ops->switch_trace)')

    edit('arch/lkl/kernel/syscalls.c',
         '\tfor (;;) {\n'
         '\t\tlkl_cpu_put();\n'
         '\t\tlkl_ops->sem_down(ti->sched_sem);',
         '\tfor (;;) {\n'
         '\t\tlkl_cpu_put_if_owned();\n'
         '\t\tlkl_ops->sem_down(ti->sched_sem);',
         'lkl_cpu_put_if_owned();')

    say('orphaned switch releases the CPU and the idle loop tolerates a cleared owner')

def cpu_handoff_wake():
    """Restore the stock handover in __switch_to.

    The guard above stopped `change_owner` on a dying task's final switch
    and then `lkl_cpu_release()` cleared the count -- but it also skipped
    `sem_up(_next->sched_sem)`, and that wake is the only thing that runs
    the scheduler again. Every boot wedged at the same slice count (386k):
    one task dead in dup3 waiting on its own sched_sem, a dozen host tasks
    parked on theirs, cpu free, no syscalls in flight, nothing left to
    wake anybody.

    The dead-owner case the guard was written for is real, and is handled
    where it belongs now: host-task TLS is generation-qualified so a
    reused slot cannot adopt (or destroy) the previous occupant's Linux
    task, and the watchdog releases a CPU label only when its owner is
    provably dead. The handover itself stays stock: transfer the label,
    wake the next host thread, and let it put() the count it inherits.
    """
    edit('arch/lkl/kernel/threads.c',
         '\t/* A dying task unwinding via TIF_SCHED_JB must not hand the CPU\n'
         '\t * to an unbacked host task: nobody there will ever put() it.\n'
         '\t * Live switches (prev->exit_state == 0) are untouched. */\n'
         '\tbool orphaned = prev->exit_state != 0 &&\n'
         '\t\t(is_idle_task(next) || next == host0);\n'
         '\n'
         '\tif (orphaned) {\n'
         '\t\t/* Skipping the handover is not enough: the count this\n'
         '\t\t * host thread entered Linux with has no other taker, and\n'
         '\t\t * the owner label may already name the dead tid the\n'
         '\t\t * switch-in moved it to. Release before parking. */\n'
         '\t\tlkl_cpu_release();\n'
         '\t} else {\n'
         '\t\tBUG_ON(!_next->tid);\n'
         '\t\tlkl_cpu_change_owner(_next->tid);\n'
         '\t}\n'
         '\n'
         '\tif (test_bit(TIF_SCHED_JB, &_prev_flags)) {\n'
         '\t\t/* Atomic. Must be done before wakeup next */\n'
         '\t\tclear_ti_thread_flag(_prev, TIF_SCHED_JB);\n'
         '\t\t_prev_jb = _prev->sched_jb;\n'
         '\t}\n'
         '\n'
         '\tif (!orphaned)\n'
         '\t\tlkl_ops->sem_up(_next->sched_sem);',
         '\tBUG_ON(!_next->tid);\n'
         '\tlkl_cpu_change_owner(_next->tid);\n'
         '\n'
         '\tif (test_bit(TIF_SCHED_JB, &_prev_flags)) {\n'
         '\t\t/* Atomic. Must be done before wakeup next */\n'
         '\t\tclear_ti_thread_flag(_prev, TIF_SCHED_JB);\n'
         '\t\t_prev_jb = _prev->sched_jb;\n'
         '\t}\n'
         '\n'
         '\tlkl_ops->sem_up(_next->sched_sem);',
         'if (lkl_ops->switch_trace)')

    say('stock CPU handover with wake restored (scheduler can always run again)')

def switch_trace():
    """Record every CPU-owner install and host-task tid assignment.

    The dead-owner strand is installed by a single `__switch_to` -- a
    `change_owner(_next->tid)` where `_next` is a task whose `tid` names
    an nk thread that is gone. nk cannot see `prev`/`next` from outside,
    so the kernel reports them: pid and tid on each side, and each
    `switch_to_host_task` tid write. nk keeps them in a ring and dumps it
    with the stall graph.
    """
    edit('arch/lkl/include/uapi/asm/host_ops.h',
         '\tstruct lkl_dev_pci_ops *pci_ops;\n};',
         '\tstruct lkl_dev_pci_ops *pci_ops;\n'
         '\n'
         '\t/* nk: owner installs and tid assignments, for the stall ring */\n'
         '\tvoid (*switch_trace)(int prev_pid, unsigned long long prev_tid,\n'
         '\t\t\t     int next_pid, unsigned long long next_tid);\n'
         '};',
         'switch_trace')

    edit('arch/lkl/kernel/threads.c',
         '\tBUG_ON(!_next->tid);\n'
         '\tlkl_cpu_change_owner(_next->tid);',
         '\tBUG_ON(!_next->tid);\n'
         '\tif (lkl_ops->switch_trace)\n'
         '\t\tlkl_ops->switch_trace(prev->pid,\n'
         '\t\t\t\t     (unsigned long long)_prev->tid,\n'
         '\t\t\t\t     next->pid,\n'
         '\t\t\t\t     (unsigned long long)_next->tid);\n'
         '\tlkl_cpu_change_owner(_next->tid);',
         'switch_trace(prev->pid,')

    edit('arch/lkl/kernel/threads.c',
         '\ttask_thread_info(task)->tid = lkl_ops->thread_self();\n',
         '\ttask_thread_info(task)->tid = lkl_ops->thread_self();\n'
         '\tif (lkl_ops->switch_trace)\n'
         '\t\tlkl_ops->switch_trace(-1,\n'
         '\t\t\t\t     (unsigned long long)lkl_ops->thread_self(),\n'
         '\t\t\t\t     task->pid,\n'
         '\t\t\t\t     (unsigned long long)task_thread_info(task)->tid);\n',
         'switch_trace(-1,')

    say('switch trace: owner installs and tid assignments recorded in an nk ring')

def del_host_task_probe():
    """Print the host-task exit protocol's vital signs.

    The dead-owner strand is installed by `__switch_to` choosing a task
    whose `tid` names an nk thread that is gone. Three questions decide
    the fix, and none of them is visible from nk:

      * was the chosen task still runnable (`on_rq`) and in what state,
      * was its tid already a dead incarnation's,
      * and -- in `del_host_task` -- did `do_exit` actually finish before
        the TLS destructor returned? The whole protocol rests on the
        task being TASK_DEAD and off the run queue at that point, and
        nothing has ever checked it.
    """
    edit('arch/lkl/include/uapi/asm/host_ops.h',
         '\t/* nk: owner installs and tid assignments, for the stall ring */\n'
         '\tvoid (*switch_trace)(int prev_pid, unsigned long long prev_tid,\n'
         '\t\t\t     int next_pid, unsigned long long next_tid);\n};',
         '\t/* nk: owner installs and tid assignments, for the stall ring */\n'
         '\tvoid (*switch_trace)(int prev_pid, unsigned long long prev_tid,\n'
         '\t\t\t     int next_pid, unsigned long long next_tid);\n'
         '\n'
         '\t/* nk: does the host thread behind this tid still exist? */\n'
         '\tint (*thread_alive)(lkl_thread_t tid);\n};',
         'thread_alive')

    edit('arch/lkl/kernel/threads.c',
         '\tBUG_ON(!_next->tid);\n'
         '\tif (lkl_ops->switch_trace)',
         '\tBUG_ON(!_next->tid);\n'
         '\tif (next->on_rq != 1)\n'
         '\t\tprintk("lkl: SW-NONQUEUED pid %d tid %llx state %ld on_rq %d prev pid %d tid %llx\\n",\n'
         '\t\t\t   next->pid, (unsigned long long)_next->tid,\n'
         '\t\t\t   (long)next->__state, next->on_rq, prev->pid,\n'
         '\t\t\t   (unsigned long long)_prev->tid);\n'
         '\tif (prev->__state == TASK_DEAD)\n'
         '\t\tprintk("lkl: SW-EXIT pid %d tid %llx next pid %d tid %llx\\n",\n'
         '\t\t\t   prev->pid, (unsigned long long)_prev->tid, next->pid,\n'
         '\t\t\t   (unsigned long long)_next->tid);\n'
         '\tif (lkl_ops->thread_alive &&\n'
         '\t    !lkl_ops->thread_alive(_next->tid))\n'
         '\t\tprintk("lkl: SW-DEAD-TID pid %d tid %llx state %ld on_rq %d dead %d prev pid %d\\n",\n'
         '\t\t\t   next->pid, (unsigned long long)_next->tid,\n'
         '\t\t\t   (long)next->__state, next->on_rq, _next->dead, prev->pid);\n'
         '\tif (lkl_ops->switch_trace)',
         'SW-DEAD-TID')

    edit('arch/lkl/kernel/syscalls.c',
         'static void del_host_task(void *arg)\n'
         '{\n'
         '\tstruct task_struct *task = (struct task_struct *)arg;\n'
         '\tstruct thread_info *ti = task_thread_info(task);\n'
         '\n'
         '\tif (lkl_cpu_get() < 0)\n'
         '\t\treturn;\n'
         '\n'
         '\tswitch_to_host_task(task);\n',
         'static void del_host_task(void *arg)\n'
         '{\n'
         '\tstruct task_struct *task = (struct task_struct *)arg;\n'
         '\tstruct thread_info *ti = task_thread_info(task);\n'
         '\n'
         '\tprintk("lkl: del_host_task pid %d enter state %ld on_rq %d tid %llx self %llx\\n",\n'
         '\t\t   task->pid, (long)task->__state, task->on_rq,\n'
         '\t\t   (unsigned long long)ti->tid,\n'
         '\t\t   (unsigned long long)lkl_ops->thread_self());\n'
         '\tif (lkl_cpu_get() < 0)\n'
         '\t\treturn;\n'
         '\n'
         '\tswitch_to_host_task(task);\n'
         '\tprintk("lkl: del_host_task pid %d reentered state %ld on_rq %d\\n",\n'
         '\t\t   task->pid, (long)task->__state, task->on_rq);\n',
         'del_host_task pid')

    edit('arch/lkl/kernel/syscalls.c',
         '\tlkl_ops->jmp_buf_set(&ti->sched_jb, exit_task);\n'
         '\tlkl_cpu_drop();\n'
         '}\n',
         '\tlkl_ops->jmp_buf_set(&ti->sched_jb, exit_task);\n'
         '\tprintk("lkl: del_host_task pid %d after exit_task state %ld on_rq %d dead %d\\n",\n'
         '\t\t   task->pid, (long)task->__state, task->on_rq, ti->dead);\n'
         '\tlkl_cpu_drop();\n'
         '}\n',
         'after exit_task state')

    say('del_host_task probe: pick state, dead tids, and whether do_exit finished')


def exit_jb_gate():
    """Do not let an inner schedule abort do_exit.

    `del_host_task` arms `sched_jb` with `exit_task` and runs do_exit, so
    that when the task's final switch happens the host thread is thrown
    back out of the kernel and can finish exiting. But do_exit can
    schedule *before* the task is dead: a reschedule point while tearing
    the mm down, a lock taken on the way. `__switch_to` consumed the
    jump buffer at that first inner switch -- measured, once per few
    dozen exits: `del_host_task pid 49 after exit_task state 0 on_rq 1`
    where healthy exits all say `state 128 on_rq 0` -- and longjmping out
    of an unfinished do_exit leaves the task TASK_RUNNING and on the run
    queue whose `tid` names a host thread that is about to stop existing.
    The next switch to that task installs the dead tid as the CPU owner,
    `sem_up` wakes nobody, and every other host thread parks on its own
    sched_sem: the machine stops with one sleeper on cpu.sem for ever.

    So the exit path keeps its jump buffer armed across inner schedules
    and parks instead: the task blocks, the host thread blocks with it,
    and both resume when the scheduler runs the task again. Only the
    final switch, with `prev->__state == TASK_DEAD`, consumes the buffer
    and throws the host thread out. Ordinary yields (`thread_sched_jb`,
    used by `lkl_cpu_put` and `switch_to_host_task`) are not `exiting`
    and keep the stock immediate longjmp, which is what makes them
    return to the host at all.
    """
    edit('arch/lkl/include/asm/thread_info.h',
         '\tstruct lkl_jmp_buf sched_jb;\n'
         '\tbool dead;\n',
         '\tstruct lkl_jmp_buf sched_jb;\n'
         '\tbool dead;\n'
         '\t/* Set by del_host_task before arming sched_jb for do_exit:\n'
         '\t * an inner schedule must park and resume the exit, not\n'
         '\t * longjmp out of it and leave a runnable zombie. */\n'
         '\tbool exiting;\n',
         'bool exiting;')

    edit('arch/lkl/kernel/threads.c',
         '\tti->dead = false;\n',
         '\tti->dead = false;\n'
         '\tti->exiting = false;\n',
         'ti->exiting = false;')

    edit('arch/lkl/kernel/threads.c',
         '\tif (test_bit(TIF_SCHED_JB, &_prev_flags)) {\n'
         '\t\t/* Atomic. Must be done before wakeup next */\n'
         '\t\tclear_ti_thread_flag(_prev, TIF_SCHED_JB);\n'
         '\t\t_prev_jb = _prev->sched_jb;\n'
         '\t}\n'
         '\n'
         '\tlkl_ops->sem_up(_next->sched_sem);\n'
         '\tif (test_bit(TIF_SCHED_JB, &_prev_flags)) {\n'
         '\t\tlkl_ops->jmp_buf_longjmp(&_prev_jb, 1);\n'
         '\t} else {\n'
         '\t\tlkl_ops->sem_down(_prev->sched_sem);\n'
         '\t}\n',
         '\tint _consume_jb = 0;\n'
         '\n'
         '\tif (test_bit(TIF_SCHED_JB, &_prev_flags)) {\n'
         '\t\tif (!_prev->exiting || prev->__state == TASK_DEAD) {\n'
         '\t\t\t/* Atomic. Must be done before wakeup next */\n'
         '\t\t\tclear_ti_thread_flag(_prev, TIF_SCHED_JB);\n'
         '\t\t\t_prev_jb = _prev->sched_jb;\n'
         '\t\t\t_consume_jb = 1;\n'
         '\t\t}\n'
         '\t\t/* else: an inner schedule on the way into do_exit.\n'
         '\t\t * Keep the buffer armed and park; the exit resumes\n'
         '\t\t * when the task is scheduled again, and the final\n'
         '\t\t * (TASK_DEAD) switch consumes it. */\n'
         '\t}\n'
         '\n'
         '\tlkl_ops->sem_up(_next->sched_sem);\n'
         '\tif (_consume_jb) {\n'
         '\t\tlkl_ops->jmp_buf_longjmp(&_prev_jb, 1);\n'
         '\t} else {\n'
         '\t\tlkl_ops->sem_down(_prev->sched_sem);\n'
         '\t}\n',
         '_consume_jb = 1;')

    edit('arch/lkl/kernel/syscalls.c',
         '\thost_task_id--;\n'
         '\twakeup_idle_host_task();\n'
         '\tset_ti_thread_flag(ti, TIF_SCHED_JB);\n',
         '\thost_task_id--;\n'
         '\twakeup_idle_host_task();\n'
         '\tti->exiting = true;\n'
         '\tset_ti_thread_flag(ti, TIF_SCHED_JB);\n',
         'ti->exiting = true;')

    say('exit jump buffer survives inner schedules (no runnable zombie with a dead tid)')


def quiet_exit_probe():
    """Keep the anomaly reports, drop the per-exit chatter.

    `del_host_task_probe` earned its lines the hard way -- `after
    exit_task state 0 on_rq 1` is what named the runnable zombie. With
    `exit_jb_gate` in place every healthy exit prints three statements
    to two consoles; a shell boot makes a hundred exits and the log
    drowns. The dead-tid and non-queued switch reports only ever fire on
    the failure, so they stay.
    """
    import re

    def drop(path, pattern):
        s = open(path).read()
        new = re.sub(pattern, '', s)
        if new != s:
            open(path, 'w').write(new)
            say(f"quiet: {path}")

    drop('arch/lkl/kernel/syscalls.c',
         r'\tprintk\("lkl: del_host_task[^;]*;\n')
    drop('arch/lkl/kernel/threads.c',
         r'\tif \(prev->__state == TASK_DEAD\)\n'
         r'\t\tprintk\("lkl: SW-EXIT[^;]*;\n')


def input_event_probe():
    """Do pointer buttons reach the Linux input layer at all?

    Motion works -- the Weston cursor follows the tablet -- but no button
    ever reaches the GTK client: no press/release, no `:active`, nothing
    posted to the daemon. This says whether QEMU's button events make it
    through the virtqueue (in which case libinput/Weston dropped them) or
    never arrive (device setup, nk's interrupt delivery, or the queue).
    """
    edit('drivers/virtio/virtio_input.c',
         '\t\t\tinput_event(vi->idev,\n'
         '\t\t\t\t    le16_to_cpu(event->type),\n'
         '\t\t\t\t    le16_to_cpu(event->code),\n'
         '\t\t\t\t    le32_to_cpu(event->value));\n',
         '\t\t\tif (le16_to_cpu(event->type) == EV_KEY)\n'
         '\t\t\t\tprintk("lkl: virtio-input EV_KEY code %u value %d\\n",\n'
         '\t\t\t\t       le16_to_cpu(event->code),\n'
         '\t\t\t\t       (int)le32_to_cpu(event->value));\n'
         '\t\t\tinput_event(vi->idev,\n'
         '\t\t\t\t    le16_to_cpu(event->type),\n'
         '\t\t\t\t    le16_to_cpu(event->code),\n'
         '\t\t\t\t    le32_to_cpu(event->value));\n',
         'virtio-input EV_KEY')

    say('input probe: EV_KEY events from virtio input devices are reported')


def exec_files():
    edit('arch/lkl/include/uapi/asm/unistd.h',
         '#define __NR_device_page (__NR_arch_specific_syscall + 3)',
         '#define __NR_device_page (__NR_arch_specific_syscall + 3)\n'
         '#define __NR_exec_files (__NR_arch_specific_syscall + 4)',
         '__NR_exec_files')
    edit('arch/lkl/include/asm/unistd.h',
         '__SYSCALL(__NR_device_page, sys_device_page)',
         '__SYSCALL(__NR_device_page, sys_device_page)\n'
         '__SYSCALL(__NR_exec_files, sys_exec_files)',
         '__NR_exec_files')
    edit('arch/lkl/kernel/syscalls.c',
         'static asmlinkage long sys_device_page(unsigned long addr, unsigned long write);',
         'static asmlinkage long sys_device_page(unsigned long addr, unsigned long write);\n'
         'static asmlinkage long sys_exec_files(void);',
         'static asmlinkage long sys_exec_files')
    path = 'arch/lkl/kernel/syscalls.c'
    if 'SYSCALL_DEFINE0(exec_files)' not in open(path).read():
        open(path, 'a').write('''
SYSCALL_DEFINE0(exec_files)
{
    int ret = unshare_files();
    if (ret)
        return ret;
    do_close_on_exec(current->files);
    return 0;
}
''')


def nk_stack_maps():
    edit('fs/proc/task_mmu.c',
         'static int pid_maps_open(struct inode *inode, struct file *file)\n{\n\treturn do_maps_open(inode, file, &proc_pid_maps_op);\n}',
         '''extern unsigned long nk_stack_top(long pid);

static int nk_stack_maps_show(struct seq_file *m, void *v)
{
    unsigned long top = (unsigned long)m->private;
    seq_printf(m, "%08lx-%08lx rw-p 00000000 00:00 0 [stack]\\n",
               top - 262144UL, top);
    return 0;
}

static const struct file_operations nk_stack_maps_ops = {
    .read = seq_read,
    .llseek = seq_lseek,
    .release = single_release,
};

static int pid_maps_open(struct inode *inode, struct file *file)
{
    struct task_struct *task = get_proc_task(inode);
    unsigned long top = task ? nk_stack_top(task_pid_nr(task)) : 0;
    if (task)
        put_task_struct(task);
    if (top) {
        struct mm_struct *mm = proc_mem_open(inode, PTRACE_MODE_READ);
        int ret;
        if (IS_ERR(mm))
            return PTR_ERR(mm);
        if (mm)
            mmdrop(mm);
        ret = single_open(file, nk_stack_maps_show, (void *)top);
        if (!ret)
            file->f_op = &nk_stack_maps_ops;
        return ret;
    }
    return do_maps_open(inode, file, &proc_pid_maps_op);
}''',
         'static int nk_stack_maps_show')


def multi_pci_devices():
    """LKL's PCI host takes a list of devices, one root bus each.

    arch/lkl/drivers/pci.c hosts exactly one device, named by lkl_pci=, and
    says so ("The PCI driver supports only one PCI device"). nk needs at
    least a disk and a network card at once. Every per-device path already
    goes through bus->sysdata / dev->sysdata (config space, BAR override,
    DMA, MSI), so the only change is here: parse a comma-separated list and
    scan each device on its own root bus number, with its own host handle.
    """
    path = 'arch/lkl/drivers/pci.c'
    edit(path,
         'static char *pcidev_name;\n'
         '\n'
         'static int __init setup_pci_device(char *str)\n'
         '{\n'
         '\tif (pcidev_name) {\n'
         '\t\tpr_info("The PCI driver supports only one PCI device.");\n'
         '\t\tpr_info("\'%s\' will be discarded.", str);\n'
         '\t\treturn -1;\n'
         '\t}\n'
         '\tpcidev_name = str;\n'
         '\treturn 0;\n'
         '}',
         '/* nk: lkl_pci=a,b,c -- one root bus per device. */\n'
         '#define LKL_PCI_MAX_DEVICES 8\n'
         'static char *pcidev_names[LKL_PCI_MAX_DEVICES];\n'
         'static int pcidev_count;\n'
         'static struct pci_bus *pcidev_buses[LKL_PCI_MAX_DEVICES];\n'
         '\n'
         'static int __init setup_pci_device(char *str)\n'
         '{\n'
         '\twhile (str && *str) {\n'
         '\t\tchar *comma = strchr(str, \',\');\n'
         '\n'
         '\t\tif (comma)\n'
         '\t\t\t*comma = \'\\0\';\n'
         '\t\tif (pcidev_count == LKL_PCI_MAX_DEVICES) {\n'
         '\t\t\tpr_info("lkl_pci: more than %d devices; \'%s\' discarded\\n",\n'
         '\t\t\t\tLKL_PCI_MAX_DEVICES, str);\n'
         '\t\t\treturn -1;\n'
         '\t\t}\n'
         '\t\tpcidev_names[pcidev_count++] = str;\n'
         '\t\tstr = comma ? comma + 1 : NULL;\n'
         '\t}\n'
         '\treturn 0;\n'
         '}',
         'LKL_PCI_MAX_DEVICES')
    edit(path,
         '\tif (!lkl_ops->pci_ops || !pcidev_name)\n'
         '\t\treturn -1;\n'
         '\n'
         '\tdev = lkl_ops->pci_ops->add(pcidev_name, (void *)memory_start,\n'
         '\t\t\t\t    memory_end - memory_start);\n'
         '\tif (!dev)\n'
         '\t\treturn -1;\n'
         '\n'
         '\tbus = pci_scan_bus(0, &lkl_pci_root_ops, (void *)dev);\n'
         '\tif (!bus) {\n'
         '\t\tlkl_ops->pci_ops->remove(dev);\n'
         '\t\treturn -1;\n'
         '\t}\n'
         '\tpci_walk_bus(bus, lkl_pci_override_resource, NULL);\n'
         '\tpci_bus_add_devices(bus);\n'
         '\tdev_set_drvdata(&pdev->dev, bus);\n'
         '\n'
         '\treturn 0;',
         '\tint i, found = 0;\n'
         '\n'
         '\tif (!lkl_ops->pci_ops || !pcidev_count)\n'
         '\t\treturn -1;\n'
         '\n'
         '\t/* nk: each device on its own root bus; one the host refuses is\n'
         '\t * skipped rather than costing the others. */\n'
         '\tfor (i = 0; i < pcidev_count; i++) {\n'
         '\t\tdev = lkl_ops->pci_ops->add(pcidev_names[i],\n'
         '\t\t\t\t\t    (void *)memory_start,\n'
         '\t\t\t\t\t    memory_end - memory_start);\n'
         '\t\tif (!dev)\n'
         '\t\t\tcontinue;\n'
         '\n'
         '\t\tbus = pci_scan_bus(i, &lkl_pci_root_ops, (void *)dev);\n'
         '\t\tif (!bus) {\n'
         '\t\t\tlkl_ops->pci_ops->remove(dev);\n'
         '\t\t\tcontinue;\n'
         '\t\t}\n'
         '\t\tpci_walk_bus(bus, lkl_pci_override_resource, NULL);\n'
         '\t\tpci_bus_add_devices(bus);\n'
         '\t\tpcidev_buses[i] = bus;\n'
         '\t\tfound++;\n'
         '\t}\n'
         '\tif (!found)\n'
         '\t\treturn -1;\n'
         '\tdev_set_drvdata(&pdev->dev, pcidev_buses);\n'
         '\n'
         '\treturn 0;',
         'each device on its own root bus')
    edit(path,
         '\tstruct pci_bus *bus = (struct pci_bus *)dev_get_drvdata(&pdev->dev);\n'
         '\n'
         '\tif (bus)\n'
         '\t\tpci_walk_bus(bus, lkl_pci_remove_devices, NULL);',
         '\tint i;\n'
         '\n'
         '\t/* nk: every root bus probe created. */\n'
         '\tfor (i = 0; i < pcidev_count; i++)\n'
         '\t\tif (pcidev_buses[i])\n'
         '\t\t\tpci_walk_bus(pcidev_buses[i], lkl_pci_remove_devices,\n'
         '\t\t\t\t     NULL);',
         'every root bus probe created')
    say("arch/lkl: lkl_pci= takes a list, one root bus per device")


if __name__ == '__main__':
    shim = sys.argv[1] if len(sys.argv) > 1 else '/shim'
    arm64_open_flags()
    linux_address_space()
    host_user_access()
    host0_signals()
    thread_files()
    device_pages()
    exec_files()
    nk_stack_maps()
    console_driver(shim)
    file_backing(shim)
    cpu_strand_fix()
    cpu_handoff_wake()
    switch_trace()
    del_host_task_probe()
    exit_jb_gate()
    quiet_exit_probe()
    input_event_probe()
    multi_pci_devices()
