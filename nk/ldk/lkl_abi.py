#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Verify nk's private syscall slots in a relocatable LKL ELF or ar archive.

Read-only and dependency-free. Resolves the syscall table's ELF relocations
to function symbols, including section-symbol+addend references emitted by
GCC. A function merely existing in the symbol table is not proof it occupies
the syscall number nk actually invokes.
"""
import argparse
from dataclasses import dataclass
from pathlib import Path
import struct

EXPECTED = {248: "exec_files", 249: "nk_file_get", 250: "nk_file_pread", 251: "nk_file_put"}


class Invalid(ValueError):
    pass


def span(data, offset, size):
    if offset < 0 or size < 0 or offset > len(data) or size > len(data) - offset:
        raise Invalid("truncated or out-of-range ELF/archive record")
    return data[offset:offset + size]


def object_bytes(data):
    if data.startswith(b"\x7fELF"):
        return data
    if not data.startswith(b"!<arch>\n"):
        raise Invalid("expected an ELF64 relocatable object or regular ar archive")
    offset, names, found = 8, b"", []
    while offset < len(data):
        header = span(data, offset, 60)
        if header[58:] != b"`\n":
            raise Invalid("bad ar member header")
        try:
            size = int(header[48:58].strip())
        except ValueError as error:
            raise Invalid("bad ar member size") from error
        payload = span(data, offset + 60, size)
        name = header[:16].decode("ascii").strip()
        if name == "//":
            names = payload
        elif name.startswith("#1/"):
            try:
                length = int(name[3:])
            except ValueError as error:
                raise Invalid("bad BSD ar name length") from error
            name = span(payload, 0, length).rstrip(b"\0").decode("utf-8")
            payload = span(payload, length, size - length)
        elif name.startswith("/") and name[1:].isdigit():
            tail = span(names, int(name[1:]), len(names) - int(name[1:]))
            end = tail.find(b"/\n")
            if end < 0:
                raise Invalid("unterminated GNU ar member name")
            name = tail[:end].decode("utf-8")
        else:
            name = name.rstrip("/")
        if name == "lkl.o":
            found.append(payload)
        offset += 60 + size + (size & 1)
    if len(found) != 1:
        raise Invalid(f"expected exactly one lkl.o archive member, found {len(found)}")
    return found[0]


@dataclass
class Section:
    kind: int
    offset: int
    size: int
    link: int
    target: int
    stride: int


@dataclass
class Symbol:
    name: str
    kind: int
    section: int
    value: int
    size: int


class Elf:
    def __init__(self, data):
        self.data = object_bytes(data)
        header = span(self.data, 0, 64)
        if header[:7] != b"\x7fELF\x02\x01\x01":
            raise Invalid("expected little-endian ELF64")
        kind, self.machine = struct.unpack_from("<HH", header, 16)
        if kind != 1 or self.machine not in (62, 183):
            raise Invalid("expected x86-64 or aarch64 ET_REL LKL object")
        offset = struct.unpack_from("<Q", header, 40)[0]
        stride, count = struct.unpack_from("<HH", header, 58)
        if stride != 64 or count == 0:
            raise Invalid("unsupported ELF section table")
        raw = span(self.data, offset, stride * count)
        self.sections = []
        for record in struct.iter_unpack("<IIQQQQIIQQ", raw):
            _, kind, _, _, at, size, link, target, _, stride = record
            self.sections.append(Section(kind, at, size, link, target, stride))

    def section_data(self, index):
        if not 0 <= index < len(self.sections):
            raise Invalid("invalid ELF section index")
        s = self.sections[index]
        return span(self.data, s.offset, s.size)

    def symbols(self, index):
        if not 0 <= index < len(self.sections):
            raise Invalid("invalid symbol table index")
        section = self.sections[index]
        if section.kind != 2 or section.stride != 24 or section.size % 24:
            raise Invalid("invalid ELF64 symbol table")
        names = self.section_data(section.link)
        result = []
        for offset, info, _, target, value, size in struct.iter_unpack("<IBBHQQ", self.section_data(index)):
            if offset >= len(names):
                raise Invalid("invalid symbol name offset")
            end = names.find(b"\0", offset)
            if end < 0:
                raise Invalid("unterminated symbol name")
            result.append(Symbol(names[offset:end].decode("utf-8"), info & 15, target, value, size))
        return result

    def syscall_slots(self, numbers):
        tables = []
        for index, section in enumerate(self.sections):
            if section.kind == 2:
                symbols = self.symbols(index)
                for table in symbols:
                    if table.name == "syscall_table" and table.kind == 1:
                        tables.append((index, symbols, table))
        if len(tables) != 1:
            raise Invalid(f"expected one syscall_table object symbol, found {len(tables)}")
        index, symbols, table = tables[0]
        if not 0 < table.section < len(self.sections) or table.size % 8:
            raise Invalid("invalid syscall table section/size")
        containing = self.sections[table.section]
        if table.value > containing.size or table.size > containing.size - table.value:
            raise Invalid("syscall table exceeds its section")
        if any(n < 0 or n >= table.size // 8 for n in numbers):
            raise Invalid("private syscall number exceeds LKL's syscall table")
        wanted = {table.value + nr * 8: nr for nr in numbers}
        result = {}
        for r_index, r in enumerate(self.sections):
            if r.kind != 4 or r.target != table.section:
                continue
            if r.link != index or r.stride != 24 or r.size % 24:
                raise Invalid("unsupported syscall-table relocations")
            for at, info, addend in struct.iter_unpack("<QQq", self.section_data(r_index)):
                if at not in wanted:
                    continue
                nr = wanted[at]
                if nr in result:
                    raise Invalid("duplicate relocation in syscall table")
                symbol, kind = info >> 32, info & 0xffffffff
                if kind != (1 if self.machine == 62 else 257) or symbol >= len(symbols):
                    raise Invalid("unsupported syscall pointer relocation")
                target = symbols[symbol]
                address = target.value + addend
                result[nr] = sorted({s.name for s in symbols if s.kind == 2 and
                                     s.section == target.section and s.section != 0 and s.value == address})
        return {nr: result.get(nr, []) for nr in numbers}


def verify(data):
    slots = Elf(data).syscall_slots(EXPECTED)
    errors = []
    for nr, name in EXPECTED.items():
        acceptable = {f"sys_{name}", f"__se_sys_{name}", f"__do_sys_{name}"}
        if not acceptable.intersection(slots[nr]):
            found = ("sys_ni_syscall (unimplemented)" if "sys_ni_syscall" in slots[nr] else
                     ", ".join(slots[nr][:6]) or "no resolved function")
            errors.append(f"{nr}: expected sys_{name}; found {found}")
    if errors:
        raise Invalid("private LKL syscall ABI mismatch:\n  " + "\n  ".join(errors))
    return slots


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifact", type=Path)
    args = parser.parse_args()
    try:
        slots = verify(args.artifact.read_bytes())
    except (OSError, Invalid, UnicodeError) as error:
        parser.exit(1, f"LKL_ABI_FAIL {error}\nRebuild with the current ldk/patch-lkl.py before dynamic userland.\n")
    for nr, names in slots.items():
        print(f"  {nr}: {', '.join(names)}")
    print("LKL_ABI_PASS")


if __name__ == "__main__":
    main()
