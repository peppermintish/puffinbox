# Licensing inventory

Puffinbox source is MIT OR Apache-2.0. The full texts are in [LICENSE-MIT](../LICENSE-MIT) and [LICENSE-APACHE](../LICENSE-APACHE). Third-party components keep their original licenses.

## Cargo dependencies

The locked Linux Cargo graph now passes the strict policy in [deny.toml](../deny.toml). Only MIT and Apache-2.0 are allowed; no license exceptions were added. `scripts/build_license_bundle.py` also generated full dependency notices successfully.

```sh
cargo deny --locked check
python3 scripts/build_license_bundle.py
```

The graph changed as follows:

| Previous dependency | Replacement or adjustment |
| --- | --- |
| Rustls, ring, webpki, untrusted, and bundled webpki roots | Reqwest and SQLx use native TLS with statically built OpenSSL 3. The operator supplies the CA bundle. Certificate-chain and hostname checks remain enabled and have source/container acceptance. |
| subtle | ctutils; a small MIT/Apache digest patch preserves HMAC verification. Argon2 was updated to 0.6, with old password-hash and new random-salt regressions. |
| foldhash | The vendored hashbrown default uses Rust's randomized standard hasher. Hashlink was adjusted for its non-Copy builder. |
| matchit | An original segment-tree router implementing the public interface consumed by Axum. |
| unicode-ident | An original ASCII identifier adapter for build macros. |
| idna_mapping | The upstream ASCII-only idna_adapter 1.0 backend. Configured URL hostnames must be ASCII or explicitly Punycode encoded. Unicode media text remains supported. |

Vendored MIT/Apache sources retain their upstream notices. Exact tarball checksums and patch descriptions are in [vendor/upstream.json](../vendor/upstream.json) and [vendor/README.md](../vendor/README.md). The adapters are original code in [crates/](../crates). The dependency regressions are in [tests/dependency_behavior.rs](../tests/dependency_behavior.rs).

## Linked runtime blocker

Cargo auditing does not cover the prebuilt Rust standard library or libc. The current unstripped static server contains `core::unicode::unicode_data` symbols for alphabetic, whitespace, case conversion, and related tables. Rust's `COPYRIGHT-library.html` assigns Unicode-3.0 to `library/core/src/unicode` data. That is outside the requested MIT/Apache-2.0 boundary.

Musl's copyright record also includes components under BSD, ISC, and other terms alongside its main MIT license. The exact linked subset needs an audit or replacement. Keeping notices does not turn those components into MIT/Apache code. The build bundle retains the Rust and musl notices, but the whole binary/image is not claimed to meet the requested boundary.

Release packaging remains blocked in [release-gates.json](release-gates.json). Validation images can be built locally; no release image or archive has been published.

## Browser assets

| Asset | Source and license |
| --- | --- |
| hls.js 1.7.3 | Official npm distribution, Apache-2.0. [Provenance and checksums](../web/vendor/README.md). |
| PDF.js 6.3.289 display and worker | Apache-2.0. [Bundle record](../web/vendor/pdfjs/README.md). |
| PDF.js qcms decoder | MIT, with upstream notices retained in the bundle. |

Adobe CMaps, Foxit base fonts, and JBIG2/OpenJPEG decoders are excluded because their licenses fall outside the allowlist. This limits affected PDFs. Assets are served locally. Full notices are in [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).

## External components

| Component | Distribution boundary |
| --- | --- |
| PostgreSQL 18.6 | A separate Compose service, not copied into the server image. PostgreSQL has its own license and the Alpine image has other package licenses. The earlier [53-entry APK inventory](postgres-image-apk-inventory.tsv) is partial and excludes the separately installed PostgreSQL server. |
| Operator FFmpeg/ffprobe | Mounted static tools or a separately assembled dynamic runtime. Licenses depend on the selected build and linked codecs. They are excluded from the default server image. |
| Acceptance FFmpeg | External Ubuntu test image, FFmpeg 8.0.1-3ubuntu2 with GPL enabled. This combined test runtime is not a permissive-only project release. |
| Builder and CI tools | Rust builder, audit tools, Linux packages, and pinned GitHub Actions are test/build infrastructure. Their full inventories are separate from the final image audit. |
| TVmaze metadata | Optional remote data under [TVmaze's CC BY-SA terms](https://www.tvmaze.com/api#licensing). Stored attribution fields do not establish end-user attribution compliance. |

External image inventories and operator obligations remain incomplete. A passing Cargo audit establishes the Cargo graph's policy result, not whole-deployment license closure.
