import copy
import unittest

from check_gnu_shared_tls import inventory


ELF = """Dynamic section:
 0x0000000000000001 (NEEDED) Shared library: [libssl.so.3]
 0x0000000000000001 (NEEDED) Shared library: [libcrypto.so.3]
 0x0000000000000001 (NEEDED) Shared library: [libc.so.6]
"""
SYMBOLS = "\n".join("                 U " + name + "@OPENSSL_3.0.0"
                    for name in ("SSL_new", "SSL_free", "SSL_get_error", "OPENSSL_init_ssl", "X509_free"))
INPUTS = {"allInputObjects": ["/probe/entry.o", "/probe/libpuffinbox.rlib(module.o)"],
          "knownRuntimeInputsAbsent": True}


class SharedTlsInventoryTests(unittest.TestCase):
    def test_versioned_external_imports_pass_without_license_clearance(self):
        result = inventory(ELF, SYMBOLS, INPUTS)
        self.assertTrue(result["externalOpenSsl3Verified"])
        self.assertIn("libssl.so.3", result["neededLibraries"])
        self.assertIn("SSL_new", result["nativeTlsImports"])
        self.assertFalse(result["licenseClearance"])

    def test_missing_or_different_abi_dependencies_fail(self):
        for content in ("", ELF.replace("libssl.so.3", "libssl.so.1.1"), ELF.replace("libcrypto.so.3", "other.so")):
            with self.subTest(content=content), self.assertRaisesRegex(ValueError, "external OpenSSL 3"):
                inventory(content, SYMBOLS, INPUTS)

    def test_static_tls_archive_inputs_fail_even_with_external_dependencies(self):
        for name in ("libssl.a", "libcrypto.a"):
            inputs = copy.deepcopy(INPUTS)
            inputs["allInputObjects"].append("/probe/native/" + name + "(crypto.o)")
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "static OpenSSL"):
                inventory(ELF, SYMBOLS, inputs)

    def test_defined_native_tls_code_or_data_fail(self):
        for kind in ("T", "t", "W", "R", "D"):
            with self.subTest(kind=kind), self.assertRaisesRegex(ValueError, "defines native OpenSSL"):
                inventory(ELF, SYMBOLS + "\n0000000000002000 " + kind + " EVP_DigestInit", INPUTS)

    def test_expected_imports_and_passing_link_inventory_are_required(self):
        with self.assertRaisesRegex(ValueError, "expected external TLS operations"):
            inventory(ELF, SYMBOLS.replace("SSL_new", "different_operation"), INPUTS)
        for inputs in ({}, {"allInputObjects": [], "knownRuntimeInputsAbsent": True},
                       {**INPUTS, "knownRuntimeInputsAbsent": False}):
            with self.subTest(inputs=inputs), self.assertRaisesRegex(ValueError, "passing link-input"):
                inventory(ELF, SYMBOLS, inputs)
