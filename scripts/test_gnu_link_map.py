import unittest

from check_gnu_link_map import inventory


HEADER = "VMA LMA Size Align Out In Symbol\n"
PROJECT = " 1000 1000 20 16 /target/deps/libpuffinbox-a.rlib(main.o):(.text.main)\n"


class RuntimeInventoryTests(unittest.TestCase):
    def test_rebuilt_archive_outside_system_directory_is_detected(self):
        result = inventory(HEADER + PROJECT + " 1020 1020 40 16 /cache/deps/libcompiler_builtins-a.rlib(math.o):(.text.round)\n", "")
        self.assertFalse(result["knownRuntimeInputsAbsent"])
        self.assertEqual(result["retainedRuntimeSections"][0]["bytes"], 64)

    def test_lazy_archive_cross_reference_does_not_establish_retention(self):
        result = inventory(HEADER + PROJECT + "atexit /usr/lib/libc_nonshared.a(atexit.oS)\n", "                 U core::unicode::unicode_data::white_space::lookup\n")
        self.assertTrue(result["knownRuntimeInputsAbsent"])
        self.assertEqual(result["unicodeImports"], 1)
        self.assertFalse(result["licenseClearance"])

    def test_inlined_generated_unicode_body_is_detected(self):
        result = inventory(HEADER + PROJECT, "0000000000010000 t core::unicode::unicode_data::white_space::lookup\n")
        self.assertFalse(result["knownRuntimeInputsAbsent"])

    def test_system_startup_object_and_comment_are_detected(self):
        result = inventory(HEADER + PROJECT + " 1040 1040 18 1 /lib/crtbeginS.o:(.comment)\n", "")
        self.assertFalse(result["knownRuntimeInputsAbsent"])

    def test_empty_or_unknown_map_format_fails_closed(self):
        for text in ["", "Archive member included to satisfy reference\n", HEADER, HEADER + " 1000 1000 20 16 <internal>:(.text)\n"]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                inventory(text, "")


if __name__ == "__main__":
    unittest.main()
