#!/usr/bin/env python3
"""Verify project source restrictions and the selected Linux TLS dependency graph."""

from __future__ import annotations

import json
import os
import hashlib
from pathlib import Path
import subprocess
import tomllib


ROOT = Path(__file__).resolve().parents[1]
TARGETS = ("x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu")
OPENSSL_PACKAGES = frozenset({
    "openssl", "openssl-sys", "openssl-src", "native-tls", "hyper-tls", "tokio-native-tls",
})


def check_postgres_patch() -> None:
    package = ROOT / "vendor" / "sqlx-postgres"
    provenance = json.loads((package / "provenance.json").read_text(encoding="utf-8"))
    if provenance["crate"] != "sqlx-postgres" or provenance["version"] != "0.8.6":
        raise SystemExit("The PostgreSQL patch version needs review.")
    changed = provenance["changedFiles"]
    if set(changed) != {"src/lib.rs", "src/connection/sasl.rs"}:
        raise SystemExit("Unexpected PostgreSQL patch scope.")
    upstream = provenance["upstreamFiles"]
    for name, expected in upstream.items():
        path = package / name
        if not path.resolve().is_relative_to(package.resolve()):
            raise SystemExit("Invalid PostgreSQL provenance path.")
        if name in changed:
            if changed[name]["upstreamSha256"] != expected:
                raise SystemExit(f"PostgreSQL upstream hash mismatch: {name}")
            expected = changed[name]["patchedSha256"]
        if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != expected:
            raise SystemExit(f"PostgreSQL package differs from reviewed source: {name}")
    allowed = set(upstream) | {"provenance.json", "PUFFINBOX.md"}
    extra = {path.relative_to(package).as_posix() for path in package.rglob("*") if path.is_file()} - allowed
    if extra:
        raise SystemExit(f"Unexpected PostgreSQL package files: {', '.join(sorted(extra))}")
    if "#![forbid(unsafe_code)]" not in (package / "src/lib.rs").read_text(encoding="utf-8"):
        raise SystemExit("The local PostgreSQL package must forbid unsafe Rust.")
    print("PASS: PostgreSQL patch and upstream notices match reviewed hashes")


def main() -> int:
    check_postgres_patch()
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    if manifest.get("lints", {}).get("rust", {}).get("unsafe_code") != "forbid":
        raise SystemExit("All project Cargo targets must forbid unsafe Rust.")
    for entry in ("src/lib.rs", "src/main.rs"):
        if "#![forbid(unsafe_code)]" not in (ROOT / entry).read_text(encoding="utf-8"):
            raise SystemExit(f"Missing unsafe-code prohibition in {entry}.")

    # External browser distributions and registry dependencies retain their
    # upstream implementation. Generated output and private test evidence are
    # outside project source. Check all remaining repository source locations.
    excluded = {".git", ".local", "target", "dist", "vendor"}
    native = {".c", ".h", ".cc", ".cpp", ".cxx", ".hpp", ".s"}
    for directory, directories, files in os.walk(ROOT):
        directories[:] = [name for name in directories if name not in excluded]
        for filename in files:
            path = Path(directory) / filename
            if path.suffix.lower() in native:
                raise SystemExit(f"Project native source must be removed: {path.relative_to(ROOT).as_posix()}")

    for target in TARGETS:
        result = subprocess.run(
            ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
            cwd=ROOT, check=True, text=True, encoding="utf-8", capture_output=True,
        )
        metadata = json.loads(result.stdout)
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        packages = {package["id"]: package for package in metadata["packages"]}
        pending = list(metadata["workspace_members"])
        selected = set()
        while pending:
            package_id = pending.pop()
            if package_id in selected:
                continue
            selected.add(package_id)
            pending.extend(dependency["pkg"] for dependency in nodes[package_id]["deps"])
        names = {packages[package_id]["name"] for package_id in selected}
        forbidden = sorted(names & OPENSSL_PACKAGES)
        if forbidden:
            raise SystemExit(f"OpenSSL TLS dependencies selected for {target}: {', '.join(forbidden)}")
        if "rustls" not in names:
            raise SystemExit(f"No rustls provider selected for {target}.")
        # openssl-probe only discovers certificate paths; it does not link or
        # implement OpenSSL and is used by rustls-native-certs.
        print(f"PASS: {target}: rustls selected; no OpenSSL TLS packages")
    print("PASS: project source has no C/assembly; unsafe Rust is forbidden for every Cargo target")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
