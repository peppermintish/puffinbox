import unittest
from types import SimpleNamespace

from check_gnu_source_map import file_path, inventory, known_non_allowlisted_path


LIBRARY = "/toolchain/lib/rustlib/src/rust/library/"
SOURCE = LIBRARY + "core/src/str/mod.rs"


class Program(dict):
    def __init__(self, version=4, name=b"src/str/mod.rs", directory=(LIBRARY + "core").encode(), states=()):
        super().__init__(file_entry=[SimpleNamespace(name=name, dir_index=1 if version < 5 else 0)],
                         include_directory=[directory])
        self.header = SimpleNamespace(version=version)
        self.states = states

    def get_entries(self):
        return [SimpleNamespace(state=state) for state in self.states]


def state(address, line=12, file=1, end=False):
    return SimpleNamespace(address=address, line=line, file=file, end_sequence=end)


class Elf:
    def __init__(self, program, has_debug=True):
        self.program = program
        self.has_debug = has_debug
        self._linetable_cache = {}

    def has_dwarf_info(self):
        return self.has_debug

    def get_dwarf_info(self):
        return self

    def iter_segments(self):
        return iter([{"p_type": "PT_LOAD", "p_flags": 5, "p_vaddr": 0x1000, "p_memsz": 0x100}])

    def iter_CUs(self):
        top = SimpleNamespace(attributes={"DW_AT_comp_dir": SimpleNamespace(value=b"/work")})
        return iter([SimpleNamespace(get_top_DIE=lambda: top)])

    def line_program_for_CU(self, unit):
        return self.program


class SourceInventoryTests(unittest.TestCase):
    def test_dwarf_four_and_five_use_their_respective_index_bases(self):
        self.assertEqual(file_path(Program(4), 1, "/work"), SOURCE)
        self.assertEqual(file_path(Program(5), 0, "/work"), SOURCE)

    def test_relative_directory_is_resolved_against_compilation_directory(self):
        self.assertEqual(file_path(Program(directory=b"src"), 1, "/work"), "/work/src/src/str/mod.rs")

    def test_invalid_indexes_and_unknown_versions_fail_closed(self):
        for program, index in [(Program(3), 1), (Program(4), 0), (Program(5), 1)]:
            with self.subTest(version=program.header.version, index=index), self.assertRaises(ValueError):
                file_path(program, index, "/work")
        program = Program()
        program["file_entry"][0].dir_index = 8
        with self.assertRaises(ValueError):
            file_path(program, 1, "/work")

    def test_missing_debug_and_only_discarded_addresses_do_not_imply_absence(self):
        program = Program(states=[state(0), state(0x20, end=True)])
        for elf in [Elf(program, has_debug=False), Elf(program)]:
            with self.assertRaises(ValueError):
                inventory(elf, {SOURCE: "source-hash"})

    def test_sequence_boundaries_and_line_zero_are_not_attributed_to_a_source(self):
        program = Program(states=[state(0x1000), state(0x1008, end=True),
                                  state(0x1010, line=0), state(0x1014, end=True),
                                  state(0), state(0x10, end=True)])
        result = inventory(Elf(program), {SOURCE: "source-hash"})
        self.assertEqual(result["mappedIntervals"], 1)
        self.assertEqual(result["standardLibrarySourceFiles"][SOURCE]["intervalBytes"], 8)
        self.assertEqual(result["lineZeroIntervals"], 1)
        self.assertEqual(result["discardedOrInvalidAddressIntervals"], 1)
        self.assertFalse(result["licenseClearance"])

    def test_a_mapped_standard_library_file_requires_its_exact_build_hash(self):
        program = Program(states=[state(0x1000), state(0x1008, end=True)])
        with self.assertRaises(ValueError):
            inventory(Elf(program), {})

    def test_generated_unicode_is_reported_when_its_own_archive_is_absent(self):
        directory = (LIBRARY + "core").encode()
        path = LIBRARY + "core/src/unicode/unicode_data.rs"
        program = Program(name=b"src/unicode/unicode_data.rs", directory=directory,
                          states=[state(0x1000), state(0x1008, end=True)])
        result = inventory(Elf(program), {path: "unicode-source-hash"})
        self.assertIn(path, result["knownNonAllowlistedMappedFiles"])

    def test_known_license_exceptions_are_checked_without_rejecting_unicode_module(self):
        for suffix in ["core/src/unicode/unicode_data.rs", "std/src/sys/sync/mutex/fuchsia.rs",
                       "vendor/compiler_builtins/src/lib.rs"]:
            self.assertTrue(known_non_allowlisted_path(LIBRARY + suffix))
        self.assertFalse(known_non_allowlisted_path(LIBRARY + "core/src/unicode/mod.rs"))
        self.assertFalse(known_non_allowlisted_path("/cargo/unicode-normalization/src/lib.rs"))


if __name__ == "__main__":
    unittest.main()
