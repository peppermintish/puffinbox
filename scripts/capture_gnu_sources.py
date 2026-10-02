#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Capture dependency and generated source bytes in the disposable GNU build."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import tarfile
import tomllib


GENERATED_SUFFIXES = {".rs", ".c", ".h", ".cc", ".cpp", ".s", ".S", ".inc", ".asm"}
EXCLUDED_DIRECTORIES = {".git", ".local", "target", "dist", "node_modules", "__pycache__"}


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def files_below(root: Path, *, exclude_build_directories: bool = False):
    for directory_name, directories, files in os.walk(root, followlinks=False):
        directory = Path(directory_name)
        directories[:] = sorted(name for name in directories if (not exclude_build_directories or name not in EXCLUDED_DIRECTORIES)
                                and not (directory / name).is_symlink())
        for name in sorted(files):
            path = directory / name
            if path.is_symlink():
                raise ValueError(f"Source snapshot cannot follow a symlink: {path}")
            if path.is_file():
                yield path


def verify_archive(package: dict, root: Path, package_files: dict[str, str], checksums: dict) -> dict:
    key = (package["name"], package["version"], package["source"])
    expected = checksums.get(key)
    if not expected or not re.fullmatch(r"[0-9a-f]{64}", expected):
        raise ValueError("Registry package has no exact lockfile checksum.")
    name = package["name"] + "-" + package["version"]
    if root.name != name or root.parent.parent.name != "src":
        raise ValueError("Unsupported Cargo registry source-cache layout.")
    archive = root.parents[2] / "cache" / root.parent.name / (name + ".crate")
    if not archive.is_file() or archive.is_symlink() or digest(archive) != expected:
        raise ValueError("Cached registry archive is missing or differs from its lockfile checksum.")
    members = set()
    with tarfile.open(archive, mode="r|*") as stream:
        for member in stream:
            relative = Path(member.name)
            if relative.is_absolute() or ".." in relative.parts or not relative.parts or relative.parts[0] != name:
                raise ValueError("Unsafe registry archive path.")
            if member.isdir():
                continue
            if not member.isfile() or len(relative.parts) < 2 or member.name in members:
                raise ValueError("Unsupported or duplicate registry archive member.")
            members.add(member.name)
            path = root.joinpath(*relative.parts[1:])
            with stream.extractfile(member) as source:
                value = hashlib.file_digest(source, "sha256").hexdigest()
            if package_files.get(str(path)) != value:
                raise ValueError(f"Registry source differs from its locked archive: {path}")
    available = {str(Path(path).relative_to(root)) for path in package_files if path != str(root / ".cargo-ok")}
    archived = {str(Path(name).relative_to(name.split("/", 1)[0])) for name in members}
    if not members or available != archived:
        raise ValueError("Registry snapshot contains missing or additional files outside its locked archive.")
    return {"registryArchiveVerified": True, "registryArchivePath": str(archive), "registryArchiveSha256": expected}


def capture_native(traces: Path | None, *, require_compilations: bool = True) -> dict:
    hashes = {}
    inputs = {}
    invocations = 0
    if traces is not None:
        for path in sorted(traces.glob("*.json")):
            record = json.loads(path.read_text())
            if record.get("licenseClearance") is not False:
                raise ValueError("Unsupported native compiler trace.")
            hashes[str(path)] = digest(path)
            if record["exitCode"] == 0 and record["compileSource"]:
                invocations += 1
                for source, value in record["sourceFiles"].items():
                    if not Path(source).is_absolute() or ".." in Path(source).parts or not re.fullmatch(r"[0-9a-f]{64}", value):
                        raise ValueError("Unsafe native compiler source path or hash.")
                    inputs.setdefault(source, set()).add(value)
        if not hashes or (require_compilations and not invocations):
            raise ValueError("Native compiler source traces are missing or contain no successful compilation.")
    return {"nativeCompilerTraceHashes": hashes, "nativeCompileInvocations": invocations,
            "nativeSourceFiles": {path: next(iter(values)) for path, values in inputs.items() if len(values) == 1},
            "ambiguousNativeSourceFiles": {path: sorted(values) for path, values in inputs.items() if len(values) != 1}}


