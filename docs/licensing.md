# Licensing inventory

Puffinbox source is MIT OR Apache-2.0. The full texts are in [LICENSE-MIT](../LICENSE-MIT) and [LICENSE-APACHE](../LICENSE-APACHE). Third-party components keep their original licenses.

## Cargo dependencies

The locked Linux Cargo graph now passes the strict policy in [deny.toml](../deny.toml). Only MIT and Apache-2.0 are allowed; no license exceptions were added. `scripts/build_license_bundle.py` also generated full dependency notices successfully.

The GNU target's Cargo license audit also passes with the same policy, and source CI now checks it explicitly. That graph check does not clear the separate shared standard library or system runtime used by the [GNU experiment](../experiments/linux-gnu-runtime/README.md).

Package declarations do not settle file-level exceptions. The exact mapped `futures-channel 0.3.34/src/mpsc/queue.rs` carries a [two-clause BSD-style notice](https://github.com/rust-lang/futures-rs/blob/0.3.34/futures-channel/src/mpsc/queue.rs), although its package declares MIT OR Apache-2.0. Its source SHA-256 is `22034085dc22050b708a37854e215fc7cbb16d65edc60370cb5d8f4b7faca18e`. It was retained in the earlier GNU `10efeb22` experiment and remained in the preceding static build. Those package audit results do not clear that component.

The working source replaces that file with an original mutex-protected `VecDeque`, keeping the private interface consumed by the unchanged channel code. The BSD implementation was excluded before extracting the verified archive; its notice was not removed from retained code. Both MIT and Apache upstream notices remain with the other byte-identical package files. Five public channel regressions passed, covering concurrent delivery, FIFO order per producer, backpressure wakeups, close, sender drop and value cleanup. Seven guard regressions passed. The license bundle and GNU experiment now verify the selected local package and reviewed file hashes against [dependency-replacements.json](../vendor/dependency-replacements.json). Full source checks passed: formatting, strict Clippy, 242 Rust, 23 database and 79 Python cases, both Linux target package audits, replacement guards and notice generation. Rebuilt static core beb244cf passed 31 container and 29 HTTPS checks, and both official clients completed the three-track FLAC mix. GNU candidate 2df83ea9 also passed its source inventory, numerical and TLS checks, plus 31 container and 29 HTTPS checks; its captured replacement bytes match the reviewed hash, with no mapped registry futures-channel source. Ledgers are under `.local/channel-source-20261003`, `.local/channel-image-20261003` and `.local/channel-gnu-runtime-20261003`, with matching container, HTTPS and client directories. This repairs one known file exception; other file-level inputs and the linked runtime still require review.

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
| futures-channel's BSD queue | An original mutex-protected FIFO queue; upstream channel state and wakeup code are retained under MIT/Apache. |

Vendored MIT/Apache sources retain their upstream notices. Exact tarball checksums and patch descriptions are in [vendor/upstream.json](../vendor/upstream.json) and [vendor/README.md](../vendor/README.md). The adapters are original code in [crates/](../crates). The dependency regressions are in [tests/dependency_behavior.rs](../tests/dependency_behavior.rs).

## Linked runtime blocker

Cargo auditing does not cover the prebuilt Rust standard library or libc. The current unstripped static server contains `core::unicode::unicode_data` symbols for alphabetic, whitespace, case conversion, and related tables. Rust's `COPYRIGHT-library.html` assigns Unicode-3.0 to `library/core/src/unicode` data. That is outside the requested MIT/Apache-2.0 boundary.

The [static link audit](runtime-link-audit.md) identifies 389 selected libc archive members, five unwind members, and the target's startup objects. Source-path metadata for 378 libc objects matches candidates in musl 1.2.5; exact source bytes, included headers, patches, and retained sections still need review. Musl's notices include other terms alongside its main MIT license. The unwind, compiler-builtins, and startup inputs also need exact provenance and retained-section review. An experimental rebuilt-standard-library probe removed executable unwind functions but retained Unicode data and startup objects; it is not a validated replacement runtime. Keeping notices does not change a component's license. The build bundle retains Rust and builder-musl notices, but the whole binary/image is not claimed to meet the requested boundary.

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
