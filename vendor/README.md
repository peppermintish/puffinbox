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
dependency entirely is allowed. No registry files were modified or copied into
the repository. This covers known exceptions, not complete dependency or linked
runtime license clearance.

The crates in `../crates` are original compatibility adapters. The route matcher
implements the Axum interface with a segment tree; it is not copied from matchit.
The identifier adapter accepts ASCII identifiers for procedural macro builds and
does not contain Unicode tables. It does not limit text in media titles, paths,
subtitles, or the user interface.
