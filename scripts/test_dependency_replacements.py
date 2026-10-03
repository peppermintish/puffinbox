import copy
import hashlib
import json
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


class DependencyFeatureTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        directory = self.root / "registry/regex-syntax"
        directory.mkdir(parents=True)
        source = b'// Table module fixture, gated by the unicode-case feature\n'
        (directory / "tables.rs").write_bytes(source)
        (directory / "Cargo.toml").write_bytes(b"// Registry package fixture\n")
        replacement = self.root / "vendor/channel"
        replacement.mkdir(parents=True)
        (replacement / "Cargo.toml").write_bytes(b"// Original package fixture\n")
        self.identifier = "registry+crates.io#regex-syntax@0.8.11"
        self.metadata = {"packages": [
            {"name":"channel", "version":"1.0.0", "source":None,
             "manifest_path":str(replacement / "Cargo.toml")},
            {"name":"regex-syntax", "version":"0.8.11", "source":"registry+crates.io",
             "id":self.identifier, "manifest_path":str(directory / "Cargo.toml")},
        ], "resolve":{"nodes":[{"id":self.identifier, "features":["std"]}]}}
        self.review = {"replacements":[{
            "name":"channel", "version":"1.0.0", "path":"vendor/channel",
            "reviewedFiles":{"Cargo.toml":hashlib.sha256((replacement / "Cargo.toml").read_bytes()).hexdigest()},
        }], "featureConstraints":[{
            "name":"regex-syntax", "version":"0.8.11", "source":"registry+crates.io",
            "optional":True, "allowedFeatures":["std"],
            "reviewedConfigFiles":{"tables.rs":hashlib.sha256(source).hexdigest()},
        }]}
        self.directory = directory

    def test_resolved_exclusion_passes_and_removed_dependency_is_safe(self):
        result = validate(self.root, self.metadata, self.review)
        self.assertEqual(result["verifiedFeatureConstraints"], 1)
        self.assertFalse(result["licenseClearance"])
        self.metadata["packages"].pop()
        self.metadata["resolve"]["nodes"].clear()
        self.assertEqual(validate(self.root, self.metadata, self.review)["verifiedFeatureConstraints"], 0)

    def test_each_unicode_feature_and_unknown_future_feature_is_rejected(self):
        for feature in ["default", "unicode", "unicode-age", "unicode-bool", "unicode-case",
                        "unicode-gencat", "unicode-perl", "unicode-script", "unicode-segment",
                        "future-table-feature"]:
            with self.subTest(feature=feature):
                self.metadata["resolve"]["nodes"][0]["features"] = ["std", feature]
                with self.assertRaisesRegex(ValueError, "Unreviewed dependency features"):
                    validate(self.root, self.metadata, self.review)

    def test_absent_duplicate_or_invalid_feature_selection_is_rejected(self):
        for nodes in [[], [self.metadata["resolve"]["nodes"][0]] * 2,
                      [{"id":self.identifier}], [{"id":self.identifier,"features":"std"}],
                      [{"id":self.identifier,"features":[1]}]]:
            with self.subTest(nodes=nodes):
                metadata = copy.deepcopy(self.metadata)
                metadata["resolve"]["nodes"] = nodes
                with self.assertRaisesRegex(ValueError, "feature selection"):
                    validate(self.root, metadata, self.review)

    def test_version_source_and_gate_changes_require_review(self):
        for key, value in [("version", "0.8.12"), ("source", None)]:
            metadata = copy.deepcopy(self.metadata)
            metadata["packages"][1][key] = value
            with self.assertRaisesRegex(ValueError, "new source review"):
                validate(self.root, metadata, self.review)
        (self.directory / "tables.rs").write_bytes(b"// Table module no longer gated\n")
        with self.assertRaisesRegex(ValueError, "Unreviewed feature exclusion source"):
            validate(self.root, self.metadata, self.review)

    def test_feature_source_paths_cannot_escape_the_selected_package(self):
        for path in ["../tables.rs", str(self.directory / "tables.rs")]:
            review = copy.deepcopy(self.review)
            digest = review["featureConstraints"][0]["reviewedConfigFiles"].pop("tables.rs")
            review["featureConstraints"][0]["reviewedConfigFiles"][path] = digest
            with self.assertRaisesRegex(ValueError, "Unreviewed feature exclusion source"):
                validate(self.root, self.metadata, review)

    def test_missing_selected_manifest_is_rejected(self):
        (self.directory / "Cargo.toml").unlink()
        with self.assertRaisesRegex(ValueError, "Invalid feature-constrained dependency path"):
            validate(self.root, self.metadata, self.review)

    def test_recorded_tower_selection_rejects_compression_and_full_features(self):
        record = json.loads((Path(__file__).resolve().parents[1]
                             / "vendor/dependency-replacements.json").read_text(encoding="utf-8"))
        constraint = copy.deepcopy(next(item for item in record["featureConstraints"]
                                        if item["name"] == "tower-http"))
        directory = self.root / "registry/tower-http"
        directory.mkdir()
        source = b"// Synthetic reviewed module gate fixture\n"
        for name in constraint["reviewedConfigFiles"]:
            path = directory / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(source)
            constraint["reviewedConfigFiles"][name] = hashlib.sha256(source).hexdigest()
        identifier = "registry+crates.io#tower-http@0.6.11"
        self.metadata["packages"].append({
            "name": "tower-http", "version": constraint["version"],
            "source": constraint["source"], "id": identifier,
            "manifest_path": str(directory / "Cargo.toml"),
        })
        node = {"id": identifier, "features": constraint["allowedFeatures"].copy()}
        self.metadata["resolve"]["nodes"].append(node)
        self.review["featureConstraints"].append(constraint)
        self.assertEqual(validate(self.root, self.metadata, self.review)["verifiedFeatureConstraints"], 2)
        for feature in ["compression-br", "compression-deflate", "compression-gzip",
                        "compression-zstd", "compression-full", "decompression-br",
                        "decompression-deflate", "decompression-gzip", "decompression-zstd",
                        "decompression-full", "full"]:
            with self.subTest(feature=feature):
                node["features"] = constraint["allowedFeatures"] + [feature]
                with self.assertRaisesRegex(ValueError, "Unreviewed dependency features for tower-http"):
                    validate(self.root, self.metadata, self.review)


if __name__ == "__main__":
    unittest.main()
