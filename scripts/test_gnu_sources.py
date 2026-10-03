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

from capture_gnu_sources import capture, capture_native, mapped_dependencies, verify_native_source_bytes


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

    def test_legacy_native_hashes_survive_cleanup_but_changed_inputs_remain_ambiguous(self):
        traces = self.root / "traces"
        traces.mkdir()
        before = hashlib.sha256(self.native.read_bytes()).hexdigest()
        record = {"licenseClearance": False, "exitCode": 0, "compileSource": True,
                  "sourceFiles": {str(self.native): before}}
        (traces / "one.json").write_text(json.dumps(record))
        snapshot = self.snapshot()
        self.native.unlink()
        snapshot.update(capture_native(traces))
        self.assertFalse(snapshot["nativeSourceBytesVerifiedAtCapture"])
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


class NativeSourceBytesTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.traces = self.root / "native-compiler-traces"
        self.traces.mkdir()
        self.store = self.root / "native-source-bytes"
        self.store.mkdir()
        self.content = b"Synthetic generated assembly\n"
        self.value = hashlib.sha256(self.content).hexdigest()
        self.blob = self.store / self.value
        self.blob.write_bytes(self.content)
        self.record = {"schemaVersion": 2, "sourceByteDirectory": "native-source-bytes", "licenseClearance": False,
                       "exitCode": 0, "compileSource": True,
                       "sourceFiles": {str(self.root / "removed/native.S"): self.value}}
        self.trace = self.traces / "one.json"
        self.write_trace()

    def write_trace(self):
        self.trace.write_text(json.dumps(self.record))

    def test_copies_are_verified_after_original_inputs_are_gone(self):
        snapshot = capture_native(self.traces, require_source_bytes=True)
        self.assertTrue(snapshot["nativeSourceBytesVerifiedAtCapture"])
        result = verify_native_source_bytes(snapshot, self.store)
        self.assertEqual(result["verifiedFiles"], 1)
        self.assertEqual(result["verifiedBytes"], len(self.content))
        self.assertFalse(result["licenseClearance"])
        self.assertEqual((self.root / snapshot["nativeSourceByteFiles"][self.value]).read_bytes(), self.content)
        # The byte directory can be relocated independently of builder paths.
        relocated = self.root / "relocated"
        self.store.rename(relocated)
        self.assertEqual(verify_native_source_bytes(snapshot, relocated), result)

    def test_missing_or_modified_copies_fail_capture_and_later_review(self):
        snapshot = capture_native(self.traces, require_source_bytes=True)
        for content in (b"changed", None):
            if content is None:
                self.blob.unlink()
            else:
                self.blob.write_bytes(content)
            with self.subTest(content=content):
                with self.assertRaisesRegex(ValueError, "missing, linked or altered"):
                    capture_native(self.traces)
                with self.assertRaisesRegex(ValueError, "missing, linked or altered"):
                    verify_native_source_bytes(snapshot, self.store)

    def test_hash_only_legacy_records_cannot_satisfy_a_byte_requirement(self):
        legacy = copy.deepcopy(self.record)
        del legacy["schemaVersion"]
        del legacy["sourceByteDirectory"]
        (self.traces / "legacy.json").write_text(json.dumps(legacy))
        snapshot = capture_native(self.traces)
        self.assertFalse(snapshot["nativeSourceBytesVerifiedAtCapture"])
        with self.assertRaisesRegex(ValueError, "hashes without byte copies"):
            capture_native(self.traces, require_source_bytes=True)
        with self.assertRaisesRegex(ValueError, "no complete byte-copy verification"):
            verify_native_source_bytes(snapshot, self.store)

    def test_byte_indexes_cannot_escape_or_omit_source_hashes(self):
        snapshot = capture_native(self.traces, require_source_bytes=True)
        for relative in ("../" + self.value, "/" + self.value, "native-source-bytes/../" + self.value):
            changed = copy.deepcopy(snapshot)
            changed["nativeSourceByteFiles"][self.value] = relative
            with self.subTest(relative=relative), self.assertRaisesRegex(ValueError, "Unsafe native source byte-copy"):
                verify_native_source_bytes(changed, self.store)
        snapshot["nativeSourceByteFiles"].clear()
        with self.assertRaisesRegex(ValueError, "does not cover"):
            verify_native_source_bytes(snapshot, self.store)
        self.record["sourceByteDirectory"] = "../outside"
        self.write_trace()
        with self.assertRaisesRegex(ValueError, "Unsafe native source byte directory"):
            capture_native(self.traces)

    def test_symlinked_copies_and_directories_fail(self):
        actual = self.root / "actual"
        actual.write_bytes(self.content)
        self.blob.unlink()
        try:
            self.blob.symlink_to(actual)
        except OSError as error:
            self.skipTest(f"Symlinks are unavailable: {error}")
        with self.assertRaisesRegex(ValueError, "missing, linked or altered"):
            capture_native(self.traces)
        self.blob.unlink()
        self.store.rmdir()
        actual_store = self.root / "actual-store"
        actual_store.mkdir()
        (actual_store / self.value).write_bytes(self.content)
        self.store.symlink_to(actual_store, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "directory is missing or linked"):
            capture_native(self.traces)

    def test_probe_only_capture_requires_an_existing_byte_directory(self):
        self.record["compileSource"] = False
        self.record["sourceFiles"] = {}
        self.write_trace()
        self.blob.unlink()
        snapshot = capture_native(self.traces, require_compilations=False, require_source_bytes=True)
        self.assertEqual(verify_native_source_bytes(snapshot, self.store)["verifiedFiles"], 0)
        self.store.rmdir()
        with self.assertRaisesRegex(ValueError, "directory is missing or linked"):
            capture_native(self.traces, require_compilations=False, require_source_bytes=True)


class NativeCompilerTraceTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == "linux", "GNU dependency paths require the Linux probe environment")
    def test_assembler_debug_basenames_require_one_exact_known_input(self):
        specification = importlib.util.spec_from_file_location("native_source_wrapper", WRAPPER_PATH)
        wrapper = importlib.util.module_from_spec(specification)
        specification.loader.exec_module(wrapper)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            first = root / "one/source.c"
            first.parent.mkdir()
            first.write_text("Synthetic first source")
            label = root / "source.c"
            self.assertEqual(wrapper.resolve_assembler_paths([label], [first], root), ([first], {str(label): str(first)}))
            second = root / "two/source.c"
            second.parent.mkdir()
            second.write_text("Synthetic second source")
            with self.assertRaisesRegex(ValueError, "missing or ambiguous"):
                wrapper.resolve_assembler_paths([label], [first, second], root)
            with self.assertRaisesRegex(ValueError, "missing or ambiguous"):
                wrapper.resolve_assembler_paths([root / "unrelated.inc"], [first], root)

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
        self.assertEqual(wrapper.dependency_paths("object.o:\n", directory, allow_empty=True), [])

    @unittest.skipUnless(sys.platform == "linux" and shutil.which("cc"), "Requires the disposable Linux C compiler")
    def test_real_compilation_captures_a_header_before_source_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            (root / "nested").mkdir()
            source = root / "nested/source.c"
            source.write_text('#include "value.h"\nint example(void) { return EXAMPLE_VALUE; }\n')
            header = root / "nested/value.h"
            header.write_bytes(b"#define EXAMPLE_VALUE 7\n")
            output = root / "output"
            output.mkdir()
            environment = {**os.environ, "PUFFINBOX_RUNTIME_PROBE_OUTPUT": str(output)}
            processes = [subprocess.Popen([sys.executable, str(WRAPPER_PATH), "-g", "-c", str(source),
                                           "-o", str(root / f"object-{index}.o")], cwd=root, env=environment,
                                          stdout=subprocess.PIPE, stderr=subprocess.PIPE) for index in range(2)]
            for process in processes:
                stdout, stderr = process.communicate(timeout=30)
                self.assertEqual(process.returncode, 0, stdout + stderr)
            records = list((output / "native-compiler-traces").glob("*.json"))
            self.assertEqual(len(records), 2)
            record = json.loads(records[0].read_text())
            self.assertTrue(record["dependencyRuleCaptured"])
            self.assertTrue(record["assemblerDependencyRuleCaptured"])
            self.assertTrue(record["preprocessorDependencyRuleCaptured"])
            self.assertEqual(record["assemblerPathAliases"].get(str(root / "source.c")), str(source))
            self.assertEqual(record["sourceFiles"][str(header)], hashlib.sha256(header.read_bytes()).hexdigest())
            source.unlink()
            header.unlink()
            snapshot = capture_native(output / "native-compiler-traces", require_source_bytes=True)
            self.assertIn(str(header), snapshot["nativeSourceFiles"])
            self.assertEqual(snapshot["nativeCompileInvocations"], 2)
            self.assertEqual((output / snapshot["nativeSourceByteFiles"][record["sourceFiles"][str(header)]]).read_bytes(),
                             b"#define EXAMPLE_VALUE 7\n")
            self.assertTrue(snapshot["nativeSourceBytesVerifiedAtCapture"])
            self.assertEqual(verify_native_source_bytes(snapshot, output / "native-source-bytes")["verifiedFiles"],
                             len(set(snapshot["nativeSourceFiles"].values())))
            self.assertEqual((output / snapshot["nativeSourceByteFiles"][record["sourceFiles"][str(header)]]).stat().st_mode & 0o777,
                             0o444)

    @unittest.skipUnless(sys.platform == "linux" and shutil.which("cc"), "Requires the disposable Linux C compiler")
    def test_inherited_user_only_dependencies_still_capture_system_headers(self):
        for option in ("-MMD", "--write-user-dependencies", "preprocessor"):
            with self.subTest(option=option), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                system = root / "system"
                system.mkdir()
                header = system / "system-value.h"
                header.write_bytes(b"#define SYSTEM_VALUE 19\n")
                source = root / "source.c"
                source.write_text('#include <system-value.h>\n#ifndef CPP_VALUE\n#define CPP_VALUE 0\n#endif\n'
                                  'int example(void) { return SYSTEM_VALUE + CPP_VALUE; }\n')
                dependency_option = "-Wp,-MMD," + str(root / "original.d") + ",-DCPP_VALUE=3" if option == "preprocessor" else option
                arguments = [dependency_option, "-MF", str(root / "original.d"), "-isystem", str(system),
                             "-c", str(source)]
                baseline = subprocess.run(["cc", *arguments, "-o", str(root / "baseline.o")],
                                          cwd=root, capture_output=True, timeout=30)
                self.assertEqual(baseline.returncode, 0, baseline.stderr)
                self.assertNotIn(str(header), (root / "original.d").read_text())
                output = root / "output"
                output.mkdir()
                result = subprocess.run([sys.executable, str(WRAPPER_PATH), *arguments,
                                         "-o", str(root / "captured.o")], cwd=root,
                                        env={**os.environ, "PUFFINBOX_RUNTIME_PROBE_OUTPUT": str(output)},
                                        capture_output=True, timeout=30)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((root / "baseline.o").read_bytes(), (root / "captured.o").read_bytes())
                records = list((output / "native-compiler-traces").glob("*.json"))
                self.assertEqual(len(records), 1)
                record = json.loads(records[0].read_text())
                self.assertIn(dependency_option, record["arguments"])
                digest = hashlib.sha256(header.read_bytes()).hexdigest()
                self.assertEqual(record["sourceFiles"][str(header)], digest)
                source.unlink()
                header.unlink()
                snapshot = capture_native(output / "native-compiler-traces", require_source_bytes=True)
                self.assertEqual((output / snapshot["nativeSourceByteFiles"][digest]).read_bytes(),
                                 b"#define SYSTEM_VALUE 19\n")
                self.assertGreaterEqual(verify_native_source_bytes(snapshot, output / "native-source-bytes")["verifiedFiles"], 2)

    @unittest.skipUnless(sys.platform == "linux" and shutil.which("cc"), "Requires the disposable Linux assembler")
    def test_plain_and_preprocessed_assembly_preserve_includes_and_binary_constants(self):
        for suffix in (".s", ".S"):
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary).resolve()
                source = root / ("source" + suffix)
                prefix = '#include "value.h"\n' if suffix == ".S" else ""
                source.write_text(prefix + '.include "constants.inc"\n.text\n.globl example\nexample:\n'
                                  'mov $EXAMPLE_VALUE, %eax\nret\n.section .rodata\n.incbin "literal.bin"\n'
                                  '.section .note.GNU-stack,"",@progbits\n')
                header = root / "value.h"
                header.write_bytes(b"#define VALUE 3\n")
                include = root / "constants.inc"
                include.write_bytes(b".equ EXAMPLE_VALUE, 7\n")
                binary = root / "literal.bin"
                binary.write_bytes(b"Original synthetic binary constant\0")
                expected = {str(path): path.read_bytes() for path in (source, include, binary)}
                if suffix == ".S":
                    expected[str(header)] = header.read_bytes()
                output = root / "output"
                output.mkdir()
                subprocess.run([sys.executable, str(WRAPPER_PATH), "-c", str(source), "-o", str(root / "object.o")],
                               cwd=root, env={**os.environ, "PUFFINBOX_RUNTIME_PROBE_OUTPUT": str(output)},
                               check=True, capture_output=True)
                for name in expected:
                    Path(name).unlink()
                snapshot = capture_native(output / "native-compiler-traces", require_source_bytes=True)
                for name, content in expected.items():
                    value = snapshot["nativeSourceFiles"][name]
                    self.assertEqual((output / snapshot["nativeSourceByteFiles"][value]).read_bytes(), content)
                self.assertEqual(verify_native_source_bytes(snapshot, output / "native-source-bytes")["verifiedFiles"],
                                 len(set(snapshot["nativeSourceFiles"].values())))


if __name__ == "__main__":
    unittest.main()
