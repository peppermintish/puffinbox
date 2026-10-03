#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Reject retained SipHash code in an unstripped server with bundled OpenSSL."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


SIPHASH_SYMBOL = re.compile(r"^(?:SipHash_|ossl_siphash_|ossl_mac_legacy_siphash_|siphash_|mac_siphash_)")


def inventory(symbols: str, link_map: str = "", native_sources: dict | None = None) -> dict:
    selected = [line.split() for line in symbols.splitlines() if line.split()]
    defined = {parts[-1] for parts in selected if len(parts) >= 3 and parts[-2].upper() != "U"}
    if not {"SSL_new", "SSL_free", "OPENSSL_init_ssl"}.issubset(defined):
        raise ValueError("Expected unstripped server symbols with bundled OpenSSL.")
    if any(SIPHASH_SYMBOL.match(parts[-1]) for parts in selected):
        raise ValueError("The server retains OpenSSL SipHash symbols.")
    if re.search(r"\blib(?:crypto|default)-lib-siphash(?:_prov)?[.]o\b", link_map):
        raise ValueError("The link map includes an OpenSSL SipHash object.")
    if native_sources is not None and any(
        name.endswith("/crypto/siphash/siphash.c") for name in native_sources["nativeSourceFiles"]
    ):
        raise ValueError("The native source capture includes OpenSSL SipHash compilation.")
    return {"bundledOpenSslIdentified": True, "siphashSymbolsAbsent": True,
            "linkMapChecked": bool(link_map), "nativeSourcesChecked": native_sources is not None,
            "licenseClearance": False}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--map", type=Path)
    parser.add_argument("--native-sources", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    sections = subprocess.check_output(["readelf", "-SW", str(args.binary)], text=True)
    if ".symtab " not in sections:
        raise ValueError("The exclusion check requires a complete symbol table.")
    symbols = subprocess.check_output(["nm", str(args.binary)], text=True)
    result = inventory(symbols, args.map.read_text() if args.map else "",
                       json.loads(args.native_sources.read_text()) if args.native_sources else None)
    with args.binary.open("rb") as binary:
        result["binarySha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
    if args.output:
        with args.output.open("x") as output:
            json.dump(result, output, indent=2)
            output.write("\n")
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"OpenSSL exclusion check failed: {error}") from error
