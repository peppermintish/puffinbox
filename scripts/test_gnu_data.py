# SPDX-License-Identifier: MIT OR Apache-2.0
import hashlib
import importlib.util
from io import BytesIO
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest

from check_gnu_data import StringReader, byte_size, covered_bytes, inherited, inventory, join_sources, string_fields


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
RUST_COMPILER = shutil.which("rustc")


def string_type():
    byte = Die(100, tag="DW_TAG_base_type", attrs={"DW_AT_byte_size": 1, "DW_AT_encoding": 7})
    pointer = Die(101, tag="DW_TAG_pointer_type", attrs={"DW_AT_byte_size": 8, "DW_AT_type": 100},
                  references={"DW_AT_type": byte})
    length = Die(102, tag="DW_TAG_base_type", attrs={"DW_AT_byte_size": 8, "DW_AT_encoding": 7})
    data = Die(103, tag="DW_TAG_member", attrs={"DW_AT_name": "data_ptr", "DW_AT_type": 101,
                                              "DW_AT_data_member_location": 0}, references={"DW_AT_type": pointer})
    size = Die(104, tag="DW_TAG_member", attrs={"DW_AT_name": "length", "DW_AT_type": 102,
                                              "DW_AT_data_member_location": 8}, references={"DW_AT_type": length})
    return Die(105, tag="DW_TAG_structure_type", attrs={"DW_AT_name": "&str", "DW_AT_byte_size": 16},
               children=[data, size])


class StringShapeTests(unittest.TestCase):
    def test_nested_layout_uses_declared_offsets(self):
        string = string_type()
        field = Die(106, tag="DW_TAG_member", attrs={"DW_AT_name": "label", "DW_AT_type": 105,
                                                   "DW_AT_data_member_location": 8},
                    references={"DW_AT_type": string})
        parent = Die(107, tag="DW_TAG_structure_type", attrs={"DW_AT_byte_size": 24}, children=[field])
        self.assertEqual(list(string_fields(parent)), [(8, ("label",))])
        field.attributes["DW_AT_data_member_location"].value = 9
        self.assertEqual(list(string_fields(parent)), [])
        field.attributes["DW_AT_data_member_location"].value = [0x23, 8]
        self.assertEqual(list(string_fields(parent)), [])

    def test_malformed_string_shapes_are_not_guessed(self):
        for change in ("offset", "signed", "width", "extra", "no_type", "no_encoding"):
            with self.subTest(change=change):
                string = string_type()
                data, size = string.children
                if change == "offset":
                    size.attributes["DW_AT_data_member_location"].value = 4
                elif change == "signed":
                    size.references["DW_AT_type"].attributes["DW_AT_encoding"].value = 5
                elif change == "width":
                    data.references["DW_AT_type"].attributes["DW_AT_byte_size"].value = 4
                elif change == "extra":
                    string.children.append(Die(108, tag="DW_TAG_member"))
                elif change == "no_type":
                    del data.attributes["DW_AT_type"]
                else:
                    del size.references["DW_AT_type"].attributes["DW_AT_encoding"]
                self.assertEqual(list(string_fields(string)), [])

    def test_cycles_arrays_and_pointer_graphs_are_not_traversed(self):
        alias = Die(108, tag="DW_TAG_typedef", attrs={"DW_AT_type": 108})
        alias.references["DW_AT_type"] = alias
        self.assertEqual(list(string_fields(alias)), [])
        string = string_type()
        for tag in ("DW_TAG_array_type", "DW_TAG_pointer_type", "DW_TAG_union_type"):
            container = Die(109, tag=tag, attrs={"DW_AT_type": 105}, references={"DW_AT_type": string})
            self.assertEqual(list(string_fields(container)), [])
        member = Die(110, tag="DW_TAG_member", attrs={"DW_AT_type": 111, "DW_AT_data_member_location": 0})
        parent = Die(111, tag="DW_TAG_structure_type", attrs={"DW_AT_byte_size": 16}, children=[member])
        member.references["DW_AT_type"] = parent
        self.assertEqual(list(string_fields(parent)), [])


class Section(dict):
    def __init__(self, name, **values):
        super().__init__(values)
        self.name = name


