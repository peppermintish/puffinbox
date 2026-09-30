# Reviewer notes

Puffinbox is partial and unreleased. Start with [the compatibility matrix](compatibility.md), [acceptance results and reproduction](acceptance.md), and [the licensing inventory](licensing.md).

The current local checks pass: 211 standard Rust tests, 20 PostgreSQL integration cases plus one recorder unit test, 23 Python tests, browser/reader checks, strict Cargo auditing, static Linux/container builds, TLS certificate tests, and 20 container acceptance checks. The restart check exercises FFmpeg while active and verifies graceful drain, saved position, catalog persistence, and invalidation of the old HLS session.

Official Jellyfin web can now authenticate and list the synthetic movie library. Item details and playback still need work. Release gates remain open: linked Unicode data is outside the license allowlist, musl's linked components need review, and API coverage is 79 of 364 declarations. Feature areas are partial, native playback is unvalidated, and there is no external-network or production-scale result.

The source is published on GitHub. [CI passed at `fe76ccc`](https://github.com/peppermintish/puffinbox/actions/runs/36780885839); the newer preference and catalog changes await their own cloud result. `scripts/check_release_readiness.py` rejects release packaging until the documented gates pass. No release tag or archive has been created.

Use the generated synthetic acceptance state for tests. Its database, credentials, media, and logs are ignored by Git. Existing personal libraries and older test stacks should be preserved.
