# Supplemental dependency notices

Cargo dependencies are resolved using `Cargo.lock`. The sole local package is `sqlx-postgres`, retaining a narrow SCRAM role fix with its upstream manifest and license texts, with compatible password preparation. Its verified archive and file hashes are recorded in `sqlx-postgres/provenance.json`.

`notices/MIMETYPEMAP-MIT.txt` and `notices/MIME-DB-MIT.txt` preserve attribution for sources cited by mime_guess's extension table. `notices/tracing-subscriber-datetime-MIT.txt` preserves the musl-derived timestamp formatter notice used by Tracing Subscriber. The license bundle copies these texts alongside the generated Cargo notices.

The previous source forks and exact-license guards were retired when the dependency policy changed to MIT/Apache-2.0 compatibility on 2026-10-05. Their history remains in Git.