class FakeElf(dict):
    elfclass, little_endian = 64, True

    def __init__(self, relocations=(), kind="ET_DYN", relocation_type="SHT_RELA"):
        super().__init__(e_type=kind, e_machine="EM_X86_64")
        self.section = Section(".rela.dyn", sh_flags=2, sh_type=relocation_type)
        self.section.iter_relocations = lambda: iter(relocations)

    def iter_sections(self):
        return iter([self.section])


def relocation(address=0x1000, kind=8, symbol=0, addend=0x2000):
    return {"r_offset": address, "r_info_type": kind, "r_info_sym": symbol, "r_addend": addend}


class StringPointerTests(unittest.TestCase):
    def reader(self, length=5, pointer=1, payload=b"hello", relocations=None, kind="ET_DYN", relocation_type="SHT_RELA"):
        raw = pointer.to_bytes(8, "little") + length.to_bytes(8, "little") + bytes(48) + payload
        sections = [(0x1000, 0x1010, Section(".data.rel.ro", sh_offset=0), True),
                    (0x2000, 0x2000 + len(payload), Section(".rodata", sh_offset=64), False)]
        return StringReader(FakeElf([relocation()] if relocations is None else relocations, kind,
                                    relocation_type), BytesIO(raw), sections)

    def test_relative_pointer_uses_addend_and_hashes_the_payload(self):
        row, span = self.reader().inspect(0x1000, 16, 0)
        self.assertEqual(span, (0x2000, 0x2005))
        self.assertEqual(row["storedBytesSha256"], hashlib.sha256(b"hello").hexdigest())
        self.assertEqual(row["binding"], "R_X86_64_RELATIVE")

    def test_empty_string_does_not_require_a_relocation_or_dereference(self):
        row, span = self.reader(length=0, relocations=[]).inspect(0x1000, 16, 0)
        self.assertIsNone(span)
        self.assertEqual(row["binding"], "empty-no-dereference")
        self.assertNotIn("dataAddress", row)

    def test_only_fixed_executable_pointers_can_omit_relocations(self):
        with self.assertRaisesRegex(ValueError, "PIE"):
            self.reader(pointer=0x2000, relocations=[]).inspect(0x1000, 16, 0)
        row, _ = self.reader(pointer=0x2000, relocations=[], kind="ET_EXEC").inspect(0x1000, 16, 0)
        self.assertEqual(row["binding"], "absolute")

    def test_external_duplicate_and_overlapping_relocations_are_rejected(self):
        cases = [[relocation(kind=1, symbol=2)], [relocation(symbol=2)],
                 [relocation(), relocation()], [relocation(address=0x1001)],
                 [relocation(address=0x1008)], [relocation(address=0x2000)], [relocation(kind=18)]]
        for entries in cases:
            with self.subTest(entries=entries), self.assertRaisesRegex(ValueError, "relocation"):
                self.reader(relocations=entries).inspect(0x1000, 16, 0)

    def test_unrelated_tls_word_is_indexed_without_becoming_a_string_pointer(self):
        reader = self.reader(relocations=[relocation(), relocation(address=0x3000, kind=18)])
        row, _ = reader.inspect(0x1000, 16, 0)
        self.assertEqual(row["binding"], "R_X86_64_RELATIVE")
        self.assertEqual(reader.overlapping(0x3007, 1), [0x3000])

    def test_unmodeled_relocation_widths_and_packed_relocations_are_rejected(self):
        for reader in (self.reader(relocations=[relocation(kind=5)]), self.reader(relocation_type="SHT_RELR")):
            with self.assertRaisesRegex(ValueError, "explicit-addend"):
                reader.inspect(0x1000, 16, 0)

    def test_other_elf_machines_are_not_interpreted_as_x86_64(self):
        elf = FakeElf([relocation()])
        elf["e_machine"] = "EM_AARCH64"
        template = self.reader()
        reader = StringReader(elf, template.stream, template.sections)
        with self.assertRaisesRegex(ValueError, "x86-64"):
            reader.inspect(0x1000, 16, 0)

    def test_payload_bounds_utf8_truncation_and_field_bounds(self):
        cases = [(self.reader(length=6), 16, 0, "read-only"),
                 (self.reader(length=1024 * 1024 + 1), 16, 0, "one-MiB"),
                 (self.reader(payload=b"\xffello"), 16, 0, "UTF-8"),
                 (self.reader(relocations=[relocation(addend=0x3000)]), 16, 0, "read-only"),
                 (self.reader(), 15, 0, "root variable"), (self.reader(), 16, -1, "root variable")]
        for reader, size, offset, reason in cases:
            with self.subTest(reason=reason), self.assertRaisesRegex(ValueError, reason):
                reader.inspect(0x1000, size, offset)
        reader = self.reader()
        reader.stream = BytesIO(bytes(8))
        with self.assertRaisesRegex(ValueError, "Truncated"):
            reader.inspect(0x1000, 16, 0)


