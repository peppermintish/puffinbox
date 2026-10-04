#!/usr/bin/env python3
"""Verify reviewed dependency replacements, source constraints and required notices."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def validate_feature_constraint(metadata: dict, constraint: dict) -> bool:
    name = constraint["name"]
    selected = [package for package in metadata["packages"] if package["name"] == name]
    if not selected and constraint.get("optional") is True:
        return False
    if len(selected) != 1:
        raise ValueError(f"Expected exactly one feature-constrained {name} package.")
    package = selected[0]
    if package["version"] != constraint["version"] or package.get("source") != constraint["source"]:
        raise ValueError(f"The feature exclusion requires a new source review for {name}.")
    nodes = [node for node in metadata["resolve"]["nodes"] if node["id"] == package["id"]]
    if len(nodes) != 1:
        raise ValueError(f"Missing resolved feature selection for {name}.")
    features = nodes[0].get("features")
    if not isinstance(features, list) or any(not isinstance(feature, str) for feature in features):
        raise ValueError(f"Invalid resolved feature selection for {name}.")
    unexpected = set(features) - set(constraint["allowedFeatures"])
    if unexpected:
        raise ValueError(f"Unreviewed dependency features for {name}: {', '.join(sorted(unexpected))}.")
    missing = set(constraint.get("requiredFeatures", [])) - set(features)
    if missing:
        raise ValueError(f"Required dependency features for {name}: {', '.join(sorted(missing))}.")
    manifest = Path(package["manifest_path"])
    directory = manifest.parent
    if not manifest.is_file() or manifest.is_symlink() or directory.is_symlink():
        raise ValueError(f"Invalid feature-constrained dependency path for {name}.")
    directory = directory.resolve()
    reviewed = constraint["reviewedConfigFiles"]
    if not reviewed:
        raise ValueError(f"No feature exclusion source hashes for {name}.")
    for name_in_package, expected in reviewed.items():
        relative = Path(name_in_package)
        source = directory / relative
        if (relative.is_absolute() or ".." in relative.parts or source.is_symlink()
                or not source.resolve().is_relative_to(directory)
                or hashlib.sha256(source.read_bytes()).hexdigest() != expected):
            raise ValueError(f"Unreviewed feature exclusion source: {name}/{name_in_package}.")
    return True


def validate(root: Path, metadata: dict, review: dict) -> dict:
    root = root.resolve()
    replacements = review["replacements"]
    if not replacements:
        raise ValueError("No reviewed dependency replacements were supplied.")
    for replacement in replacements:
        name, version = replacement["name"], replacement["version"]
        relative = Path(replacement["path"])
        directory = root / relative
        if relative.is_absolute() or ".." in relative.parts or directory.is_symlink():
            raise ValueError(f"Invalid reviewed dependency path for {name}.")
        directory = directory.resolve()
        if not directory.is_relative_to(root / "vendor"):
            raise ValueError(f"Reviewed dependency {name} must be inside vendor.")
        selected = [package for package in metadata["packages"] if package["name"] == name]
        if len(selected) != 1:
            raise ValueError(f"Expected exactly one selected {name} package.")
        package = selected[0]
        manifest = Path(package["manifest_path"])
        if (package["version"] != version or package.get("source") is not None
                or manifest.is_symlink() or manifest.resolve() != directory / "Cargo.toml"):
            raise ValueError(f"Cargo did not select the reviewed {name} {version} replacement.")
        reviewed = replacement["reviewedFiles"]
        actual = {path.relative_to(directory).as_posix(): path for path in directory.rglob("*")
                  if path.is_file() or path.is_symlink()}
        if not reviewed or actual.keys() != reviewed.keys():
            raise ValueError(f"The retained file inventory changed for {name}.")
        for path, expected in reviewed.items():
            source = actual[path]
            if (source.is_symlink() or not source.resolve().is_relative_to(directory)
                    or hashlib.sha256(source.read_bytes()).hexdigest() != expected):
                raise ValueError(f"Unreviewed dependency source: {name}/{path}.")
    result = {"verifiedReplacements": len(replacements), "licenseClearance": False}
    constraints = review.get("featureConstraints", [])
    if constraints:
        result["verifiedFeatureConstraints"] = sum(
            validate_feature_constraint(metadata, constraint) for constraint in constraints
        )
    notices = review.get("requiredNotices", [])
    if notices:
        for notice in notices:
            relative = Path(notice["path"])
            source = root / relative
            if (relative.is_absolute() or ".." in relative.parts
                    or relative.parts[:2] != ("vendor", "notices")
                    or any((root / Path(*relative.parts[:index])).is_symlink()
                           for index in range(1, len(relative.parts) + 1))
                    or not source.resolve().is_relative_to(root / "vendor" / "notices")
                    or not source.is_file()
                    or hashlib.sha256(source.read_bytes()).hexdigest() != notice["sha256"]):
                raise ValueError(f"Missing or unreviewed required dependency notice: {notice['path']}.")
        result["verifiedRequiredNotices"] = len(notices)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default="x86_64-unknown-linux-musl")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    review = json.loads((root / "vendor" / "dependency-replacements.json").read_text(encoding="utf-8"))
    completed = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", args.target],
        cwd=root, text=True, encoding="utf-8", capture_output=True, check=True,
    )
    print(json.dumps(validate(root, json.loads(completed.stdout), review)))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"Dependency replacement check failed: {error}") from error
