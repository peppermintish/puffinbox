# Reviewer notes

Puffinbox is partial and unreleased. Start with [the compatibility matrix](compatibility.md), [acceptance results and reproduction](acceptance.md), and [the licensing inventory](licensing.md).

The latest local source and container results are recorded in acceptance.md: 220 standard Rust tests, 22 PostgreSQL integration cases plus one recorder unit test, 23 Python tests, and 24 container checks. The restart check stops the server while FFmpeg is active and verifies graceful drain, saved playback position, catalog persistence, socket reconnection, and invalidation of the old HLS session.

Official Jellyfin web can authenticate, browse movies, combine catalog filters, decode full-duration HLS, seek, and resume from cached details after user-data notifications. A transient playback notice remains unresolved. Earlier native-player checks used Puffinbox's original interface; the desktop client's usual interface and the current filter image have no native-player pass. The route report matches 83 of 364 operation declarations and does not measure behavioral compatibility.

The last complete [cloud pass is `a52ef31`](https://github.com/peppermintish/puffinbox/actions/runs/36854813552). The [`f33c138` container job passed](https://github.com/peppermintish/puffinbox/actions/runs/36909587357), but its source job failed in the Live TV browser test. The synchronization fix passes locally and still needs a cloud result. Current run status belongs in acceptance.md rather than being inferred from an older successful checkpoint.

Release gates remain open. Linked Unicode data is outside the license allowlist, and the [runtime audit](runtime-link-audit.md) has not cleared libc, unwind, compiler-builtins, or startup objects. Feature areas remain partial; there is no external-network or production-scale result. `scripts/check_release_readiness.py` rejects release packaging until the documented gates pass. No release tag or archive has been created.

Use the generated synthetic acceptance state for tests. Its database, credentials, media, and logs are ignored by Git. Existing personal libraries and older test stacks should be preserved.
