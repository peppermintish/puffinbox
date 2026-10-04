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
        self.attributes = {key: SimpleNamespace(value=value, form="DW_FORM_data8")
                           for key, value in (attrs or {}).items()}
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

    def test_integer_references_are_not_literal_sizes_or_bounds(self):
        scalar = Die(1, attrs={"DW_AT_byte_size": 16})
        scalar.attributes["DW_AT_byte_size"].form = "DW_FORM_ref4"
        self.assertIsNone(byte_size(scalar))
        scalar.attributes["DW_AT_byte_size"].form = "DW_FORM_data8"
        dimension = Die(2, tag="DW_TAG_subrange_type", attrs={"DW_AT_count": 3})
        dimension.attributes["DW_AT_count"].form = "DW_FORM_ref4"
        array = Die(3, tag="DW_TAG_array_type", attrs={"DW_AT_type": 1},
                    references={"DW_AT_type": scalar}, children=[dimension])
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


def slice_type():
    element = string_type()
    pointer = Die(301, tag="DW_TAG_pointer_type", attrs={"DW_AT_byte_size": 8, "DW_AT_type": element.offset},
                  references={"DW_AT_type": element})
    length = Die(302, tag="DW_TAG_base_type", attrs={"DW_AT_byte_size": 8, "DW_AT_encoding": 7})
    data = Die(303, tag="DW_TAG_member", attrs={"DW_AT_name": "data_ptr", "DW_AT_type": pointer.offset,
                                              "DW_AT_data_member_location": 0}, references={"DW_AT_type": pointer})
    size = Die(304, tag="DW_TAG_member", attrs={"DW_AT_name": "length", "DW_AT_type": length.offset,
                                              "DW_AT_data_member_location": 8}, references={"DW_AT_type": length})
    return Die(305, tag="DW_TAG_structure_type", attrs={"DW_AT_name": "&[&str]", "DW_AT_byte_size": 16},
               children=[data, size])


def optional_type():
    scalar = Die(401, tag="DW_TAG_base_type", attrs={"DW_AT_byte_size": 8, "DW_AT_encoding": 7})
    discriminant = Die(402, tag="DW_TAG_member", attrs={"DW_AT_type": scalar.offset, "DW_AT_data_member_location": 0},
                       references={"DW_AT_type": scalar})
    empty = Die(403, tag="DW_TAG_structure_type", attrs={"DW_AT_name": "None", "DW_AT_byte_size": 16})
    element = string_type()
    payload = Die(404, tag="DW_TAG_member", attrs={"DW_AT_name": "__0", "DW_AT_type": element.offset,
                                                "DW_AT_data_member_location": 0}, references={"DW_AT_type": element})
    some = Die(405, tag="DW_TAG_structure_type", attrs={"DW_AT_name": "Some", "DW_AT_byte_size": 16}, children=[payload])
    variants = []
    for index, shape in enumerate((empty, some)):
        member = Die(406 + index, tag="DW_TAG_member", attrs={"DW_AT_name": shape.attributes["DW_AT_name"].value,
                                                            "DW_AT_type": shape.offset, "DW_AT_data_member_location": 0},
                     references={"DW_AT_type": shape})
        variants.append(Die(408 + index, tag="DW_TAG_variant", attrs={"DW_AT_discr_value": 0} if index == 0 else {},
                            children=[member]))
    part = Die(410, tag="DW_TAG_variant_part", attrs={"DW_AT_discr": discriminant.offset},
               references={"DW_AT_discr": discriminant}, children=[discriminant, *variants])
    part.attributes["DW_AT_discr"].form = "DW_FORM_ref4"
    return Die(411, tag="DW_TAG_structure_type", attrs={"DW_AT_name": "Option<&str>", "DW_AT_byte_size": 16},
               children=[part, empty, some])


