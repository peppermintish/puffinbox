#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Check that an experimental GNU executable uses external OpenSSL 3 libraries."""

from __future__ import annotations

from pathlib import Path
import re


TLS_NAMES = re.compile(r"^(?:SSL_|OPENSSL_|CRYPTO_|EVP_|BIO_|BN_|ERR_|X509_|RSA_|EC_|ASN1_)")


def inventory(elf_text: str, symbols_text: str, link_inputs: dict) -> dict:
    needed = sorted(set(re.findall(r"\(NEEDED\)\s+Shared library: \[([^\]]+)\]", elf_text)))
    if not {"libssl.so.3", "libcrypto.so.3"}.issubset(needed):
        raise ValueError("Expected external OpenSSL 3 SSL and crypto dependencies.")
    inputs = link_inputs.get("allInputObjects")
    if not inputs or not link_inputs.get("knownRuntimeInputsAbsent"):
        raise ValueError("External TLS requires a complete passing link-input inventory.")
    archives = [path for path in inputs if Path(path.split("(", 1)[0]).name in {"libssl.a", "libcrypto.a"}]
    if archives:
        raise ValueError("The executable retains static OpenSSL archive inputs.")
    imports = set()
    definitions = set()
    for line in symbols_text.splitlines():
        fields = line.split()
        if len(fields) < 2 or not TLS_NAMES.match(fields[-1]):
            continue
        name = fields[-1].split("@", 1)[0]
        if fields[-2] == "U":
            imports.add(name)
        elif len(fields) >= 3 and re.fullmatch(r"[0-9a-fA-F]+", fields[0]):
            definitions.add(name)
    if definitions:
        raise ValueError("The executable defines native OpenSSL symbols.")
    if not {"SSL_new", "SSL_free", "SSL_get_error", "OPENSSL_init_ssl"}.issubset(imports):
        raise ValueError("The executable does not import the expected external TLS operations.")
    return {"neededLibraries": needed, "nativeTlsImports": sorted(imports),
            "staticTlsArchiveInputs": archives, "nativeTlsDefinitions": sorted(definitions),
            "externalOpenSsl3Verified": True, "licenseClearance": False,
            "scope": "Dynamic dependency names, expected TLS imports, and absence of named static TLS archives "
                     "and native TLS definitions. Other source provenance and external-runtime distribution remain separate."}
