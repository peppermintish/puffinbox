# SPDX-License-Identifier: MIT OR Apache-2.0
import copy
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from check_gnu_notices import InTreeNotices, classify, expression_allowed


SYSROOT = "/compiler"
LIBRARY = SYSROOT + "/lib/rustlib/src/rust/library/"
SOURCE_HASH = "1" * 64


def node(path, expression, children=""):
    return (f"<div><p><b>File/Directory:</b> <code>{path}</code></p>"
            f"<p><b>License:</b> {expression}</p><p><b>Copyright:</b> Synthetic fixture</p>{children}</div>")


def notice(children=""):
    return ('<h2 id="in-tree-files">In-tree</h2>' + node(".", "Apache-2.0 OR MIT", children)
            + '<h2 id="out-of-tree-dependencies">Out-of-tree</h2>').encode()


def inputs(paths=("core/src/str/mod.rs",), data=None):
    data = notice() if data is None else data
    hashes = {"sysroot": SYSROOT, "compilerNotices": {"COPYRIGHT-library.html": hashlib.sha256(data).hexdigest()},
              "sourceFiles": {LIBRARY + path: SOURCE_HASH for path in paths}}
    hashes_bytes = json.dumps(hashes).encode()
    source = {"binarySha256": "2" * 64, "sourceHashesSha256": hashlib.sha256(hashes_bytes).hexdigest(),
              "standardLibrarySourceFiles": {LIBRARY + path: {"sha256": SOURCE_HASH} for path in paths}}
    return source, hashes, data, hashes_bytes


def report(source, hashes, data, hashes_bytes):
    return classify(source, hashes, data, "3" * 64, hashlib.sha256(hashes_bytes).hexdigest())