class OptionalShapeTests(unittest.TestCase):
    def test_only_the_verified_niche_layout_is_selected(self):
        shape = optional_type()
        self.assertEqual(list(string_fields(shape, optional=True)), [(0, ())])
        self.assertEqual(list(string_fields(shape)), [])
        shape.children[1].children = [Die(412, tag="DW_TAG_template_type_param")]
        shape.children[2].children.insert(0, Die(413, tag="DW_TAG_template_type_param"))
        shape.children.append(Die(414, tag="DW_TAG_subprogram"))
        self.assertEqual(list(string_fields(shape, optional=True)), [(0, ())])

    def test_nested_offsets_and_variant_paths_keep_the_existing_depth_bound(self):
        shape = optional_type()
        member = Die(412, tag="DW_TAG_member", attrs={"DW_AT_name": "label", "DW_AT_type": shape.offset,
                                                     "DW_AT_data_member_location": 8}, references={"DW_AT_type": shape})
        parent = Die(413, tag="DW_TAG_structure_type", attrs={"DW_AT_byte_size": 24}, children=[member])
        self.assertEqual(list(string_fields(parent, optional=True)), [(8, ("label",))])
        self.assertEqual(list(string_fields(shape, path=("level",) * 6, optional=True)), [(0, ("level",) * 6)])
        self.assertEqual(list(string_fields(shape, path=("level",) * 7, optional=True)), [])

    def test_malformed_selectors_and_variant_fields_are_not_guessed(self):
        changes = ("size", "name", "no_selector", "selector_form", "other_member", "signed", "offset", "bit_field",
                   "discriminant_list", "nonzero_none", "two_defaults", "two_none", "none_payload", "some_offset",
                   "some_payload", "some_bit_field", "some_cycle", "extra_field", "missing_variant")
        for change in changes:
            with self.subTest(change=change):
                shape = optional_type()
                part, empty, some = shape.children
                discriminant, absent, present = part.children
                if change == "size":
                    shape.attributes["DW_AT_byte_size"].value = 24
                elif change == "name":
                    shape.attributes["DW_AT_name"].value = "Option<&[u8]>"
                elif change == "no_selector":
                    del part.attributes["DW_AT_discr"]
                elif change == "selector_form":
                    part.attributes["DW_AT_discr"].form = "DW_FORM_data8"
                elif change == "other_member":
                    part.references["DW_AT_discr"] = Die(414, tag="DW_TAG_member")
                elif change == "signed":
                    discriminant.references["DW_AT_type"].attributes["DW_AT_encoding"].value = 5
                elif change == "offset":
                    discriminant.attributes["DW_AT_data_member_location"].value = 8
                elif change == "bit_field":
                    discriminant.attributes["DW_AT_bit_size"] = SimpleNamespace(value=64, form="DW_FORM_data1")
                elif change == "discriminant_list":
                    present.attributes["DW_AT_discr_list"] = SimpleNamespace(value=[], form="DW_FORM_block")
                elif change == "nonzero_none":
                    absent.attributes["DW_AT_discr_value"].value = 1
                elif change == "two_defaults":
                    del absent.attributes["DW_AT_discr_value"]
                elif change == "two_none":
                    present.attributes["DW_AT_discr_value"] = SimpleNamespace(value=0, form="DW_FORM_data1")
                elif change == "none_payload":
                    empty.children = [some.children[0]]
                elif change == "some_offset":
                    some.children[0].attributes["DW_AT_data_member_location"].value = 8
                elif change == "some_payload":
                    some.children[0].references["DW_AT_type"].attributes["DW_AT_name"].value = "&[u8]"
                elif change == "some_bit_field":
                    some.children[0].attributes["DW_AT_data_bit_offset"] = SimpleNamespace(value=0, form="DW_FORM_data1")
                elif change == "some_cycle":
                    alias = Die(414, tag="DW_TAG_typedef", attrs={"DW_AT_type": 414})
                    alias.references["DW_AT_type"] = alias
                    some.children[0].references["DW_AT_type"] = alias
                elif change == "extra_field":
                    shape.children.append(discriminant)
                else:
                    part.children.pop()
                self.assertEqual(list(string_fields(shape, optional=True)), [])


