# SPDX-License-Identifier: MIT OR Apache-2.0
import hashlib
import importlib.util
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest

from check_gnu_data import byte_size, covered_bytes, inherited, inventory, join_sources


class Die:
    def __init__(self, offset, tag="DW_TAG_variable", attrs=None, references=None, children=()):
        self.offset, self.tag = offset, tag
        self.attributes = {key: SimpleNamespace(value=value) for key, value in (attrs or {}).items()}
        self.references, self.children = references or {}, children
        self.cu = SimpleNamespace(header=SimpleNamespace(address_size=8))

    def get_DIE_from_attribute(self, key):
        return self.references[key]

    def iter_children(self):
        return iter(self.children)


class DataAttributionTests(unittest.TestCase):
    def test_overlapping_aliases_do_not_inflate_byte_coverage(self):
        self.assertEqual(covered_bytes([(16, 24), (18, 26), (16, 24), (40, 40), (26, 30)]), 14)
        self.assertEqual(covered_bytes([]), 0)
        with self.assertRaises(ValueError):
            covered_bytes([(8, 7)])

    def test_declaration_chains_resolve_but_cycles_do_not(self):
        declared = Die(3, attrs={"DW_AT_decl_file": 0})
        abstract = Die(2, attrs={"DW_AT_specification": 3}, references={"DW_AT_specification": declared})
        local = Die(1, attrs={"DW_AT_abstract_origin": 2}, references={"DW_AT_abstract_origin": abstract})
        self.assertIs(inherited(local, "DW_AT_decl_file"), declared)
        declared.attributes = {"DW_AT_specification": SimpleNamespace(value=1)}
        declared.references = {"DW_AT_specification": local}
        self.assertIsNone(inherited(local, "DW_AT_decl_file"))

    def test_zero_sizes_and_pointer_storage_are_not_unknown(self):
        self.assertEqual(byte_size(Die(1, attrs={"DW_AT_byte_size": 0})), 0)
        self.assertEqual(byte_size(Die(2, tag="DW_TAG_pointer_type")), 8)
        self.assertIsNone(byte_size(Die(3, attrs={"DW_AT_byte_size": -1})))

    def test_array_dimensions_use_counts_and_lower_bounds(self):
        scalar = Die(1, attrs={"DW_AT_byte_size": 2})
        outer = Die(2, tag="DW_TAG_array_type", attrs={"DW_AT_type": 1},
                    references={"DW_AT_type": scalar}, children=[
                        Die(3, tag="DW_TAG_subrange_type", attrs={"DW_AT_count": 4}),
                        Die(4, tag="DW_TAG_subrange_type", attrs={"DW_AT_lower_bound": 2, "DW_AT_upper_bound": 4}),
                    ])
        self.assertEqual(byte_size(outer), 24)
        outer.children = [Die(5, tag="DW_TAG_subrange_type", attrs={"DW_AT_count": [0x50]})]
        self.assertIsNone(byte_size(outer))
        outer.children = []
        self.assertIsNone(byte_size(outer))

    def test_recursive_types_and_dynamic_bounds_remain_unbounded(self):
        recursive = Die(1, attrs={"DW_AT_type": 1})
        recursive.references["DW_AT_type"] = recursive
        self.assertIsNone(byte_size(recursive))
        scalar = Die(2, attrs={"DW_AT_byte_size": 1})
        array = Die(3, tag="DW_TAG_array_type", attrs={"DW_AT_type": 2},
                    references={"DW_AT_type": scalar}, children=[
                        Die(4, tag="DW_TAG_subrange_type", attrs={"DW_AT_upper_bound": [0x50]})])
        self.assertIsNone(byte_size(array))

    def test_language_dependent_array_defaults_are_not_guessed(self):
        scalar = Die(1, attrs={"DW_AT_byte_size": 1})
        array = Die(2, tag="DW_TAG_array_type", attrs={"DW_AT_type": 1},
                    references={"DW_AT_type": scalar}, children=[
                        Die(3, tag="DW_TAG_subrange_type", attrs={"DW_AT_upper_bound": 7})])
        language = SimpleNamespace(value=0x1C)
        array.cu.get_top_DIE = lambda: SimpleNamespace(attributes={"DW_AT_language": language})
        self.assertEqual(byte_size(array), 8)
        language.value = 0x08
        self.assertIsNone(byte_size(array))

    def test_standard_variable_sources_require_the_exact_build_snapshot(self):
        path = "/toolchain/lib/rustlib/src/rust/library/core/src/unicode/unicode_data.rs"
        result = {"sourceFiles": {path: {"variables": 1}}}
        with self.assertRaisesRegex(ValueError, "exact standard-library build hash"):
            join_sources(result, {}, {})


