# SPDX-License-Identifier: MIT OR Apache-2.0
import importlib.util
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest

from check_gnu_data import read_only_sections
from check_gnu_data_map import inventory


HEADER = "             VMA              LMA     Size Align Out     In      Symbol\n"


def row(address, size, tail, align=1):
    return f"{address:16x} {address:16x} {size:8x} {align:5d} {tail}\n"


class MapDataTests(unittest.TestCase):
    def sections(self):
        return [(0x1000, 0x1020, SimpleNamespace(name=".rodata"), False)]

    def test_named_internal_alias_and_zero_rows_use_range_unions(self):
        source = HEADER + row(0x1000, 32, ".rodata")
        source += row(0x1000, 8, "        original.o:(.rodata.first)")
        source += row(0x1004, 8, "        alias.o:(.rodata.alias)")
        source += row(0x100C, 4, "        <internal>:(.rodata.cst4)")
        source += row(0x1010, 0, "        empty.o:(.rodata.empty)")
        result = inventory(source, self.sections())
        coverage = result["sectionCoverage"][0]
        self.assertEqual(len(result["rows"]), 3)
        self.assertEqual(coverage["namedInputRangeUnionBytes"], 12)
        self.assertEqual(coverage["internalInputRangeUnionBytes"], 4)
        self.assertEqual(coverage["mappedInputRangeUnionBytes"], 16)
        self.assertEqual(coverage["outsideParsedInputRows"], 16)
        self.assertFalse(result["licenseClearance"])

    def test_debug_and_executable_offsets_cannot_be_loaded_data_associations(self):
        source = HEADER + row(0x1000, 32, ".debug_info")
        source += row(0x1000, 32, "        debug.o:(.debug_info)")
        source += row(0x1000, 32, ".text") + row(0x1000, 32, "        code.o:(.text)")
        source += row(0x1000, 32, ".rodata") + row(0x1000, 8, "        original.o:(.rodata)")
        result = inventory(source, self.sections())
        self.assertEqual([entry["input"] for entry in result["rows"]], ["original.o"])
        self.assertEqual(result["sectionCoverage"][0]["outsideParsedInputRows"], 24)

    def test_indented_local_symbols_do_not_replace_the_output_section(self):
        source = HEADER + row(0x1000, 32, ".rodata")
        source += row(0x1000, 8, "        original.o:(.rodata.first)")
        source += row(0x1000, 8, "                .local_constant")
        source += row(0x1008, 8, "        original.o:(.rodata.last)")
        self.assertEqual(inventory(source, self.sections())["sectionCoverage"][0]["mappedInputRangeUnionBytes"], 16)

    def test_missing_duplicate_or_inconsistent_headers_are_rejected(self):
        for source in ["unsupported\n", HEADER, HEADER + row(0x1000, 32, ".rodata"),
                       HEADER + row(0x1000, 31, ".rodata"),
                       HEADER + row(0x1000, 32, ".rodata") * 2]:
            with self.subTest(source=source), self.assertRaises(ValueError):
                inventory(source, self.sections())
        with self.assertRaises(ValueError):
            inventory(HEADER, [])
        with self.assertRaises(ValueError):
            inventory(HEADER, self.sections() * 2)

    def test_input_ranges_cannot_escape_their_elf_section(self):
        for address, size in [(0x0FFF, 8), (0x101F, 2), (0x1020, 1), (0x1000, 33)]:
            with self.subTest(address=address, size=size), self.assertRaisesRegex(ValueError, "escapes"):
                inventory(HEADER + row(0x1000, 32, ".rodata") + row(address, size, "        original.o:(.rodata)"), self.sections())

    def test_read_only_after_relocation_keeps_its_separate_section_identity(self):
        sections = self.sections() + [(0x2000, 0x2010, SimpleNamespace(name=".data.rel.ro"), True)]
        source = HEADER + row(0x1000, 32, ".rodata") + row(0x1000, 8, "        original.o:(.rodata)")
        source += row(0x2000, 16, ".data.rel.ro") + row(0x2000, 16, "        original.o:(.data.rel.ro)")
        result = inventory(source, sections)
        self.assertEqual(len(result["sectionCoverage"]), 2)
        self.assertFalse(result["sectionCoverage"][0]["readOnlyAfterRelocation"])
        self.assertTrue(result["sectionCoverage"][1]["readOnlyAfterRelocation"])


COMPILER, RUST_COMPILER = shutil.which("cc") or shutil.which("gcc"), shutil.which("rustc")
HAS_INSPECTOR = importlib.util.find_spec("elftools") is not None


@unittest.skipUnless(COMPILER and RUST_COMPILER and HAS_INSPECTOR, "Requires C, Rust's LLD and external pyelftools.")
class CompiledMapDataControls(unittest.TestCase):
    def test_real_lld_map_joins_loaded_elf_data_and_excludes_debug_sections(self):
        from elftools.elf.elffile import ELFFile

        sysroot = Path(subprocess.check_output([RUST_COMPILER, "--print", "sysroot"], text=True).strip())
        linker_dir = sysroot / "lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld"
        self.assertTrue((linker_dir / "ld.lld").is_file(), "Required Rust LLD is absent.")
        with tempfile.TemporaryDirectory(prefix="puffinbox-data-map-control-") as directory:
            root = Path(directory)
            source, binary, map_path = root / "original.c", root / "control", root / "control.map"
            source.write_text('''
const unsigned char original_bytes[8] = {3, 17, 29, 43, 61, 79, 97, 113};
const unsigned char * const original_pointer = original_bytes;
volatile const void *address_sink;
int main(void) { address_sink = original_bytes; address_sink = &original_pointer; return 0; }
''')
            subprocess.run([COMPILER, "-B" + str(linker_dir), "-fuse-ld=lld", "-g", "-O0", "-fPIE", "-pie",
                            "-Wl,-z,relro", "-Wl,-Map," + str(map_path), str(source), "-o", str(binary)],
                           check=True, capture_output=True)
            with binary.open("rb") as stream:
                sections = read_only_sections(ELFFile(stream))
                result = inventory(map_path.read_text(), sections)
            self.assertTrue(result["rows"])
            self.assertFalse(any(".debug" in entry["inputSection"] for entry in result["rows"]))
            coverage = {entry["section"]: entry for entry in result["sectionCoverage"]}
            self.assertGreater(coverage[".rodata"]["namedInputRangeUnionBytes"], 0)
            self.assertGreaterEqual(coverage[".data.rel.ro"]["namedInputRangeUnionBytes"], 8)
            self.assertEqual(set(coverage), {section.name for _, _, section, _ in sections})
            self.assertFalse(result["licenseClearance"])


if __name__ == "__main__":
    if "--require-controls" in sys.argv:
        if not (COMPILER and RUST_COMPILER and HAS_INSPECTOR):
            raise SystemExit("C, Rust and external pyelftools are required.")
        sys.argv.remove("--require-controls")
    unittest.main()