class SliceShapeTests(unittest.TestCase):
    def test_only_the_verified_string_slice_layout_is_selected(self):
        shape = slice_type()
        self.assertEqual(list(string_fields(shape, slices=True)), [(0, ())])
        self.assertEqual(list(string_fields(shape)), [])
        member = Die(306, tag="DW_TAG_member", attrs={"DW_AT_name": "names", "DW_AT_type": shape.offset,
                                                     "DW_AT_data_member_location": 8},
                     references={"DW_AT_type": shape})
        parent = Die(307, tag="DW_TAG_structure_type", attrs={"DW_AT_byte_size": 24}, children=[member])
        self.assertEqual(list(string_fields(parent, slices=True)), [(8, ("names",))])

    def test_names_cannot_substitute_for_a_valid_pointee_and_header(self):
        for change in ("byte_element", "signed_count", "offset", "bit_field", "cycle", "reference_offset"):
            with self.subTest(change=change):
                shape = slice_type()
                data, size = shape.children
                pointer = data.references["DW_AT_type"]
                if change == "byte_element":
                    pointer.references["DW_AT_type"] = Die(308, tag="DW_TAG_base_type",
                                                           attrs={"DW_AT_byte_size": 1, "DW_AT_encoding": 7})
                elif change == "signed_count":
                    size.references["DW_AT_type"].attributes["DW_AT_encoding"].value = 5
                elif change == "offset":
                    size.attributes["DW_AT_data_member_location"].value = 4
                elif change == "bit_field":
                    size.attributes["DW_AT_bit_size"] = SimpleNamespace(value=64, form="DW_FORM_data1")
                elif change == "cycle":
                    alias = Die(308, tag="DW_TAG_typedef", attrs={"DW_AT_type": 308})
                    alias.references["DW_AT_type"] = alias
                    pointer.references["DW_AT_type"] = alias
                else:
                    size.attributes["DW_AT_data_member_location"].form = "DW_FORM_ref4"
                self.assertEqual(list(string_fields(shape, slices=True)), [])


class StringShapeTests(unittest.TestCase):
    def array(self, element, counts, *, size=None):
        attributes = {"DW_AT_type": element.offset}
        if size is not None:
            attributes["DW_AT_byte_size"] = size
        return Die(200, tag="DW_TAG_array_type", attrs=attributes,
                   references={"DW_AT_type": element}, children=[
                       Die(201 + index, tag="DW_TAG_subrange_type", attrs={"DW_AT_count": count})
                       for index, count in enumerate(counts)])

    def test_dense_arrays_include_every_element_and_dimension(self):
        array = self.array(string_type(), [2, 3])
        self.assertEqual(list(string_fields(array)), [
            (0, ("[0][0]",)), (16, ("[0][1]",)), (32, ("[0][2]",)),
            (48, ("[1][0]",)), (64, ("[1][1]",)), (80, ("[1][2]",)),
        ])

    def test_array_elements_preserve_nested_member_offsets(self):
        string = string_type()
        member = Die(106, tag="DW_TAG_member", attrs={"DW_AT_name": "label", "DW_AT_type": 105,
                                                     "DW_AT_data_member_location": 8},
                     references={"DW_AT_type": string})
        parent = Die(107, tag="DW_TAG_structure_type", attrs={"DW_AT_byte_size": 24}, children=[member])
        self.assertEqual(list(string_fields(self.array(parent, [2]))),
                         [(8, ("[0]", "label")), (32, ("[1]", "label"))])

    def test_unknown_padded_strided_and_excessive_arrays_are_not_guessed(self):
        for counts, size in [([], None), ([[0x50]], None), ([-1], None), ([2], 33), ([4097], None)]:
            with self.subTest(counts=counts, size=size):
                self.assertEqual(list(string_fields(self.array(string_type(), counts, size=size))), [])
        for attribute in ("DW_AT_byte_stride", "DW_AT_bit_stride", "DW_AT_ordering"):
            array = self.array(string_type(), [2])
            array.attributes[attribute] = SimpleNamespace(value=1)
            self.assertEqual(list(string_fields(array)), [])
        array = self.array(string_type(), [0])
        self.assertEqual(list(string_fields(array)), [])
        array = self.array(string_type(), [2], size=32)
        array.children[0].attributes["DW_AT_count"].form = "DW_FORM_ref4"
        self.assertEqual(list(string_fields(array)), [])
        array = self.array(string_type(), [2])
        array.children[0].attributes["DW_AT_byte_stride"] = SimpleNamespace(value=24, form="DW_FORM_data8")
        self.assertEqual(list(string_fields(array)), [])

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

    def test_cycles_unbounded_arrays_and_pointer_graphs_are_not_traversed(self):
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