class CompilerNoticeTests(unittest.TestCase):
    def test_license_choices_and_required_components_respect_precedence(self):
        expected = {
            "MIT": True, "apache-2.0": True, "Unlicense OR MIT": True,
            "MIT AND Apache-2.0": True, "MIT OR BSD-2-Clause AND Apache-2.0": True,
            "(MIT OR BSD-2-Clause) AND Apache-2.0": True,
            "(Apache-2.0 OR MIT) AND BSD-2-Clause": False,
            "MIT AND (Apache-2.0 OR BSD-2-Clause)": True,
            "BSD-2-Clause AND MIT OR Apache-2.0 AND Zlib": False,
            "mit and (apache-2.0 or Zlib)": True, "LicenseRef-MIT": False, "MIT+": False,
            "DocumentRef-fixture:LicenseRef-MIT": False,
        }
        for expression, allowed in expected.items():
            with self.subTest(expression=expression):
                self.assertEqual(expression_allowed(expression), allowed)

    def test_with_additions_are_not_stripped_from_required_licenses(self):
        for expression, allowed in [
            ("MIT AND Apache-2.0 WITH LLVM-exception", False),
            ("MIT OR Apache-2.0 WITH LLVM-exception", True),
            ("MIT WITH AdditionRef-synthetic", False),
            ("MIT with LLVM-exception", False),
        ]:
            with self.subTest(expression=expression):
                self.assertEqual(expression_allowed(expression), allowed)

    def test_malformed_alternatives_fail_even_after_an_allowed_branch(self):
        for expression in ["", "MIT OR", "MIT OR ()", "MIT OR (Apache-2.0", "MIT OR Apache-2.0 garbage",
                           "MIT && Apache-2.0", "MIT OR Apache-2.0 WITH", "(MIT) WITH LLVM-exception",
                           "MIT OR Apache-2.0 WITH (LLVM-exception)", "MIT Or Apache-2.0", "MIT +"]:
            with self.subTest(expression=expression), self.assertRaises(ValueError):
                expression_allowed(expression)

    def test_nested_exceptions_use_the_most_specific_path_and_directory_boundaries(self):
        data = notice(node("library/core/src/unicode", "Unicode-3.0", node("mod.rs", "MIT OR Apache-2.0"))
                      + node("library/std/src/sys/sync/mutex/fuchsia.rs", "BSD-2-Clause AND (MIT OR Apache-2.0)"))
        paths = ("core/src/unicode/mod.rs", "core/src/unicode/unicode_data.rs", "core/src/unicode_extra.rs",
                 "std/src/sys/sync/mutex/fuchsia.rs")
        result = report(*inputs(paths, data))
        self.assertEqual(set(result["outsideNoticeAllowlist"]),
                         {LIBRARY + paths[1], LIBRARY + paths[3]})
        files = result["mappedStandardLibraryFiles"]
        self.assertEqual(files[LIBRARY + paths[0]]["applicableNoticePath"], "library/core/src/unicode/mod.rs")
        self.assertEqual(files[LIBRARY + paths[2]]["applicableNoticePath"], ".")
        self.assertFalse(result["licenseClearance"])

    def test_vendor_paths_are_unreviewed_even_when_default_is_allowlisted(self):
        path = "vendor/foldhash/src/lib.rs"
        result = report(*inputs((path,)))
        record = result["outsideNoticeAllowlist"][LIBRARY + path]
        self.assertIsNone(record["licenseExpression"])
        self.assertIsNone(record["applicableNoticePath"])
        self.assertEqual(record["reviewStatus"], "unreviewed-vendored-dependency")
        self.assertFalse(result["licenseClearance"])

    def test_missing_sections_unclosed_trees_and_fields_fail_closed(self):
        fixtures = [b"<div></div>", notice().replace(b'</div>', b'', 1),
                    notice().replace(b'out-of-tree-dependencies', b'unknown'),
                    notice().replace(b'License:', b'Missing:'),
                    notice().replace(b'File/Directory:', b'Missing:'),
                    notice().replace(b'<div>', b'<div><p>License: MIT</p>', 1),
                    notice().replace(b'<div>', b'<div>unreviewed text', 1),
                    notice().replace(b'<code>.</code>', b'<code>library</code>')]
        for data in fixtures:
            with self.subTest(data=data), self.assertRaises(ValueError):
                InTreeNotices().rules(data.decode())

    def test_duplicate_rules_and_unsafe_paths_fail_closed(self):
        for children in [node("library/core", "MIT") * 2,
                         *(node(path, "MIT") for path in ["/library", "../library", "library/../std",
                                                           "library//std", "library\\std", "C:/library", "."])]:
            with self.subTest(children=children), self.assertRaises(ValueError):
                InTreeNotices().rules(notice(children).decode())

    def test_changed_notice_and_mismatched_source_record_fail_closed(self):
        source, hashes, data, hashes_bytes = inputs()
        with self.assertRaises(ValueError):
            report(source, hashes, data + b"\n", hashes_bytes)
        changed_source = copy.deepcopy(source)
        changed_source["sourceHashesSha256"] = "4" * 64
        with self.assertRaises(ValueError):
            report(changed_source, hashes, data, hashes_bytes)
        changed_source = copy.deepcopy(source)
        changed_source["standardLibrarySourceFiles"][LIBRARY + "core/src/str/mod.rs"]["sha256"] = "5" * 64
        with self.assertRaises(ValueError):
            report(changed_source, hashes, data, hashes_bytes)
        with self.assertRaises(ValueError):
            report(source, {key: value for key, value in hashes.items() if key != "compilerNotices"}, data, hashes_bytes)

    def test_empty_sources_wrong_sysroot_and_traversal_fail_closed(self):
        source, hashes, data, hashes_bytes = inputs()
        for paths in [{}, {"/another/compiler/lib/rustlib/src/rust/library/core/lib.rs": {"sha256": SOURCE_HASH}},
                      {LIBRARY + "core/../vendor/lib.rs": {"sha256": SOURCE_HASH}}]:
            changed_source = copy.deepcopy(source)
            changed_source["standardLibrarySourceFiles"] = paths
            with self.subTest(paths=paths), self.assertRaises(ValueError):
                report(changed_source, hashes, data, hashes_bytes)

    def test_cli_records_rejection_without_overwriting_existing_evidence(self):
        data = notice(node("library/core/src/unicode", "Unicode-3.0"))
        source, hashes, data, hashes_bytes = inputs(("core/src/unicode/unicode_data.rs",), data)
        script = Path(__file__).with_name("check_gnu_notices.py")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "inventory.json").write_text(json.dumps(source))
            (root / "hashes.json").write_bytes(hashes_bytes)
            (root / "notice.html").write_bytes(data)
            command = [sys.executable, str(script), "--inventory", str(root / "inventory.json"),
                       "--source-hashes", str(root / "hashes.json"), "--notice", str(root / "notice.html"),
                       "--output", str(root / "result.json")]
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(result.returncode, 1, result.stderr)
            saved = (root / "result.json").read_bytes()
            self.assertIn(LIBRARY + "core/src/unicode/unicode_data.rs", json.loads(saved)["outsideNoticeAllowlist"])
            self.assertFalse(json.loads(saved)["licenseClearance"])
            result = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertEqual((root / "result.json").read_bytes(), saved)


if __name__ == "__main__":
    unittest.main()