HAS_INSPECTOR = importlib.util.find_spec("elftools") is not None
COMPILER = shutil.which("cc")


@unittest.skipUnless(HAS_INSPECTOR and COMPILER, "Requires external pyelftools and a C compiler.")
class CompiledDataControls(unittest.TestCase):
    def inspect(self, source):
        from elftools.elf.elffile import ELFFile

        with tempfile.TemporaryDirectory(prefix="puffinbox-data-control-") as directory:
            root = Path(directory)
            path = root / "original.c"
            path.write_text(source)
            binary = root / "control"
            subprocess.run([COMPILER, "-gdwarf-5", "-g", "-O0", "-fPIE", "-pie", "-Wl,-z,relro",
                            str(path), "-o", str(binary)], check=True, capture_output=True)
            with binary.open("rb") as stream:
                elf = ELFFile(stream)
                result = inventory(elf, stream)
                result["controlSymbols"] = {symbol.name: symbol["st_value"]
                                            for symbol in elf.get_section_by_name(".symtab").iter_symbols()
                                            if symbol.name.startswith("original_")}
            return result

    def test_read_only_and_relro_data_are_hashed_but_writable_storage_is_excluded(self):
        result = self.inspect('''
const unsigned char original_data[8] = {3, 17, 29, 43, 61, 79, 97, 113};
extern const unsigned char original_alias[8] __attribute__((alias("original_data")));
const unsigned char * const original_pointer = original_data;
unsigned char writable_data[8] = {1};
unsigned char zero_data[8];
volatile const void *address_sink;
int main(void) {
    address_sink = original_data; address_sink = original_alias;
    address_sink = &original_pointer; address_sink = writable_data; address_sink = zero_data;
    return 0;
}
''')
        rows = {row["name"]: row for row in result["rows"]}
        self.assertEqual(rows["original_data"]["storedBytesSha256"],
                         hashlib.sha256(bytes([3, 17, 29, 43, 61, 79, 97, 113])).hexdigest())
        self.assertEqual(rows["original_data"]["byteSize"], 8)
        self.assertEqual(result["controlSymbols"]["original_alias"], result["controlSymbols"]["original_data"])
        self.assertTrue(rows["original_pointer"]["readOnlyAfterRelocation"])
        self.assertNotIn("writable_data", rows)
        self.assertNotIn("zero_data", rows)
        section = next(row for row in result["sectionCoverage"] if row["section"] == rows["original_data"]["section"])
        self.assertEqual(section["variableBytes"], 8)
        self.assertGreater(section["remainingBytes"], 0)
        self.assertFalse(result["licenseClearance"])

    def test_a_known_exception_is_visible_without_any_executable_line_interval(self):
        path = "/toolchain/lib/rustlib/src/rust/library/core/src/unicode/unicode_data.rs"
        result = self.inspect(f'''
#line 1 "{path}"
const unsigned char original_control[4] = {{2, 3, 5, 7}};
#line 1 "original_main.c"
volatile const void *address_sink;
int main(void) {{ address_sink = original_control; return 0; }}
''')
        self.assertIn(path, result["knownNonAllowlistedSourceFiles"])
        self.assertEqual(result["sourceFiles"][path]["variables"], 1)
        self.assertFalse(result["licenseClearance"])

    def test_missing_dwarf_fails_instead_of_claiming_no_retained_data(self):
        from elftools.elf.elffile import ELFFile

        with tempfile.TemporaryDirectory(prefix="puffinbox-data-control-") as directory:
            binary = Path(directory) / "control"
            subprocess.run([COMPILER, "-x", "c", "-", "-o", str(binary)], input=b"int main(void) {return 0;}\n",
                           check=True, capture_output=True)
            with binary.open("rb") as stream, self.assertRaisesRegex(ValueError, "no DWARF"):
                inventory(ELFFile(stream), stream)


if __name__ == "__main__":
    if "--require-controls" in sys.argv:
        sys.argv.remove("--require-controls")
        if not HAS_INSPECTOR or not COMPILER:
            raise SystemExit("Compiled controls require external pyelftools and a C compiler.")
    unittest.main()
