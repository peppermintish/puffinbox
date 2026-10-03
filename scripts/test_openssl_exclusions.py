import importlib.util
from pathlib import Path
import tomllib
import unittest

from check_openssl_exclusions import inventory


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("openssl_configure", ROOT / "scripts/openssl-configure.py")
WRAPPER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(WRAPPER)
BASELINE = "00000100 T SSL_new\n00000200 T SSL_free\n00000300 T OPENSSL_init_ssl\n"


class OpenSslExclusionTests(unittest.TestCase):
    def test_original_retained_implementation_and_provider_symbols_are_rejected(self):
        for name in ["SipHash_Final", "SipHash_Init", "SipHash_Update", "SipHash_hash_size",
                     "SipHash_set_hash_size", "ossl_siphash_functions",
                     "ossl_mac_legacy_siphash_signature_functions", "siphash_new", "mac_siphash_newctx"]:
            with self.subTest(symbol=name), self.assertRaisesRegex(ValueError, "retains"):
                inventory(BASELINE + f"00000400 T {name}\n")

    def test_stripped_or_external_tls_symbols_cannot_pass_the_bundled_check(self):
        for symbols in ["", "nm: no symbols", "U SSL_new\nU SSL_free\nU OPENSSL_init_ssl\n"]:
            with self.subTest(symbols=symbols), self.assertRaisesRegex(ValueError, "bundled OpenSSL"):
                inventory(symbols)

    def test_native_compile_or_link_inputs_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "link map"):
            inventory(BASELINE, "archive.rlib(libcrypto-lib-siphash.o):(.text.SipHash_Init)")
        with self.assertRaisesRegex(ValueError, "native source"):
            inventory(BASELINE, native_sources={"nativeSourceFiles": {
                "/build/src/crypto/siphash/siphash.c": "a" * 64}})

    def test_rust_siphash_names_and_unused_header_declarations_are_distinct(self):
        result = inventory(BASELINE + "00000400 T std::hash::SipHasher13::finish\n",
                           native_sources={"nativeSourceFiles": {"/build/src/include/crypto/siphash.h": "a" * 64}})
        self.assertTrue(result["siphashSymbolsAbsent"])
        self.assertFalse(result["licenseClearance"])

    def test_wrapper_rejects_reenable_and_other_perl_programs(self):
        for args in [[], ["-e", "print 1"], ["./Configure", "enable-siphash"], ["./Configure", "enable-quic"]]:
            with self.subTest(args=args), self.assertRaises(ValueError):
                WRAPPER.configure_arguments(args)
        self.assertEqual(WRAPPER.configure_arguments(["./Configure", "linux-x86_64", "--prefix=/path with spaces"]),
                         ["perl", "./Configure", "linux-x86_64", "--prefix=/path with spaces", "no-siphash", "no-quic"])

    def test_cargo_forces_the_reviewed_wrapper(self):
        config = tomllib.loads((ROOT / ".cargo/config.toml").read_text(encoding="utf-8"))
        self.assertEqual(config["env"]["OPENSSL_SRC_PERL"], {
            "value": "scripts/openssl-configure.py", "relative": True, "force": True})


if __name__ == "__main__":
    unittest.main()
