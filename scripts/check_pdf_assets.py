#!/usr/bin/env python3
"""Check the reviewed PDF.js distribution against its upstream file hashes."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path, PurePosixPath


VERSION = "6.3.289"
INTEGRITY = "sha512-ZHjSVpDa3D6izMq8/04lvkhkATUmL9px6ChPaXc1k6nU2Mrhlg1/7F0bdUqCwUjw3NsPTfPZsMDUU6ZIcRaeQw=="
LICENSES = {
    "LICENSE": "Apache-2.0",
    "cmaps/LICENSE": "BSD-3-Clause",
    "standard_fonts/LICENSE_FOXIT": "BSD-3-Clause",
    "wasm/LICENSE_JBIG2": "BSD-3-Clause AND Apache-2.0",
    "wasm/LICENSE_PDFJS_JBIG2": "Apache-2.0",
    "wasm/LICENSE_OPENJPEG": "BSD-2-Clause",
    "wasm/LICENSE_PDFJS_OPENJPEG": "BSD-2-Clause",
    "wasm/LICENSE_QCMS": "MIT",
    "wasm/LICENSE_PDFJS_QCMS": "MIT",
}


def check(root: Path) -> dict:
    vendor = root / "web" / "vendor" / "pdfjs"
    manifest = json.loads((vendor / "provenance.json").read_text(encoding="utf-8"))
    if (manifest["package"], manifest["version"], manifest["integrity"]) != ("pdfjs-dist", VERSION, INTEGRITY):
        raise ValueError("PDF.js package provenance differs from the reviewed release")
    if manifest["registryTarball"] != f"https://registry.npmjs.org/pdfjs-dist/-/pdfjs-dist-{VERSION}.tgz":
        raise ValueError("PDF.js package source differs from the reviewed registry archive")
    if manifest["licenseFiles"] != LICENSES or not set(LICENSES).issubset(manifest["files"]):
        raise ValueError("PDF.js license notices differ from the reviewed distribution")
    files = manifest["files"]
    if len(files) != 194:
        raise ValueError("PDF.js asset selection differs from the reviewed distribution")
    for name, expected in files.items():
        relative = PurePosixPath(name)
        if relative.is_absolute() or ".." in relative.parts or "\\" in name:
            raise ValueError(f"Unsafe PDF.js manifest path: {name}")
        source = vendor / name
        if source.is_symlink() or not source.is_file() or not source.resolve().is_relative_to(vendor.resolve()):
            raise ValueError(f"Missing or unsafe PDF.js asset: {name}")
        if "liberation" in name.lower() or "quickjs" in name.lower():
            raise ValueError(f"Unreviewed PDF.js asset: {name}")
        if hashlib.sha256(source.read_bytes()).hexdigest() != expected:
            raise ValueError(f"PDF.js asset differs from upstream bytes: {name}")
    actual = {file.relative_to(vendor).as_posix() for file in vendor.rglob("*") if file.is_file()}
    if actual != set(files) | {"README.md", "provenance.json"}:
        raise ValueError(f"Unreviewed PDF.js files: {sorted(actual ^ (set(files) | {'README.md', 'provenance.json'}))}")
    return manifest


if __name__ == "__main__":
    check(Path(__file__).resolve().parents[1])
    print("Verified 194 upstream PDF.js files and nine compatible license notices")
