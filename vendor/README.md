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

These patches keep SQLx's existing database and cryptographic APIs while removing
dependencies outside the project's license policy. Update them explicitly when
upgrading SQLx; do not edit the Cargo registry cache.

The crates in `../crates` are original compatibility adapters. The route matcher
implements the Axum interface with a segment tree; it is not copied from matchit.
The identifier adapter accepts ASCII identifiers for procedural macro builds and
does not contain Unicode tables. It does not limit text in media titles, paths,
subtitles, or the user interface.