class OptionalPointerTests(unittest.TestCase):
    def reader(self, pointer=0, length=0, relocations=()):
        header = pointer.to_bytes(8, "little") + length.to_bytes(8, "little")
        stream = BytesIO(header + bytes(16) + b"hello")
        sections = [(0x1000, 0x1010, Section(".data.rel.ro", sh_offset=0), True),
                    (0x3000, 0x3005, Section(".rodata", sh_offset=32), False)]
        return StringReader(FakeElf(list(relocations)), stream, sections)

    def test_none_never_reads_inactive_length_or_payload(self):
        for length, writes in [(0, []), (2**64 - 1, []), (2**64 - 1, [relocation(address=0x1008, kind=1, symbol=2)])]:
            with self.subTest(length=length, writes=writes):
                option, string, span = self.reader(length=length, relocations=writes).inspect_optional(0x1000, 16, 0)
                self.assertEqual(option["variant"], "None")
                self.assertIsNone(string)
                self.assertIsNone(span)

    def test_some_empty_is_distinct_from_none(self):
        option, string, span = self.reader(pointer=1).inspect_optional(0x1000, 16, 0)
        self.assertEqual(option["variant"], "Some")
        self.assertEqual(string["length"], 0)
        self.assertIsNone(span)
        self.assertNotIn("dataAddress", string)

    def test_relocated_pointer_is_selected_before_inspecting_its_stored_word(self):
        option, string, span = self.reader(length=5, relocations=[relocation(addend=0x3000)]).inspect_optional(0x1000, 16, 0)
        self.assertEqual(option["variant"], "Some")
        self.assertEqual(option["discriminantBinding"], "R_X86_64_RELATIVE")
        self.assertEqual(string["storedBytesSha256"], hashlib.sha256(b"hello").hexdigest())
        self.assertEqual(span, (0x3000, 0x3005))

    def test_nonlocal_overlapping_duplicate_and_ambiguous_selectors_fail_even_when_empty(self):
        cases = [[relocation(kind=1, symbol=2)], [relocation(address=0x0FFF)],
                 [relocation(), relocation()], [relocation(addend=0)], [relocation(addend=-1)]]
        for writes in cases:
            with self.subTest(writes=writes), self.assertRaisesRegex(ValueError, "discriminant"):
                self.reader(relocations=writes).inspect_optional(0x1000, 16, 0)

    def test_active_string_uses_existing_length_bounds_and_relocation_checks(self):
        for reader in (self.reader(pointer=1, length=5),
                       self.reader(length=5, relocations=[relocation(addend=0x3000), relocation(address=0x1008)])):
            with self.assertRaisesRegex(ValueError, "relocation"):
                reader.inspect_optional(0x1000, 16, 0)
        with self.assertRaisesRegex(ValueError, "bounded variable"):
            self.reader().inspect_optional(0x1000, 8, 0)


class SlicePointerTests(unittest.TestCase):
    def reader(self, count=2, payload=b"hello", relocations=None):
        header = (1).to_bytes(8, "little") + count.to_bytes(8, "little")
        elements = (1).to_bytes(8, "little") + (5).to_bytes(8, "little") + bytes(16)
        stream = BytesIO(header + bytes(48) + elements + bytes(32) + payload)
        sections = [(0x1000, 0x1010, Section(".data.rel.ro", sh_offset=0), True),
                    (0x2000, 0x2020, Section(".data.rel.ro.elements", sh_offset=64), True),
                    (0x3000, 0x3000 + len(payload), Section(".rodata", sh_offset=128), False)]
        entries = [relocation(addend=0x2000), relocation(address=0x2000, addend=0x3000)]
        return StringReader(FakeElf(entries if relocations is None else relocations), stream, sections)

    def test_element_count_reads_complete_descriptors_and_shared_string_checks(self):
        row, strings, storage = self.reader().inspect_slice(0x1000, 16, 0)
        self.assertEqual(row["elementCount"], 2)
        self.assertEqual(row["byteSize"], 32)
        self.assertEqual(storage, (0x2000, 0x2020))
        self.assertEqual(strings[0][1], (0x3000, 0x3005))
        self.assertEqual(strings[0][0]["storedBytesSha256"], hashlib.sha256(b"hello").hexdigest())
        self.assertEqual(strings[1][0]["length"], 0)
        self.assertIsNone(strings[1][1])

    def test_empty_slice_never_dereferences_its_pointer(self):
        row, strings, storage = self.reader(count=0, relocations=[]).inspect_slice(0x1000, 16, 0)
        self.assertEqual(row["elementCount"], 0)
        self.assertNotIn("dataAddress", row)
        self.assertEqual(strings, [])
        self.assertIsNone(storage)

    def test_excessive_counts_and_root_budget_fail_before_expansion(self):
        for reader, limit in [(self.reader(count=4097), 4096), (self.reader(), 1)]:
            with self.assertRaisesRegex(ValueError, "element count"):
                reader.inspect_slice(0x1000, 16, 0, limit)
        with self.assertRaisesRegex(ValueError, "read-only"):
            self.reader(count=3).inspect_slice(0x1000, 16, 0)

    def test_nonlocal_misaligned_or_invalid_elements_do_not_return_partial_coverage(self):
        cases = [
            (self.reader(relocations=[relocation(kind=1, symbol=2)]), "relocation"),
            (self.reader(relocations=[relocation(addend=0x2001)]), "aligned"),
            (self.reader(relocations=[relocation(addend=0x2000), relocation(address=0x2000, kind=1, symbol=2)]), "relocation"),
            (self.reader(relocations=[relocation(addend=0x2000), relocation(address=0x2000, addend=0x3000),
                                      relocation(address=0x2008)]), "relocation"),
            (self.reader(payload=b"\xffello"), "UTF-8"),
        ]
        for reader, reason in cases:
            with self.subTest(reason=reason), self.assertRaisesRegex(ValueError, reason):
                reader.inspect_slice(0x1000, 16, 0)


