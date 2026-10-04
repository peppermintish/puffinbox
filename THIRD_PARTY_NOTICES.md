# Third-party notices

Puffinbox application code is licensed under MIT or Apache-2.0. The full texts are in `LICENSE-MIT` and `LICENSE-APACHE`. Dependencies retain their own compatible licenses and notices.

## Browser playback library

The project bundles the official hls.js v1.7.3 npm distribution under Apache-2.0. Copyright (c) 2017 Dailymotion; files derived from videojs-contrib-hls retain the Brightcove notice described in the package license. Full attribution is in `web/vendor/hls.js-LICENSE.txt`, with provenance and hashes in `web/vendor/README.md`.

## PDF reading library

The book reader bundles unchanged Mozilla PDF.js `pdfjs-dist` v6.3.289 assets under Apache-2.0, with the full text in `web/vendor/pdfjs/LICENSE`. The qcms decoder uses MIT terms. Adobe CMaps and Foxit base fonts use BSD-3-Clause; JBIG2 uses BSD-3-Clause and Apache-2.0; OpenJPEG uses BSD-2-Clause. Their full notices are retained beside the assets and copied into the distribution license bundle.

Liberation fonts with GPL font terms are excluded. The reader uses installed font substitutes where needed and disables PDF scripting and XFA. Provenance, the reviewed file hashes, and registry integrity are in `web/vendor/pdfjs/README.md` and `provenance.json`.

## Rust dependencies and static runtime

The release build audits `Cargo.lock` using cargo-deny and generates full dependency license texts using cargo-about. `dist/licenses/` includes those texts, project notices, Rust standard-library/toolchain copyright records and musl attribution. Images and archives retain the bundle.

The accepted permissive terms and external-component boundaries are recorded in [docs/licensing.md](docs/licensing.md). HTTP and database TLS use rustls. The standard provider's upstream notices remain applicable. No OpenSSL TLS implementation is bundled.

The MIME table's cited MIT notices are preserved in `vendor/notices/MIMETYPEMAP-MIT.txt` and `vendor/notices/MIME-DB-MIT.txt`. Tracing Subscriber's musl-derived timestamp formatter notice is preserved in `vendor/notices/tracing-subscriber-datetime-MIT.txt`. The bundle copies all three supplemental notices.

The local SQLx PostgreSQL 0.8.6 package retains LaunchBadge's MIT and Apache-2.0 texts and all upstream notices. Its narrow SCRAM role and password fix and verified provenance are recorded in `vendor/sqlx-postgres/PUFFINBOX.md` and `provenance.json`; the original stringprep dependency remains enabled for compatible password preparation.

## External services and executables

PostgreSQL is a separate service and is not copied into the server image. FFmpeg and ffprobe are supplied by the operator or a separate acceptance-tool image. Operator-assembled runtimes retain the chosen distribution's notices and obligations; they are separate from project releases.
