import copy
import hashlib
from pathlib import Path
import tempfile
import unittest

from check_dependency_replacements import validate


class DependencyReplacementTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.directory = self.root / "vendor/channel"
        self.directory.mkdir(parents=True)
        files = {"Cargo.toml": b'[package]\nname="channel"\nversion="1.0.0"\nlicense="MIT"\n',
                 "queue.rs": b"// Original MIT queue fixture\n"}
        for name, data in files.items():
            (self.directory / name).write_bytes(data)
        self.review = {"replacements": [{"name": "channel", "version": "1.0.0",
                                        "path": "vendor/channel", "reviewedFiles": {
                                            name: hashlib.sha256(data).hexdigest()
                                            for name, data in files.items()}}]}
        self.metadata = {"packages": [{"name": "channel", "version": "1.0.0", "source": None,
                                      "manifest_path": str(self.directory / "Cargo.toml")}]}

    def test_reviewed_local_package_passes_without_whole_binary_clearance(self):
        self.assertEqual(validate(self.root, self.metadata, self.review),
                         {"verifiedReplacements": 1, "licenseClearance": False})

    def test_registry_package_is_rejected_despite_allowed_package_license(self):
        metadata = copy.deepcopy(self.metadata)
        metadata["packages"][0].update(source="registry+crates.io", license="MIT OR Apache-2.0")
        with self.assertRaisesRegex(ValueError, "did not select"):
            validate(self.root, metadata, self.review)

    def test_restored_queue_notice_is_rejected(self):
        (self.directory / "queue.rs").write_bytes(b"// Synthetic excluded BSD queue fixture\n")
        with self.assertRaisesRegex(ValueError, "Unreviewed dependency source"):
            validate(self.root, self.metadata, self.review)

    def test_added_and_missing_sources_are_rejected(self):
        extra = self.directory / "extra.rs"
        extra.write_bytes(b"// New unreviewed source\n")
        with self.assertRaisesRegex(ValueError, "inventory changed"):
            validate(self.root, self.metadata, self.review)
        extra.unlink()
        (self.directory / "queue.rs").unlink()
        with self.assertRaisesRegex(ValueError, "inventory changed"):
            validate(self.root, self.metadata, self.review)

    def test_version_change_requires_a_new_review(self):
        metadata = copy.deepcopy(self.metadata)
        metadata["packages"][0]["version"] = "1.0.1"
        with self.assertRaisesRegex(ValueError, "did not select"):
            validate(self.root, metadata, self.review)

    def test_foreign_local_manifest_and_duplicate_package_are_rejected(self):
        metadata = copy.deepcopy(self.metadata)
        metadata["packages"][0]["manifest_path"] = str(self.root / "another/Cargo.toml")
        with self.assertRaisesRegex(ValueError, "did not select"):
            validate(self.root, metadata, self.review)
        metadata = copy.deepcopy(self.metadata)
        metadata["packages"].append(copy.deepcopy(metadata["packages"][0]))
        with self.assertRaisesRegex(ValueError, "exactly one"):
            validate(self.root, metadata, self.review)

    def test_review_path_cannot_escape_vendor(self):
        for path in ["../outside", "channel", str(self.directory)]:
            review = copy.deepcopy(self.review)
            review["replacements"][0]["path"] = path
            with self.assertRaises(ValueError):
                validate(self.root, self.metadata, review)


if __name__ == "__main__":
    unittest.main()
