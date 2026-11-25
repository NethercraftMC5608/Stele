"""Retained-file syscall ABI must be correct on the first build and every rerun."""
import os
from pathlib import Path
import runpy
import tempfile
import unittest

NK = Path(__file__).resolve().parents[1]
PATCH = runpy.run_path(str(NK / "ldk/patch-lkl.py"))


class FilePatch(unittest.TestCase):
    def test_fresh_old_and_current_trees(self):
        for base in (None, 4, 5):
            with self.subTest(base=base), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                files = {
                    "arch/lkl/include/uapi/asm/unistd.h": "#define __NR_device_page (__NR_arch_specific_syscall + 3)\n#define __NR_exec_files (__NR_arch_specific_syscall + 4)\n",
                    "arch/lkl/include/asm/unistd.h": "__SYSCALL(__NR_share_files, sys_share_files)\n",
                    "arch/lkl/kernel/syscalls.c": "static asmlinkage long sys_share_files(long pid);\n",
                    "arch/lkl/drivers/Makefile": "obj-y += existing.o\n",
                    "arch/lkl/Makefile": "LKL_ENTRY_POINTS += lkl_start_kernel\n",
                }
                if base is not None:
                    for i, name in enumerate(("nk_file_get", "nk_file_pread", "nk_file_put")):
                        files["arch/lkl/include/uapi/asm/unistd.h"] += f"#define __NR_{name}\t(__NR_arch_specific_syscall + {base+i})\n"
                for name, content in files.items():
                    path = root / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(content)
                previous = Path.cwd()
                try:
                    os.chdir(root)
                    PATCH["file_backing"](str(NK))
                    first = {name: (root / name).read_text() for name in files}
                    PATCH["file_backing"](str(NK))
                    second = {name: (root / name).read_text() for name in files}
                finally:
                    os.chdir(previous)
                self.assertEqual(first, second)
                text = first["arch/lkl/include/uapi/asm/unistd.h"]
                for name, offset in (("nk_file_get", 5), ("nk_file_pread", 6), ("nk_file_put", 7)):
                    self.assertIn(f"#define __NR_{name}\t(__NR_arch_specific_syscall + {offset})", text)
                    self.assertEqual(text.count(f"#define __NR_{name}"), 1)
                source = first["arch/lkl/kernel/syscalls.c"]
                self.assertNotIn("\\t", source)
                self.assertEqual(source.count("fget((unsigned)fd)"), 1)
                self.assertNotIn("fget_raw", source)


if __name__ == "__main__":
    unittest.main()