@unittest.skipUnless(HAS_INSPECTOR and RUST_COMPILER, "Requires external pyelftools and Rust.")
class CompiledStringControls(unittest.TestCase):
    def test_rust_optional_strings_select_the_active_niche_variant(self):
        from elftools.elf.elffile import ELFFile

        with tempfile.TemporaryDirectory(prefix="puffinbox-optional-string-control-") as directory:
            root = Path(directory)
            source, binary = root / "original.rs", root / "control"
            source.write_text('''
struct Holder { marker: u64, label: Option<&'static str> }
struct Budget { strings: [&'static str; 4096], tail: Option<&'static str> }
#[used] static ORIGINAL_STRING: &str = "optional shared";
#[used] static ORIGINAL_SOME: Option<&str> = Some("optional shared");
#[used] static ORIGINAL_EMPTY: Option<&str> = Some("");
#[used] static ORIGINAL_NONE: Option<&str> = None;
#[used] static ORIGINAL_HOLDER: Holder = Holder { marker: 17, label: Some("optional nested") };
#[used] static ORIGINAL_ARRAY: [Option<&str>; 3] = [Some("optional shared"), None, Some("optional array")];
#[used] static ORIGINAL_BYTES: Option<&[u8]> = Some(b"excluded optional byte slice");
#[used] static CONTROL_BUDGET: Budget = Budget { strings: ["budget shared"; 4096], tail: Some("excluded budget tail") };
fn main() {
    std::hint::black_box((&ORIGINAL_STRING, &ORIGINAL_SOME, &ORIGINAL_EMPTY, &ORIGINAL_NONE,
                         ORIGINAL_HOLDER.marker, &ORIGINAL_HOLDER.label, &ORIGINAL_ARRAY, &ORIGINAL_BYTES,
                         &CONTROL_BUDGET.strings, &CONTROL_BUDGET.tail, ORIGINAL_SOME.unwrap_or("fallback")));
}
''')
            subprocess.run([RUST_COMPILER, "-C", "debuginfo=2", "-C", "opt-level=0", "-C", "relocation-model=pic",
                            "-C", "link-arg=-pie", "-C", "link-arg=-Wl,-z,relro", str(source), "-o", str(binary)],
                           check=True, capture_output=True)
            with binary.open("rb") as stream:
                result = inventory(ELFFile(stream), stream)
        options = [row for row in result.get("optionalStringReferences", [])
                   if row["rootName"].startswith("ORIGINAL_")]
        self.assertEqual(len(options), 7)
        self.assertEqual(sum(row["variant"] == "Some" for row in options), 5)
        self.assertEqual(sum(row["variant"] == "None" for row in options), 2)
        array = [row for row in options if row["rootName"] == "ORIGINAL_ARRAY"]
        self.assertEqual([row["memberPath"] for row in array], [("[0]",), ("[1]",), ("[2]",)])
        strings = [row for row in result["stringReferences"] if row["rootName"].startswith("ORIGINAL_")]
        self.assertEqual(len(strings), 6)
        shared = [row for row in strings if row.get("storedBytesSha256") == hashlib.sha256(b"optional shared").hexdigest()]
        self.assertEqual(len(shared), 3)
        self.assertEqual(len({row["dataAddress"] for row in shared}), 1)
        nested = next(row for row in strings if row["rootName"] == "ORIGINAL_HOLDER")
        self.assertEqual(nested["memberPath"], ("label", "Some", "__0"))
        empty = next(row for row in strings if row["rootName"] == "ORIGINAL_EMPTY")
        self.assertEqual(empty["length"], 0)
        self.assertNotIn("dataAddress", empty)
        self.assertFalse(any(row["rootName"] in ("ORIGINAL_NONE", "ORIGINAL_BYTES") for row in strings))
        budget = [row for row in result["stringReferences"] if row["rootName"] == "CONTROL_BUDGET"]
        rejected = [row for row in result["unsupportedOptionalStringFields"] if row["rootName"] == "CONTROL_BUDGET"]
        self.assertEqual(len(budget), 4096)
        self.assertEqual(len(rejected), 1)
        self.assertIn("per-root", rejected[0]["reason"])
        self.assertFalse(result["licenseClearance"])

    def test_rust_string_slices_follow_typed_read_only_lists(self):
        from elftools.elf.elffile import ELFFile

        with tempfile.TemporaryDirectory(prefix="puffinbox-slice-control-") as directory:
            root = Path(directory)
            source, binary = root / "original.rs", root / "control"
            source.write_text('''
struct Holder { marker: u64, strings: &'static [&'static str] }
#[used] static ORIGINAL_SINGLE: &str = "slice shared";
#[used] static ORIGINAL_STRINGS: &[&str] = &["slice first", "slice shared", ""];
#[used] static ORIGINAL_EMPTY_SLICE: &[&str] = &[];
#[used] static ORIGINAL_HOLDER: Holder = Holder { marker: 17, strings: &["slice shared", "slice last"] };
#[used] static ORIGINAL_BYTES: &[u8] = b"excluded byte slice";
fn main() {
    std::hint::black_box((&ORIGINAL_SINGLE, &ORIGINAL_STRINGS, &ORIGINAL_EMPTY_SLICE,
                         ORIGINAL_HOLDER.marker, &ORIGINAL_HOLDER.strings, &ORIGINAL_BYTES));
}
''')
            subprocess.run([RUST_COMPILER, "-C", "debuginfo=2", "-C", "opt-level=0", "-C", "relocation-model=pic",
                            "-C", "link-arg=-pie", "-C", "link-arg=-Wl,-z,relro", str(source), "-o", str(binary)],
                           check=True, capture_output=True)
            with binary.open("rb") as stream:
                result = inventory(ELFFile(stream), stream)
        slices = {row["rootName"]: row for row in result.get("stringSliceReferences", [])
                  if row["rootName"].startswith("ORIGINAL_")}
        self.assertEqual(set(slices), {"ORIGINAL_STRINGS", "ORIGINAL_EMPTY_SLICE", "ORIGINAL_HOLDER"})
        self.assertEqual(slices["ORIGINAL_STRINGS"]["elementCount"], 3)
        self.assertEqual(slices["ORIGINAL_EMPTY_SLICE"]["elementCount"], 0)
        self.assertNotIn("dataAddress", slices["ORIGINAL_EMPTY_SLICE"])
        self.assertEqual(slices["ORIGINAL_HOLDER"]["memberPath"], ("strings",))
        strings = [row for row in result["stringReferences"] if row["rootName"].startswith("ORIGINAL_")]
        self.assertEqual(len(strings), 6)
        shared = [row for row in strings if row.get("storedBytesSha256") == hashlib.sha256(b"slice shared").hexdigest()]
        self.assertEqual(len(shared), 3)
        self.assertEqual(len({row["dataAddress"] for row in shared}), 1)
        self.assertEqual([row["memberPath"] for row in strings if row["rootName"] == "ORIGINAL_STRINGS"],
                         [("[0]",), ("[1]",), ("[2]",)])
        self.assertEqual([row["memberPath"] for row in strings if row["rootName"] == "ORIGINAL_HOLDER"],
                         [("strings", "[0]"), ("strings", "[1]")])
        payloads = [(int(row["dataAddress"], 16), int(row["dataAddress"], 16) + row["length"])
                    for row in strings if row["length"]]
        self.assertEqual(covered_bytes(payloads), len(b"slice firstslice sharedslice last"))
        self.assertFalse(result["licenseClearance"])

    def test_rust_pie_structures_arrays_aliases_and_empty_strings_use_real_dwarf(self):
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
#[used] static ORIGINAL_ARRAY: [&str; 3] = ["array control", "original string control", ""];
#[used] static ORIGINAL_GRID: [[&str; 2]; 2] = [["grid zero", "grid one"], ["grid two", "grid three"]];
#[used] static ORIGINAL_RECORDS: [Nested; 2] = [Nested { marker: 11, label: "record zero" }, Nested { marker: 13, label: "record one" }];
fn main() {
    std::hint::black_box((&ORIGINAL_STRING, &ORIGINAL_ALIAS, &ORIGINAL_EMPTY,
                         ORIGINAL_NESTED.marker, &ORIGINAL_NESTED.label, &ORIGINAL_ARRAY,
                         &ORIGINAL_GRID, &ORIGINAL_RECORDS));
}
''')
            subprocess.run([RUST_COMPILER, "-C", "debuginfo=2", "-C", "opt-level=0", "-C", "relocation-model=pic",
                            "-C", "link-arg=-pie", "-C", "link-arg=-Wl,-z,relro", str(source), "-o", str(binary)],
                           check=True, capture_output=True)
            with binary.open("rb") as stream:
                elf = ELFFile(stream)
                self.assertEqual(elf["e_type"], "ET_DYN")
                result = inventory(elf, stream)
        original = [row for row in result["stringReferences"] if row["rootName"].startswith("ORIGINAL_")]
        rows = {row["rootName"]: row for row in original if row["rootName"] not in
                {"ORIGINAL_ARRAY", "ORIGINAL_GRID", "ORIGINAL_RECORDS"}}
        self.assertEqual(set(rows), {"ORIGINAL_STRING", "ORIGINAL_ALIAS", "ORIGINAL_EMPTY", "ORIGINAL_NESTED"})
        self.assertEqual(rows["ORIGINAL_STRING"]["binding"], "R_X86_64_RELATIVE")
        self.assertEqual(rows["ORIGINAL_STRING"]["dataAddress"], rows["ORIGINAL_ALIAS"]["dataAddress"])
        self.assertEqual(rows["ORIGINAL_EMPTY"]["length"], 0)
        self.assertEqual(rows["ORIGINAL_NESTED"]["memberPath"], ("label",))
        self.assertEqual(rows["ORIGINAL_NESTED"]["storedBytesSha256"], hashlib.sha256(b"nested control").hexdigest())
        array = [row for row in original if row["rootName"] == "ORIGINAL_ARRAY"]
        self.assertEqual([row["memberPath"] for row in array], [("[0]",), ("[1]",), ("[2]",)])
        self.assertEqual([row["length"] for row in array], [len(b"array control"), len(b"original string control"), 0])
        self.assertEqual(array[1]["dataAddress"], rows["ORIGINAL_STRING"]["dataAddress"])
        for name, values in [("ORIGINAL_GRID", [b"grid zero", b"grid one", b"grid two", b"grid three"]),
                             ("ORIGINAL_RECORDS", [b"record zero", b"record one"])]:
            selected = [row for row in original if row["rootName"] == name]
            self.assertEqual([row["storedBytesSha256"] for row in selected],
                             [hashlib.sha256(value).hexdigest() for value in values])
        self.assertEqual([row["memberPath"] for row in original if row["rootName"] == "ORIGINAL_RECORDS"],
                         [("[0]", "label"), ("[1]", "label")])
        payload_spans = [(int(row["dataAddress"], 16), int(row["dataAddress"], 16) + row["length"])
                         for row in original if row["length"]]
        self.assertEqual(covered_bytes(payload_spans), len(b"original string controlnested controlarray control"
                                                         b"grid zerogrid onegrid twogrid threerecord zerorecord one"))
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
