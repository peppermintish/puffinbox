# Licensing and distribution

Puffinbox's original code is offered under MIT or Apache-2.0. Dependencies may use other compatible permissive terms; they retain their own licenses and attribution requirements. The policy was corrected on 2026-10-05 from an exact two-license allowlist to compatibility with MIT and Apache-2.0.

## Dependency policy

`deny.toml` and `about.toml` accept MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, ISC, Zlib, CC0-1.0, Unicode-3.0, Unicode-DFS-2016 and Apache-2.0 with the LLVM exception. Unknown terms and licenses outside this list fail the dependency audit. This list does not relicense upstream work. Additional file notices must be retained even where the package manifest offers MIT or Apache-2.0.

HTTP and PostgreSQL TLS use rustls with native certificate roots. The selected Linux graphs must contain no OpenSSL or native-tls crypto packages. `openssl-probe` is a Rust certificate-path discovery utility used by rustls-native-certs; it does not provide or link OpenSSL. Standard rustls crypto providers and their native internals are permitted.

The former license-exclusion forks and ASCII-only URL adapter have been removed. Cargo resolves registry implementations from the lockfile, including normal Unicode hostname handling. A single SQLx PostgreSQL patch preserves valid Unicode database roles: its upstream username SASLprep panics on these roles. The patch sends an empty SCRAM username while retaining the original startup role and server-proof verification; password preparation now follows PostgreSQL normalization and raw fallback rules. PostgreSQL documents that it ignores this SCRAM username. Archive/file hashes and retained notices are in `vendor/sqlx-postgres/`; the real authentication regression covers the behavior. The compiled-Wasm-only plugin setting remains a product restriction.

## Project source

Every Cargo target uses `unsafe_code = "forbid"`. The library and server also declare `#![forbid(unsafe_code)]`. This applies to project tests and examples as well as production Rust. External dependencies keep their upstream implementation. Puffinbox contains no project-owned C or assembly.

Media processes retain resource limits, Landlock filesystem rules and syscall restrictions. The server starts a single-threaded internal worker before launching a decoder, using safe dependency APIs to transfer file capabilities and apply confinement.

## Notices and release artifacts

`scripts/build_license_bundle.py` audits the locked Cargo graph and writes full dependency texts to `dist/licenses/`. The bundle also contains project notices, supplemental MIME and timestamp notices, musl attribution and Rust standard-library/toolchain copyright records. Images and archives include that directory. Browser distributions retain their notices under `web/vendor/`.

The previous runtime experiments addressed the superseded exact-license policy. Their source and reports remain in Git history and private validation records; they are not part of the current build. See [the historical runtime record](runtime-link-audit.md).

A passing package audit alone is not release acceptance. The rebuilt image must contain the required notices, use the intended TLS provider and pass the current source, container, restart and client checks. The [release gates](release-gates.json) remain authoritative.

## External components

PostgreSQL runs as a separate operator-supplied service. FFmpeg and ffprobe are external executables; the default project image contains neither. Operator-assembled FFmpeg runtime images retain their distribution's notices and codec obligations and are separate from project release artifacts. Test certificate generators, reverse proxies and official Jellyfin clients are external validation tools.

This inventory records the project's distribution choices and evidence. It is not a legal opinion or a compatibility certification.
