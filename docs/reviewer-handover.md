# Reviewer notes

Puffinbox is partial and unreleased. Start with [the compatibility matrix](compatibility.md), [acceptance results and reproduction](acceptance.md), and [the licensing inventory](licensing.md).

The current local checks pass: 211 standard Rust tests, 18 PostgreSQL integration cases plus one recorder unit test, 23 Python tests, browser/reader checks, strict Cargo auditing, static Linux/container builds, TLS certificate tests, and 20 container acceptance checks. The restart check exercises FFmpeg while active and verifies graceful drain, saved position, catalog persistence, and invalidation of the old HLS session.

Release gates remain open. Rust's linked Unicode data is outside the MIT/Apache-2.0 allowlist; musl's linked components need review. API coverage is 73 of 364 declarations, with broader behavior unvalidated. Feature areas are partial, current native playback is unvalidated, and there is no external-network or production-scale result.

The source is published on GitHub, and the [first CI run](https://github.com/peppermintish/puffinbox/actions/runs/36773703056) is in progress. `scripts/check_release_readiness.py` rejects release packaging until the documented gates pass. No release tag or archive has been created.

Use the generated synthetic acceptance state for tests. Its database, credentials, media, and logs are ignored by Git. Existing personal libraries and older test stacks should be preserved.
