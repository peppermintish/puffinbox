# Licensing inventory

Puffinbox source is MIT OR Apache-2.0. The full texts are in [LICENSE-MIT](../LICENSE-MIT) and [LICENSE-APACHE](../LICENSE-APACHE). Third-party components keep their original licenses.

## Cargo dependencies

The locked Linux Cargo graph now passes the strict policy in [deny.toml](../deny.toml). Only MIT and Apache-2.0 are allowed; no license exceptions were added. `scripts/build_license_bundle.py` also generated full dependency notices successfully.

The GNU target's Cargo license audit also passes with the same policy, and source CI now checks it explicitly. That graph check does not clear the separate shared standard library or system runtime used by the [GNU experiment](../experiments/linux-gnu-runtime/README.md).

Package declarations do not settle file-level exceptions. The exact mapped `futures-channel 0.3.34/src/mpsc/queue.rs` carries a [two-clause BSD-style notice](https://github.com/rust-lang/futures-rs/blob/0.3.34/futures-channel/src/mpsc/queue.rs), although its package declares MIT OR Apache-2.0. Its source SHA-256 is `22034085dc22050b708a37854e215fc7cbb16d65edc60370cb5d8f4b7faca18e`. It was retained in the earlier GNU `10efeb22` experiment and remained in the preceding static build. Those package audit results do not clear that component.

Puffinbox replaces that file with an original mutex-protected `VecDeque`, keeping the private interface consumed by the unchanged channel code. The BSD implementation was excluded before extracting the verified archive; its notice was not removed from retained code. Both MIT and Apache upstream notices remain with the other byte-identical package files. Five public channel regressions passed, covering concurrent delivery, FIFO order per producer, backpressure wakeups, close, sender drop and value cleanup. Seven guard regressions passed. The license bundle and GNU experiment now verify the selected local package and reviewed file hashes against [dependency-replacements.json](../vendor/dependency-replacements.json). Full source checks passed: formatting, strict Clippy, 242 Rust, 23 database and 79 Python cases, both Linux target package audits, replacement guards and notice generation. Rebuilt static core beb244cf passed 31 container and 29 HTTPS checks, and both official clients completed the three-track FLAC mix. GNU candidate 2df83ea9 also passed its source inventory, numerical and TLS checks, plus 31 container and 29 HTTPS checks; its captured replacement bytes match the reviewed hash, with no mapped registry futures-channel source. Ledgers are under `.local/channel-source-20261003`, `.local/channel-image-20261003` and `.local/channel-gnu-runtime-20261003`, with matching container, HTTPS and client directories. This repairs one known file exception; other file-level inputs and the linked runtime still require review.

The current studio source also passed both package audits, selected-source guards and notice generation, with 243 Rust cases, 24 database cases and 79 Python checks. Core `3f06f4ac` passed 31 container and 29 HTTPS checks. [Both main cloud jobs passed at `d356176`](https://github.com/peppermintish/puffinbox/actions/runs/37094607378), as did [both GNU TLS modes](https://github.com/peppermintish/puffinbox/actions/runs/37094607400). These results preserve the strict allowlist; they do not clear the linked-runtime blocker below.

Registry `regex-syntax 0.8.11` declares MIT OR Apache-2.0, but its [Unicode table directory has separate Unicode terms](https://github.com/rust-lang/regex/blob/140167995737fa11dfe11b8af8b9aa143b790b4e/regex-syntax/src/unicode_tables/LICENSE-UNICODE). Both Linux target graphs currently enable only `std`. The reviewed module declarations gate every Unicode table behind disabled features. The dependency guard now checks those resolved features and the exact manifest and module-gate hashes; default, Unicode or other unreviewed features cause failure. A version change requires review. Thirteen guard regressions and all 85 Python checks passed. This preserves an existing exclusion; it does not remove the separate Unicode data retained by the Rust standard library or establish whole-binary clearance.

Registry `tower-http 0.6.11` declares MIT, but its [compression helper has a separate CC0 notice](https://github.com/tower-rs/tower-http/blob/1d082ef7bdb6d80a2964698804a46c338b4c6a99/tower-http/src/compression/pin_project_cfg.rs). The compression module is disabled in both Linux target graphs. The guard now also checks Tower HTTP's exact manifest, module gates and helper hash, and allows only the currently reviewed middleware features. Its `default` feature is empty. Compression, decompression, `full` and other added features fail the check. Fourteen guard regressions and all 86 Python checks passed, along with both target guards, the GNU package audit and full notice generation. No registry source or license allowlist changed. [Both main jobs passed at the preceding regex guard checkpoint `871ec9a`](https://github.com/peppermintish/puffinbox/actions/runs/37096233490), as did [both GNU TLS modes](https://github.com/peppermintish/puffinbox/actions/runs/37096233479).

The selected `rustix 1.1.5` [Linux vDSO parser](https://github.com/bytecodealliance/rustix/blob/287214b889865d8e1406a0ee71cc409b6f6191c8/src/backend/linux_raw/vdso.rs) identifies a CC0 source origin despite the package's MIT/Apache alternatives. Its exact source appears in a linked package's compiler dependency rule for historical GNU server `5f872697`, but has no mapped instruction interval there. Linux builds now require Rustix's supported `use-libc` backend. The source guard checks that required feature, the permitted feature set and six exact manifest/build/module hashes. It rejects removal of `use-libc` or addition of unreviewed features; fifteen guard regressions and all 104 Python cases passed. Independent artifact checks found no raw-backend source input in two debug profiles, exact static server `f28cfbfd` or GNU audit server `b957a857`. The static image passed 35 container and 29 HTTPS checks, plus the original album in both official clients. The [input review](runtime-link-audit.md#rust-package-input-review) preserves the archive, file, feature and artifact hashes. This excludes one known file origin without changing upstream source, licensing terms or the allowlist; complete retained-input and linked-runtime review remains open.

That broader review also verified historical mapped and selected Rust source inputs, including three generated modules replayed from locked build scripts. The tracing date-conversion file's full musl notice includes BSD exceptions for other files; its selected time-conversion code and the upstream author's permission identify MIT/Apache options. Those text matches do not establish a selected BSD origin. Logging was left unchanged. The [runtime audit](runtime-link-audit.md#rust-package-input-review) records the exact primary permission evidence and the limits of textual searches.

Plugins now accept compiled WebAssembly only. Wasmi's optional text compiler is disabled, removing `wat`, `wast`, `wasm-encoder`, `unicode-width`, `leb128fmt` and the compiler's separate `wasmparser` version from the locked graph. The guard allows only Wasmi's required `std` feature and verifies its exact manifest and parser gate against the checksum-verified 1.1.0 archive. Original binary fixtures retain the hook, import rejection, memory, fuel and output-limit checks. The [example instructions](../examples/plugins/README.md) explain how to stage the binary and migrate text hooks from earlier unreleased checkpoints. This removes one generated-data dependency; the other Unicode inputs and whole-runtime review remain open.

```sh
cargo deny --locked check
python3 scripts/build_license_bundle.py
```

## Bundled OpenSSL exclusion

The selected `openssl-src 300.6.1+3.6.3` archive contains a separate CC0 reference
notice in [the SipHash implementation](https://github.com/openssl/openssl/blob/openssl-3.6.3/crypto/siphash/siphash.c),
alongside its Apache header. Its source SHA-256 is
`67b99076c867bc014fcdad96f50c9195dec1971d0b41a59606fcee38844a2c3a`.
The earlier static server retained five named SipHash functions. Package-level
Apache declarations did not clear that input.

Linux builds now exclude the implementation through an original Configure
wrapper with `no-siphash` and `no-quic`. The first `no-siphash` build still
retained those functions: OpenSSL's internal QUIC code uses them independently of
the disabled provider. The combined options remove both paths; Puffinbox's
native TLS clients do not use QUIC. No upstream source or license terms changed.
The [dependency record](../vendor/dependency-replacements.json) guards the exact
package, selected features, archive and configuration inputs.

The exclusion checker rejects the old binary and the failed single-option build.
Fresh source, static core `88c03d90` and experimental GNU server `e02164b6` pass.
The GNU link map has no SipHash objects, and its successful native compilation
capture has no SipHash implementation input. Mapped-source inspection and
compiler-notice classification also passed. Source checks passed formatting,
strict Clippy, 243 Rust cases, 24 database cases, 92 Python checks, both target
package audits and full notice generation. Trusted certificates were accepted;
wrong CAs and hostnames were rejected. The rebuilt core passed 31 container and
29 local HTTPS checks, including active FFmpeg shutdown and saved video progress
after restart. Both official clients completed the original three-track FLAC
mix on the retained backend. Evidence is under `.local/openssl-exclusion-*`,
with the failed first builds preserved separately from the passing `20261003b`
source and GNU runs.

This resolves the known retained OpenSSL file exception. Whole-runtime license
clearance remains false for the linked-runtime reasons below. Existing Cargo
build directories need a native dependency rebuild when adopting or changing the
wrapper; see [the build note](../vendor/README.md).

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

The GNU [bounded string audit](runtime-link-audit.md#bounded-string-references)
now records supported Rust string references within source-associated read-only
variables. On the earlier `3bbe84d4` binary, it adds 13,795 bytes of payload
coverage and retains all previous variable records. The 80 GNU regressions pass,
including compiled C and Rust controls. Root declarations do not establish
literal origins, and most read-only data remains uncovered. This is additional
audit evidence; whole-runtime clearance and the MIT/Apache boundary are unchanged.

The GNU audit now preserves exact native source bodies instead of hashes alone. Fresh candidate `d7d42ac4` has all 927 mapped native files available after build cleanup and passes its source/notice checks, 33 container checks and 29 local HTTPS checks. A separate replay regenerated all 107 bodies without byte-identical package candidates from the verified locked OpenSSL archive, matching every preserved output hash. Selected generator/template hashes and their notice contexts are recorded; file-level license review remains open. Copies and non-allowlisted external runtime files stay outside CI uploads and release bundles. This improves reviewability without clearing the boundary; see [runtime-link-audit.md](runtime-link-audit.md#preserved-native-source-copies).

A corrected GNU capture now includes inherited system-header dependencies. Candidate `d1275bc2` passed source/notice checks, 35 container checks and 29 local HTTPS checks, with 1,872 verified byte copies and 252 system input paths. All 927 mapped native body hashes match the earlier candidate. Earlier traces omitted system headers when OpenSSL supplied user-only dependency options; their mapped-body records do not establish complete include coverage. The additional headers and unmapped content remain under review. See [the capture correction](runtime-link-audit.md#system-headers-in-dependency-rules).

A [preprocessor replay](runtime-link-audit.md#native-preprocessor-replay) completed all 1,034 conservatively selected C and preprocessed-assembly commands against their exact captured inputs and compiler. Independent verification covered 3,102 compressed outputs and records. It identifies system macro expansions or definedness tests and two larger macro candidates for review; it does not prove retained code or clear header licensing. No policy exception, runtime replacement or production packaging change was made.

The experimental [character adapter](runtime-link-audit.md#external-c-character-conversion) now calls external C case-conversion functions through their public declarations. Fresh GNU candidate `94f7a1e4` passed its byte/EOF fixture, full source/notice checks, 35 container and 29 HTTPS checks, and the original four-track album in both official clients. A complete new preprocessor replay no longer reports `__tobody`; the socket declaration macro remains for review. The static backend was restored with all 50 saved rows retained. This narrows the known header contribution without clearing unmapped content, external-runtime distribution or production adoption. The allowlist and release gates are unchanged.

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

## ASCII MIME lookup

The local [mime_guess 2.0.5 patch](../vendor/mime_guess/PUFFINBOX.md) replaces
unicase with an original ASCII comparator. The checksum-verified upstream archive
is `f7c44f8e672c00fe5308fa235f821cb4198414e1c77935c1ab6948d3fd78550e`.
The MIT notice and complete extension table are unchanged; the table SHA-256 is
`1e89c58024547606d78e71488f0e027b740613fc22ac5877e24e3851e0f0628b`.
The exact retained-file guard covers fourteen files. Git preserves their reviewed
bytes and original line endings. The locked production and test graphs no longer
contain unicase or its Unicode case-folding table. Forward lookup covers all 1,408
registered extensions in lower, upper and mixed ASCII case, with Unicode filename
prefixes. Optional reverse mappings, wildcard projections and nine served-file
header cases also pass. Non-ASCII extensions return an empty guess and use the
caller's unknown-type fallback.

The table cites [MimeTypeMap](https://github.com/samuelneff/MimeTypeMap/blob/45622b360000f1450c8241c5e83ad61f46b902d8/LICENSE)
and [mime-db](https://github.com/jshttp/mime-db/blob/424fb61ca34d480d3f25dd945acc44f37c360f56/LICENSE)
as data sources. Their MIT notices are preserved and copied into the full notice
bundle. The [cited converter](https://gist.github.com/soyuka/b7e29d359b2c14c21bdead923c01cc81)
declares WTFPL; it is not copied, executed or bundled. Those license records do
not establish the exact historical extraction inputs or later manual updates for
every retained entry. That input review remains open; package MIT declarations
and passing audits do not complete it.

Static server `1d69ce60` still defines 41 generated Rust Unicode namespace symbols.
This MIME exclusion leaves the standard-library, native, startup and remaining
generated-data reviews open. It changes no allowlist, exception or release gate.

## PostgreSQL SCRAM username

The local [SQLx PostgreSQL 0.8.6 patch](../vendor/sqlx-postgres/PUFFINBOX.md)
retains its MIT and Apache-2.0 notices. The archive SHA-256 is
`db58fcd5a53cf07c184b154801ff91347e4c30d17a3562a635ff028ad5deda46`,
and the recorded upstream revision is `bab1b022bd56a64f9a08b46b36b97c5cff19d77e`.
The exact guard covers 116 files: 113 byte-identical upstream files, two changed
upstream files and an original provenance note. Git preserves their reviewed
bytes. The active manifest drops stringprep; the authentication function sends
an empty SCRAM username. [PostgreSQL uses the role from the startup message](https://www.postgresql.org/docs/18/sasl-authentication.html)
and ignores the SCRAM username field. Nonce generation, password processing,
client proofs, server-signature verification and TLS options are unchanged.

The selected normal, build and development dependency trees for both Linux
targets exclude stringprep, unicode-bidi, unicode-normalization and
unicode-properties. The lockfile still contains inactive SQLx MySQL dependencies;
this exclusion does not claim their deletion or a general SASLprep replacement.
A root integration test, also selected by CI, checks ten real PostgreSQL SCRAM
exchanges with Unicode and punctuation roles, incorrect passwords, a missing
role and altered server signatures. Its packet proxy is limited to a disposable
loopback fixture. Unicode password normalization and channel binding remain
unvalidated.

Both strict package audits and full notices pass without exceptions. Exact
current static server `863f0c01` still defines 41 generated Rust Unicode namespace
symbols. Standard-library, native, startup, historical MIME inputs and complete
bundled-runtime licensing remain open. No allowlist or release gate is relaxed.


The later GNU candidate `3bbe84d4` keeps its separately supplied standard library
external and has no defined generated Unicode symbols in the runtime inventory.
Its mapped sources and named objects have identified owners. The new
[read-only data inspection](runtime-link-audit.md#read-only-variable-data) binds
address-backed declarations to exact build hashes while reporting unexamined
bytes explicitly. Source-less vtable names, anonymous constants, inlined
contents and complete binary provenance remain uncleared. Passing this scoped
inspection does not establish the requested bundled license boundary.

## Timestamp notice

The selected Tracing Subscriber 0.3.23 formatter includes a musl-derived
timestamp routine and its supplemental MIT notice. The exact source SHA-256 is
`a6eeeb475e1f0b8cf90d1ef0dcb862b244cea58261485d440d51095694637213`.
The [cited Kudu contributor permits MIT use](https://github.com/tokio-rs/tracing/issues/1644#issuecomment-963888244).
The upstream blanket musl notice lists BSD exceptions for other paths; it does
not classify `src/time/__secs_to_tm.c` as BSD. No implementation or license
declaration was changed for this review.

The [exact supplemental notice](../vendor/notices/tracing-subscriber-datetime-MIT.txt)
is preserved with SHA-256
`e239ef69c9c4eead9406bdc6a34f2bd1220c3dcd3b17c8377cb2fb73fdaf0d38`.
The [source guard](../vendor/dependency-replacements.json) checks the version,
selected features, source/configuration hashes and notice bytes. Notice
generation copies it to `TRACING-DATETIME-MIT.txt`; the existing builder-musl
notice did not contain this exact notice. This resolves a specific attribution
gap, without clearing every source file, retained constant or runtime input.

## Album-name image notice check

The current static server `523c195b` still defines 41 generated Rust Unicode
namespace symbols. Its retained image contains the exact supplemental timestamp
MIT notice (`e239ef69`), verified against the reviewed source bytes. Source guards,
package audits and full notice generation pass without exceptions. This image
check does not clear standard-library, native, startup, compiler-builtins,
anonymous or inlined inputs. Exact records are under
`.local/album-tag-name-client-20261004`; all release gates remain open.
