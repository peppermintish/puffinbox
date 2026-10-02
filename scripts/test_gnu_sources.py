import copy
import hashlib
import io
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

from capture_gnu_sources import capture, capture_native, mapped_dependencies


class DependencySourceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.package = self.root / "registry/src/test-index/demo-1.0"
        self.package.mkdir(parents=True)
        self.manifest = self.package / "Cargo.toml"
        self.manifest.write_text('[package]\nname="demo"\nversion="1.0"\nlicense="MIT"\n')
        self.source = self.package / "lib.rs"
        self.source.write_bytes(b"abc")
        (self.package / "LICENSE").write_text("Synthetic notice fixture")
        self.generated = self.root / "build/out"
        self.generated.mkdir(parents=True)
        self.native = self.generated / "native.S"
        self.native.write_bytes(b"abc")
        self.metadata = {"packages": [{"id": "registry+test#demo@1.0", "name": "demo", "version": "1.0",
                                      "license": "MIT", "license_file": None, "source": "registry+test",
                                      "manifest_path": str(self.manifest)}]}
        self.archive = self.root / "registry/cache/test-index/demo-1.0.crate"
        self.archive.parent.mkdir(parents=True)
        self.lockfile = self.root / "Cargo.lock"
        self.archive_members = {"demo-1.0/" + path.name: path.read_bytes() for path in self.package.iterdir()}
        self.write_archive(self.archive_members)
        (self.package / ".cargo-ok").write_text("Cargo cache marker")
        self.inventory = {"allMappedSources": {str(self.source): {"mappedIntervals": 1, "intervalBytes": 8},
                                               str(self.native): {"mappedIntervals": 2, "intervalBytes": 12}}}

    def write_archive(self, members):
        with tarfile.open(self.archive, "w:gz") as archive:
            for name, content in members.items():
                member = tarfile.TarInfo(name)
                member.size = len(content)
                archive.addfile(member, io.BytesIO(content))
        self.lockfile.write_text('[[package]]\nname="demo"\nversion="1.0"\nsource="registry+test"\n'
                                 + 'checksum="' + hashlib.sha256(self.archive.read_bytes()).hexdigest() + '"\n')

    def snapshot(self):
        return capture(self.metadata, self.generated, self.lockfile)

    def test_registry_and_generated_bytes_have_separate_provenance(self):
        record = self.snapshot()
        mapped = mapped_dependencies(self.inventory, record)
        self.assertEqual(mapped[str(self.source)]["sha256"],
                         "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        self.assertEqual(mapped[str(self.source)]["declaredLicense"], "MIT")
        self.assertTrue(record["packages"][0]["registryArchiveVerified"])
        self.assertIn(str(self.package / "LICENSE"), mapped[str(self.source)]["noticeFiles"])
        self.assertEqual(mapped[str(self.native)]["byteIdenticalPackageFiles"], [str(self.source)])
        self.assertNotIn("declaredLicense", mapped[str(self.native)])
        self.assertTrue(mapped[str(self.native)]["licenseReviewRequired"])
        self.assertFalse(record["licenseClearance"])

    def test_changed_and_missing_registry_files_fail_archive_verification(self):
        self.source.write_bytes(b"modified")
        with self.assertRaisesRegex(ValueError, "differs from its locked archive"):
            self.snapshot()
        self.source.unlink()
        with self.assertRaisesRegex(ValueError, "differs from its locked archive"):
            self.snapshot()

    def test_registry_archive_paths_cannot_escape_the_package(self):
        for name in ("../lib.rs", "demo-1.0/../lib.rs", "/demo-1.0/lib.rs", "unrelated/lib.rs"):
            self.write_archive({name: b"abc"})
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "Unsafe registry archive"):
                self.snapshot()

    def test_added_files_and_changed_archive_bytes_fail(self):
        extra = self.package / "injected.rs"
        extra.write_text("Injected file")
        with self.assertRaisesRegex(ValueError, "additional files"):
            self.snapshot()
        extra.unlink()
        with self.archive.open("ab") as archive:
            archive.write(b"changed")
        with self.assertRaisesRegex(ValueError, "lockfile checksum"):
            self.snapshot()

    def test_archive_links_are_not_accepted_as_source_files(self):
        with tarfile.open(self.archive, "w:gz") as archive:
            member = tarfile.TarInfo("demo-1.0/link.rs")
            member.type = tarfile.SYMTYPE
            member.linkname = "lib.rs"
            archive.addfile(member)
        value = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        self.lockfile.write_text('[[package]]\nname="demo"\nversion="1.0"\nsource="registry+test"\nchecksum="' + value + '"\n')
        with self.assertRaisesRegex(ValueError, "Unsupported or duplicate"):
            self.snapshot()

    def test_missing_source_snapshot_does_not_inherit_the_package_declaration(self):
        record = self.snapshot()
        del record["sourceFiles"][str(self.source)]
        with self.assertRaisesRegex(ValueError, "lack exact snapshots"):
            mapped_dependencies(self.inventory, record)
        record = self.snapshot()
        del record["generatedSourceFiles"][str(self.native)]
        with self.assertRaisesRegex(ValueError, "lack exact snapshots"):
            mapped_dependencies(self.inventory, record)

    def test_missing_or_inconsistent_manifest_and_notice_hashes_fail(self):
        record = self.snapshot()
        for path in [str(self.manifest), str(self.package / "LICENSE")]:
            changed = copy.deepcopy(record)
            changed["sourceFiles"][path] = "changed"
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "hash"):
                mapped_dependencies(self.inventory, changed)

    def test_nested_package_selects_its_own_manifest_and_license(self):
        record = self.snapshot()
        parent_manifest = self.root / "Cargo.toml"
        parent_manifest.write_text("Parent fixture")
        parent_hash = hashlib.sha256(parent_manifest.read_bytes()).hexdigest()
        record["sourceFiles"][str(parent_manifest)] = parent_hash
        record["packages"].append({"id": "path+parent", "name": "parent", "version": "1.0",
                                   "declaredLicense": "Apache-2.0", "sourceRoot": str(self.root),
                                   "manifestPath": str(parent_manifest), "manifestSha256": parent_hash,
                                   "noticeFiles": {}})
        mapped = mapped_dependencies(self.inventory, record)
        self.assertEqual(mapped[str(self.source)]["packageId"], "registry+test#demo@1.0")

    def test_unsafe_roots_and_duplicate_packages_fail(self):
        record = self.snapshot()
        for changes in ("relative", "parent", "duplicate", "generated"):
            changed = copy.deepcopy(record)
            if changes == "relative":
                changed["packages"][0]["sourceRoot"] = "registry/demo"
            elif changes == "parent":
                changed["packages"][0]["sourceRoot"] += "/../escape"
            elif changes == "duplicate":
                changed["packages"].append(copy.deepcopy(changed["packages"][0]))
            else:
                changed["generatedSourceRoot"] = "build"
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                mapped_dependencies(self.inventory, changed)

    def test_empty_generated_snapshot_and_missing_archive_fail(self):
        self.native.unlink()
        with self.assertRaisesRegex(ValueError, "snapshot is empty"):
            self.snapshot()
        self.archive.unlink()
        with self.assertRaisesRegex(ValueError, "archive is missing"):
            self.snapshot()

    def test_symlinked_file_is_not_hashed_as_an_unrelated_source(self):
        link = self.package / "linked.rs"
        try:
            link.symlink_to(self.native)
        except OSError as error:
            self.skipTest(f"Symlinks are unavailable: {error}")
        with self.assertRaisesRegex(ValueError, "cannot follow a symlink"):
            self.snapshot()

    def test_native_source_survives_cleanup_but_changed_inputs_remain_ambiguous(self):
        traces = self.root / "traces"
        traces.mkdir()
        before = hashlib.sha256(self.native.read_bytes()).hexdigest()
        record = {"licenseClearance": False, "exitCode": 0, "compileSource": True,
                  "sourceFiles": {str(self.native): before}}
        (traces / "one.json").write_text(json.dumps(record))
        snapshot = self.snapshot()
        self.native.unlink()
        snapshot.update(capture_native(traces))
        mapped = mapped_dependencies(self.inventory, snapshot)
        self.assertEqual(mapped[str(self.native)]["sourceKind"], "native-compiler")
        record["sourceFiles"][str(self.native)] = hashlib.sha256(b"changed").hexdigest()
        (traces / "two.json").write_text(json.dumps(record))
        snapshot.update(capture_native(traces))
        with self.assertRaisesRegex(ValueError, "changed between compiler"):
            mapped_dependencies(self.inventory, snapshot)

    def test_native_and_post_build_hash_disagreement_fails(self):
        snapshot = self.snapshot()
        snapshot["nativeSourceFiles"][str(self.source)] = hashlib.sha256(b"changed").hexdigest()
        with self.assertRaisesRegex(ValueError, "differs from its final"):
            mapped_dependencies(self.inventory, snapshot)

    def test_probe_only_native_traces_require_an_explicit_no_compilation_mode(self):
        traces = self.root / "probe-traces"
        traces.mkdir()
        (traces / "version.json").write_text(json.dumps({"licenseClearance": False, "exitCode": 0,
                                                       "compileSource": False, "sourceFiles": {}}))
        with self.assertRaisesRegex(ValueError, "no successful compilation"):
            capture_native(traces)
        result = capture_native(traces, require_compilations=False)
        self.assertEqual(result["nativeCompileInvocations"], 0)
        self.assertEqual(result["nativeSourceFiles"], {})
        self.assertEqual(len(result["nativeCompilerTraceHashes"]), 1)


