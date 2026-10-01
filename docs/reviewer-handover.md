# Reviewer notes

Puffinbox is partial and unreleased. Start with [the compatibility matrix](compatibility.md), [acceptance results and reproduction](acceptance.md), and [the licensing inventory](licensing.md).

The current local checks pass: 214 standard Rust tests, 20 PostgreSQL integration cases plus one recorder unit test, 23 Python tests, browser/reader checks, strict Cargo auditing, static Linux/container builds, TLS certificate tests, and 20 container acceptance checks. The restart check exercises FFmpeg while active and verifies graceful drain, saved position, catalog persistence, and invalidation of the old HLS session.

Official Jellyfin web can authenticate, browse movie details, filter resume lists by media category, and decode HLS in Edge. Pause and stop work; resume still reports the wrong timeline after a server seek. The installed desktop player decoded HLS using Puffinbox's original interface and its native player. Its source-time display, keyboard seeking within the resumed clip, and saved stop position were checked against a burned-in timecode. Release gates remain open: linked Unicode data is outside the license allowlist, musl's linked components need review, and API coverage is 81 of 364 declarations. Feature areas are partial, and there is no external-network or production-scale result.

The source is published on GitHub. [CI passed at `11b531d`](https://github.com/peppermintish/puffinbox/actions/runs/36785032446); the navigation and playback fixes await their own cloud result. `scripts/check_release_readiness.py` rejects release packaging until the documented gates pass. No release tag or archive has been created.

Use the generated synthetic acceptance state for tests. Its database, credentials, media, and logs are ignored by Git. Existing personal libraries and older test stacks should be preserved.
