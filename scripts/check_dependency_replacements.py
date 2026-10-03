#!/usr/bin/env python3
"""Verify selected, reviewed replacements for known dependency file exceptions."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess


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
    return {"verifiedReplacements": len(replacements), "licenseClearance": False}


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
