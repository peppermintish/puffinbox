# Local dependency patches

`upstream.json` records the exact upstream crate archives and their SHA-256
checksums. The MIT and Apache-2.0 license texts are kept with each dependency.

- `digest 0.10.7`: the MAC comparison module uses RustCrypto `ctutils::CtEq`
  instead of `subtle::ConstantTimeEq`. Hash and MAC algorithms are unchanged.
- `hashbrown 0.15.5`: its default hasher uses Rust's randomized `RandomState`
  instead of `foldhash`. This build requires `std` when the default hasher is
  enabled. The table implementation is unchanged.
- `hashlink 0.10.0`: its default hasher wrapper derives `Clone` without `Copy`,
  matching the randomized standard-library hasher used above.
- `futures-channel 0.3.34`: an original mutex-protected `VecDeque` replaces
  `src/mpsc/queue.rs`, whose upstream file carries a BSD notice without an
  MIT or Apache alternative. The other retained files match the verified crate
  archive, including both upstream license texts. The channel's state,
  backpressure, wakeup and oneshot implementations are unchanged. Queue access
  now takes a mutex; concurrent throughput at the target scale is unvalidated.

These patches keep SQLx's existing database and cryptographic APIs while removing
dependencies outside the project's license policy. Update them explicitly when
upgrading SQLx; do not edit the Cargo registry cache.

`dependency-replacements.json` records the reviewed channel file hashes.
`scripts/check_dependency_replacements.py` verifies Cargo selects this local
package and rejects changed, missing or added files. The license bundle and GNU
experiment run it before building their audited outputs.

The same record constrains registry `regex-syntax 0.8.11` to its currently
selected `std` feature. Its Unicode table modules have separate Unicode terms
and are disabled in both Linux target graphs. The guard checks resolved Cargo
features and hashes the reviewed manifest, module gates and license record. A
new version, changed gates or added features require review. Removing the
dependency entirely is allowed.

Registry `tower-http 0.6.11` has a CC0 notice on its compression module's
`pin_project_cfg` helper. Compression and decompression features are currently
disabled. Its empty default feature and selected HTTP middleware features are
reviewed; the same guard hashes its manifest, module gates and notice and
rejects any added feature, including compression and `full`.

Registry `openssl-src 300.6.1+3.6.3` includes a separate CC0 reference notice in
`openssl/crypto/siphash/siphash.c`. Its exact archive, Configure script and module
gates are recorded in the same guard. Linux builds use the repository's Python
Configure wrapper to append `no-siphash` and `no-quic`. The second option is
required because OpenSSL's internal QUIC implementation still uses SipHash when
only `no-siphash` is selected. Puffinbox's native TLS clients do not use QUIC.

The binary check requires a full symbol table and identifiable bundled OpenSSL,
then rejects retained SipHash symbols. The GNU experiment also checks link
objects and captured native compilation inputs. The previous static binary and
the first single-option build fail this check. Fresh combined-option builds pass
without changing registry source or the allowlist. These are exclusion checks;
they do not assign a new license to the upstream file.

When adopting or changing this wrapper in an existing Cargo build directory,
rebuild the native dependency with `cargo clean -p openssl-sys` before building.
The upstream build does not track changes to the wrapper file or
`OPENSSL_SRC_PERL`. Docker and CI use fresh build inputs, and the post-build check
rejects a stale executable that still retains SipHash. Linux source builds
require Python 3, Perl and Make; native Windows source builds are unvalidated.

No registry files were modified or copied into the repository. These checks
cover known exceptions, not complete dependency or linked runtime license
clearance.

The crates in `../crates` are original compatibility adapters. The route matcher
implements the Axum interface with a segment tree; it is not copied from matchit.
The identifier adapter accepts ASCII identifiers for procedural macro builds and
does not contain Unicode tables. It does not limit text in media titles, paths,
subtitles, or the user interface.
