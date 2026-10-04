# Third-party notices

Puffinbox application code is licensed under either MIT or Apache-2.0. The full license texts are in `LICENSE-MIT` and `LICENSE-APACHE`.

## Browser playback library

This project bundles the official `hls.js` v1.7.3 distribution from the npm registry package tarball. It is used to play server-provided HLS streams in browsers that support Media Source Extensions but do not play HLS natively.

- Copyright (c) 2017 Dailymotion; files derived from `videojs-contrib-hls` retain the Brightcove notice described in the package license.
- License: Apache-2.0. See `web/vendor/hls.js-LICENSE.txt` and `LICENSE-APACHE` for the full license text.
- Artifact provenance and verified hashes: `web/vendor/README.md`.

## PDF reading library

- The book reader bundles the generic display and worker modules from Mozilla PDF.js `pdfjs-dist` v6.3.289 under Apache-2.0. The complete license text is in `web/vendor/pdfjs/LICENSE`.
- The retained qcms WebAssembly decoder is MIT-licensed; its upstream notices are in `web/vendor/pdfjs/wasm/LICENSE_QCMS` and `web/vendor/pdfjs/wasm/LICENSE_PDFJS_QCMS`.
- Adobe CMaps, Foxit base fonts, and JBIG2 and OpenJPEG decoders were removed because their BSD-style licenses are outside the project's MIT/Apache-2.0 allowlist. This limits support for CMap-dependent text and JBIG2/JPEG2000 images; the reader uses browser system-font substitutes where possible.
- Artifact provenance and the verified npm registry integrity value: `web/vendor/pdfjs/README.md`.

## Rust dependency graph and static runtime

The release build creates `dist/licenses/` from the locked Cargo graph using `cargo-deny` and `cargo-about`. It includes full accepted dependency license texts, the Rust standard library and toolchain copyright records, and the musl runtime copyright record for the static server binary. Release archives and the minimal container image include this directory when the strict license gate passes.

The Cargo allowlist is limited to MIT and Apache-2.0. Run `cargo deny --locked check licenses` locally; any other or unclear expression stops the Cargo license gate. The current locked graph passes without exceptions. Package declarations do not clear individual file notices: the upstream futures-channel BSD queue is replaced with original MIT/Apache code, with both upstream license texts preserved for the other retained files. The build verifies the selected replacement and reviewed hashes. Rust's linked Unicode tables, musl components and remaining file-level inputs still block release packaging. See [`docs/licensing.md`](docs/licensing.md) for the runtime blocker, replacements, and external component inventory.

## MIME extension data

The local mime_guess 2.0.5 patch retains Austin Bonander's MIT license and the
upstream extension table. The table cites Samuel Neff's MimeTypeMap and the
mime-db project; their MIT notices are retained in
`vendor/mime_guess/LICENSE-MIMETYPEMAP` and `LICENSE-MIME-DB` and copied into the
generated notice bundle. Exact upstream, retained-file and notice hashes are in
`vendor/dependency-replacements.json`. The original ASCII comparator excludes
unicase and its generated Unicode case-folding data. See
`vendor/mime_guess/PUFFINBOX.md` for behavior and remaining historical data-input
limits. This patch does not clear the full runtime boundary.

## PostgreSQL driver

The local SQLx PostgreSQL 0.8.6 patch retains the upstream MIT and Apache-2.0
licenses, including the LaunchBadge copyright notices. It sends an empty SCRAM
username because PostgreSQL uses the unchanged startup role. Removing the
`stringprep` dependency excludes its generated Unicode dependencies from the
selected PostgreSQL graph. Password processing, proofs and TLS options remain
unchanged. The provenance, two changed files and limitations are recorded in
`vendor/sqlx-postgres/PUFFINBOX.md`; exact retained-file hashes are in
`vendor/dependency-replacements.json`. The full bundled-runtime review remains
open.

## External operator services and executables

The Compose stack references a separate PostgreSQL container image. PostgreSQL is an external service and is not copied into the Puffinbox server image. FFmpeg and ffprobe are supplied by the operator or the test-only acceptance image; the default server image does not include them. Exact image-package inventories and operator-selected FFmpeg build licenses are separate from this project source and are recorded as unresolved in [`docs/licensing.md`](docs/licensing.md).