WRAPPER_PATH = Path(__file__).resolve().parents[1] / "experiments/linux-gnu-runtime/native-source-wrapper.py"


class NativeCompilerTraceTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux", "GNU dependency paths require the Linux probe environment")
    def test_dependency_rule_preserves_escaped_names_and_only_reads_the_first_rule(self):
        specification = importlib.util.spec_from_file_location("native_source_wrapper", WRAPPER_PATH)
        wrapper = importlib.util.module_from_spec(specification)
        specification.loader.exec_module(wrapper)
        directory = Path("/probe/build")
        content = "object.o: source.c \\\n include\\ file.h dollar$$name.h\ninclude\\ file.h:\n"
        self.assertEqual(wrapper.dependency_paths(content, directory),
                         [directory / "source.c", directory / "include file.h", directory / "dollar$name.h"])
        for value in ("missing target separator", "object.o:"):
            with self.assertRaises(ValueError):
                wrapper.dependency_paths(value, directory)

    @unittest.skipUnless(sys.platform == "linux" and shutil.which("cc"), "Requires the disposable Linux C compiler")
    def test_real_compilation_captures_a_header_before_source_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            source = root / "source.c"
            source.write_text('#include "value.h"\nint example(void) { return EXAMPLE_VALUE; }\n')
            header = root / "value.h"
            header.write_bytes(b"#define EXAMPLE_VALUE 7\n")
            output = root / "output"
            output.mkdir()
            environment = {**os.environ, "PUFFINBOX_RUNTIME_PROBE_OUTPUT": str(output)}
            subprocess.run([sys.executable, str(WRAPPER_PATH), "-c", str(source), "-o", str(root / "object.o")],
                           cwd=root, env=environment, check=True, capture_output=True)
            records = list((output / "native-compiler-traces").glob("*.json"))
            self.assertEqual(len(records), 1)
            record = json.loads(records[0].read_text())
            self.assertTrue(record["dependencyRuleCaptured"])
            self.assertEqual(record["sourceFiles"][str(header)], hashlib.sha256(header.read_bytes()).hexdigest())
            source.unlink()
            header.unlink()
            snapshot = capture_native(output / "native-compiler-traces")
            self.assertIn(str(header), snapshot["nativeSourceFiles"])
            self.assertEqual(snapshot["nativeCompileInvocations"], 1)


if __name__ == "__main__":
    unittest.main()