@unittest.skipUnless(HAS_INSPECTOR and RUST_COMPILER, "Requires external pyelftools and Rust.")
class CompiledStringControls(unittest.TestCase):
    def test_rust_pie_nested_alias_and_empty_strings_use_real_dwarf_and_relocations(self):
        from elftools.elf.elffile import ELFFile

        with tempfile.TemporaryDirectory(prefix="puffinbox-string-control-") as directory:
            root = Path(directory)
            source, binary = root / "original.rs", root / "control"
            source.write_text('''
struct Nested { marker: u64, label: &'static str }
#[used] static ORIGINAL_STRING: &str = "original string control";
#[used] static ORIGINAL_ALIAS: &str = "original string control";
#[used] static ORIGINAL_EMPTY: &str = "";
#[used] static ORIGINAL_NESTED: Nested = Nested { marker: 17, label: "nested control" };
#[used] static ORIGINAL_ARRAY: [&str; 1] = ["array outside scope"];
fn main() {
    std::hint::black_box((&ORIGINAL_STRING, &ORIGINAL_ALIAS, &ORIGINAL_EMPTY,
                         ORIGINAL_NESTED.marker, &ORIGINAL_NESTED.label, &ORIGINAL_ARRAY));
}
''')
            subprocess.run([RUST_COMPILER, "-C", "debuginfo=2", "-C", "opt-level=0", "-C", "relocation-model=pic",
                            "-C", "link-arg=-pie", "-C", "link-arg=-Wl,-z,relro", str(source), "-o", str(binary)],
                           check=True, capture_output=True)
            with binary.open("rb") as stream:
                elf = ELFFile(stream)
                self.assertEqual(elf["e_type"], "ET_DYN")
                result = inventory(elf, stream)
        rows = {row["rootName"]: row for row in result["stringReferences"] if row["rootName"].startswith("ORIGINAL_")}
        self.assertEqual(set(rows), {"ORIGINAL_STRING", "ORIGINAL_ALIAS", "ORIGINAL_EMPTY", "ORIGINAL_NESTED"})
        self.assertEqual(rows["ORIGINAL_STRING"]["binding"], "R_X86_64_RELATIVE")
        self.assertEqual(rows["ORIGINAL_STRING"]["dataAddress"], rows["ORIGINAL_ALIAS"]["dataAddress"])
        self.assertEqual(rows["ORIGINAL_EMPTY"]["length"], 0)
        self.assertEqual(rows["ORIGINAL_NESTED"]["memberPath"], ("label",))
        self.assertEqual(rows["ORIGINAL_NESTED"]["storedBytesSha256"], hashlib.sha256(b"nested control").hexdigest())
        payload_spans = [(int(row["dataAddress"], 16), int(row["dataAddress"], 16) + row["length"])
                         for row in rows.values() if row["length"]]
        self.assertEqual(covered_bytes(payload_spans), len(b"original string controlnested control"))
        for section in result["sectionCoverage"]:
            self.assertLessEqual(section["variableAndStringBytes"], section["variableBytes"] + section["stringPayloadBytes"])
            self.assertEqual(section["remainingAfterStringInspection"], section["bytes"] - section["variableAndStringBytes"])
        self.assertFalse(result["licenseClearance"])


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
        if not HAS_INSPECTOR or not COMPILER or not RUST_COMPILER:
            raise SystemExit("Compiled controls require external pyelftools, C and Rust compilers.")
    unittest.main()
