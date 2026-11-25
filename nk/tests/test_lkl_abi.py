"""ELF relocation fixtures for nk's build/dynamic-runner ABI gate."""
from pathlib import Path
import struct
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "ldk"))
import lkl_abi


def fixture(machine=62, wrong=False, relocation=None, table_size=252 * 8):
    names = bytearray(b"\0")

    def symbol(name, kind, section, value, size):
        at = len(names)
        names.extend(name.encode() + b"\0")
        return struct.pack("<IBBHQQ", at, kind, 0, section, value, size)

    symbols = bytearray(b"\0" * 24)
    base = 48
    symbols += symbol("syscall_table", 1, 2, base, table_size)
    symbols += symbol("", 3, 1, 0, 0)  # section symbol used by GCC relocations
    for i, name in enumerate(("sys_exec_files", "__se_sys_nk_file_get", "sys_nk_file_pread",
                              "sys_nk_file_put", "sys_ni_syscall")):
        symbols += symbol(name, 2, 1, (i + 1) * 16, 1)
    relas = bytearray()
    for nr, target in zip((248, 249, 250, 251), (0, 2, 3, 4) if wrong else (0, 1, 2, 3)):
        kind = relocation if relocation is not None else (1 if machine == 62 else 257)
        relas += struct.pack("<QQq", base + nr * 8, (2 << 32) | kind, (target + 1) * 16)
    sections = [(0, b"", 0, 0, 0), (1, bytes(96), 0, 0, 0),
                (1, bytes(base + 252 * 8), 0, 0, 0), (2, symbols, 4, 1, 24),
                (3, names, 0, 0, 0), (4, relas, 3, 2, 24)]
    data = bytearray(bytes(64))
    headers = bytearray()
    for kind, content, link, target, stride in sections:
        offset = len(data)
        data += content
        data += b"\0" * (-len(data) % 8)
        headers += struct.pack("<IIQQQQIIQQ", 0, kind, 0, 0, offset, len(content), link, target, 8, stride)
    section_offset = len(data)
    data += headers
    ident = b"\x7fELF\x02\x01\x01" + bytes(9)
    data[:64] = struct.pack("<16sHHIQQQIHHHHHH", ident, 1, machine, 1, 0, 0, section_offset,
                            0, 64, 0, 0, 64, len(sections), 0)
    return bytes(data)


def member(name, payload):
    header = f"{name:<16}{0:<12}{0:<6}{0:<6}{'100644':<8}{len(payload):<10}`\n".encode()
    assert len(header) == 60
    return header + payload + (b"\n" if len(payload) & 1 else b"")


class AbiTests(unittest.TestCase):
    def test_resolves_section_addends_for_both_targets(self):
        for machine in (62, 183):
            with self.subTest(machine=machine):
                slots = lkl_abi.verify(fixture(machine))
                self.assertIn("__se_sys_nk_file_get", slots[249])

    def test_symbols_existing_does_not_hide_shifted_syscall_slots(self):
        with self.assertRaisesRegex(lkl_abi.Invalid, "249: expected sys_nk_file_get; found sys_nk_file_pread"):
            lkl_abi.verify(fixture(wrong=True))

    def test_archive_name_formats(self):
        elf = fixture()
        for members in (member("lkl.o/", elf), member("#1/5", b"lkl.o" + elf),
                        member("//", b"lkl.o/\n") + member("/0", elf)):
            self.assertEqual(lkl_abi.verify(b"!<arch>\n" + members), lkl_abi.verify(elf))

    def test_duplicate_and_missing_archive_members(self):
        elf = fixture()
        for members in (member("other.o/", elf), member("lkl.o/", elf) * 2):
            with self.assertRaises(lkl_abi.Invalid):
                lkl_abi.verify(b"!<arch>\n" + members)

    def test_rejects_bad_format_truncation_relocations_and_table_size(self):
        for data in (b"!<thin>\n", fixture()[:63], fixture()[:-1], fixture(relocation=2),
                     fixture(table_size=8), fixture(machine=3)):
            with self.subTest(length=len(data)), self.assertRaises(lkl_abi.Invalid):
                lkl_abi.verify(data)


if __name__ == "__main__":
    unittest.main()
