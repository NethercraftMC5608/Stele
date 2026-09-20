// SPDX-License-Identifier: GPL-2.0
/*
 * Retained-file reads for callers that are already inside an LKL syscall.
 *
 * The mapping's file reference is taken as a private syscall, but a page-in
 * can happen *inside* another syscall (a kernel copy whose destination is a
 * lazy file page). Entering LKL again there re-acquires the CPU and trips
 * the owner accounting, so these two do the work in the caller's existing
 * LKL context instead. The syscall forms are for nk contexts.
 */
#include <linux/fs.h>
#include <linux/file.h>

__visible long nk_file_pread_here(void *handle, void *buf, unsigned long len,
				  unsigned long long off)
{
	struct file *f = (struct file *)handle;
	loff_t pos;

	if (!f)
		return -EINVAL;
	pos = (loff_t)off;
	return kernel_read(f, buf, len, &pos);
}

__visible void nk_file_put_here(void *handle)
{
	if (handle)
		fput((struct file *)handle);
}