def capture(metadata: dict, generated: Path, lockfile: Path, native_traces: Path | None = None,
            *, require_native_compilations: bool = True) -> dict:
    locked = tomllib.loads(lockfile.read_text())
    checksums = {}
    for package in locked["package"]:
        if package.get("source", "").startswith("registry+"):
            key = (package["name"], package["version"], package["source"])
            if key in checksums:
                raise ValueError("Duplicate registry package in the lockfile.")
            checksums[key] = package.get("checksum")
    packages = []
    sources = {}
    for package in sorted(metadata["packages"], key=lambda item: item["id"]):
        manifest = Path(package["manifest_path"])
        if not manifest.is_absolute() or manifest.name != "Cargo.toml":
            raise ValueError("Expected an absolute Cargo package manifest path.")
        root = manifest.parent
        if root.resolve() != root:
            raise ValueError("Package source roots must not contain symlinks.")
        registry = str(package.get("source") or "").startswith("registry+")
        package_files = {str(path): digest(path) for path in files_below(root, exclude_build_directories=not registry)}
        if str(manifest) not in package_files:
            raise ValueError("Package manifest is absent from its source snapshot.")
        archive_record = verify_archive(package, root, package_files, checksums) if registry else {"registryArchiveVerified": False}
        notices = {path: value for path, value in package_files.items()
                   if any(word in Path(path).name.upper() for word in ("LICENSE", "COPYRIGHT", "NOTICE"))}
        license_file = package.get("license_file")
        if license_file:
            path = Path(license_file)
            if not path.is_absolute():
                path = root / path
            if not path.is_relative_to(root) or ".." in path.parts or str(path) not in package_files:
                raise ValueError("Declared license file is outside its package snapshot or missing.")
            notices[str(path)] = package_files[str(path)]
        packages.append({"id": package["id"], "name": package["name"], "version": package["version"],
                         "declaredLicense": package.get("license"), "sourceRoot": str(root),
                         "manifestPath": str(manifest), "manifestSha256": package_files[str(manifest)],
                         "noticeFiles": notices, **archive_record,
                         "sourceFileCount": len(package_files)})
        for path, value in package_files.items():
            if path in sources and sources[path] != value:
                raise ValueError("Overlapping source snapshots changed during capture.")
            sources[path] = value
    if not packages:
        raise ValueError("Cargo returned no packages to inventory.")
    if not generated.is_absolute() or not generated.is_dir() or generated.resolve() != generated:
        raise ValueError("Expected an existing absolute generated-source directory without symlinks.")
    generated_sources = {str(path): digest(path) for path in files_below(generated)
                         if path.suffix in GENERATED_SUFFIXES}
    if not generated_sources:
        raise ValueError("The native/generated build source snapshot is empty.")
    return {"schemaVersion": 1, "packages": packages, "sourceFiles": sources,
            "generatedSourceRoot": str(generated), "generatedSourceFiles": generated_sources,
            "lockfileSha256": digest(lockfile),
            **capture_native(native_traces, require_compilations=require_native_compilations), "licenseClearance": False,
            "scope": "Source bytes available after the disposable build, package declarations, and exact notice hashes. "
                     "Archive and file verification binds registry bytes to the lockfile's package checksums. This snapshot does "
                     "not prove which headers, constants, or generated inputs were retained, or clear file exceptions."}


def mapped_dependencies(inventory: dict, record: dict) -> dict:
    from check_gnu_source_map import LIBRARY_MARKER

    if record.get("schemaVersion") != 1 or record.get("licenseClearance") is not False:
        raise ValueError("Unsupported dependency source snapshot.")
    equivalent_files = {}
    for field in ("sourceFiles", "generatedSourceFiles", "nativeSourceFiles"):
        for name, value in record[field].items():
            path = Path(name)
            if not path.is_absolute() or ".." in path.parts or not re.fullmatch(r"[0-9a-f]{64}", value):
                raise ValueError("Unsafe source path or invalid source hash.")
            if field == "sourceFiles":
                equivalent_files.setdefault(value, []).append(name)
    packages = record["packages"]
    generated_root = Path(record["generatedSourceRoot"])
    if not generated_root.is_absolute() or ".." in generated_root.parts:
        raise ValueError("Unsafe generated-source root.")
    roots = set()
    for package in packages:
        root = Path(package["sourceRoot"])
        if not root.is_absolute() or ".." in root.parts or str(root) in roots:
            raise ValueError("Unsafe or duplicate package source root.")
        roots.add(str(root))
        manifest = str(root / "Cargo.toml")
        if package["manifestPath"] != manifest or record["sourceFiles"].get(manifest) != package["manifestSha256"]:
            raise ValueError("Package manifest hash is missing or inconsistent.")
        for path, value in package["noticeFiles"].items():
            if not Path(path).is_relative_to(root) or ".." in Path(path).parts or record["sourceFiles"].get(path) != value:
                raise ValueError("Package notice hash is outside its source snapshot or inconsistent.")
    if not packages:
        raise ValueError("Dependency source snapshot has no packages.")
    mapped = {}
    missing = []
    for name, intervals in inventory["allMappedSources"].items():
        if LIBRARY_MARKER in name:
            continue
        path = Path(name)
        if not path.is_absolute() or ".." in path.parts:
            raise ValueError("Unsafe mapped dependency source path.")
        candidates = [package for package in packages if path.is_relative_to(Path(package["sourceRoot"]))]
        package = max(candidates, key=lambda item: len(Path(item["sourceRoot"]).parts), default=None)
        source_hash = record["sourceFiles"].get(name)
        generated_hash = record["generatedSourceFiles"].get(name)
        native_hash = record["nativeSourceFiles"].get(name)
        if name in record["ambiguousNativeSourceFiles"]:
            raise ValueError("Mapped native source changed between compiler invocations: " + name)
        available = {value for value in (source_hash, generated_hash, native_hash) if value}
        if len(available) > 1:
            raise ValueError("Mapped native source differs from its final source snapshot: " + name)
        if source_hash and package:
            mapped[name] = {**intervals, "sha256": source_hash, "sourceKind": "package",
                            "packageId": package["id"], "declaredLicense": package["declaredLicense"],
                            "manifestSha256": package["manifestSha256"], "noticeFiles": package["noticeFiles"]}
        elif native_hash or (generated_hash and path.is_relative_to(generated_root)):
            # Byte equality identifies candidates, not the generator or license.
            value = native_hash or generated_hash
            mapped[name] = {**intervals, "sha256": value, "sourceKind": "native-compiler" if native_hash else "generated",
                            "byteIdenticalPackageFiles": equivalent_files.get(value, []),
                            "licenseReviewRequired": True}
        else:
            missing.append(name)
    if missing:
        raise ValueError("Mapped dependency sources lack exact snapshots: " + ", ".join(missing))
    return mapped
