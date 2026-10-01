# Public interface and dependency provenance

This record describes the public material and runtime behavior used while developing Puffinbox. It is a project record, not a legal opinion or a compatibility certification.

## Jellyfin interface references

- Public route and schema comparison: [Jellyfin 12.0.0 stable OpenAPI document](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json) and [Jellyfin API reference](https://api.jellyfin.org/).
- Installed desktop client interface documentation: [Jellyfin Media Player 1.12.0 for web developers](https://raw.githubusercontent.com/jellyfin/jellyfin-media-player/v1.12.0/for-web-developers.md) and [Jellyfin Media Player 1.12.0 client API](https://raw.githubusercontent.com/jellyfin/jellyfin-media-player/v1.12.0/client-api.md).

These sources describe public HTTP routes and the documented desktop bridge. Puffinbox's server handlers, browser interface, and native-player adapter are original project code. The public API version and route declarations do not establish semantic compatibility for operations that have not been tested.

## Runtime observations

On 2026-09-27, the installed Jellyfin Media Player 1.12.0 on Windows was used to load the server-hosted `/web/` page and exercise its login flow. The client uses the page supplied by the server. On 2026-09-29, against rebuilt image `664a5a3`, the installed client played the 180-second subtitle-selection fixture natively and displayed English cues at their expected source times. In a separate run it also rendered cue 10 after switching from Server default to English mid-playback. During that switch, the player progress display reset to a 0:49 HLS window while the burned-in source timestamp continued; no seeking behavior is inferred. These are narrow observations for one fixture and client, not broad API, codec, or client compatibility. Seeking and audio-track selection remain unvalidated.

On 2026-10-01, official compiled Jellyfin web assets were loaded through a localhost proxy against the synthetic test server. API response bodies were forwarded unchanged. Authentication, movie details, media-category resume lists, HLS decoding, pause, and stop worked. Resume exposed a client/server timeline mismatch. Public demo `PlaybackInfo` responses were compared as HTTP data; no implementation source was read. The demo reported 12.1.0, while the API schema target remains 12.0.0.

The installed desktop player also decoded the current synthetic HLS fixture through Puffinbox's original web interface and the documented native bridge. Its source-time display matched the burned-in timecode. Home and Right sought within the resumed clip while paused, and the stop callback saved 125.873 seconds. This validates that bounded control path; general seeking and audio-track selection remain unvalidated.

Development of the interface relied on the public documentation linked above and direct interaction with the installed client. No Jellyfin server or client GPL implementation source was inspected, copied, or adapted for this project.

## Bundled browser dependency

The local HLS adapter is the official `hls.js` v1.7.3 npm distribution. Its registry tarball integrity, local SHA-256, upstream reference, and Apache-2.0 license are recorded in [the bundled dependency record](../web/vendor/README.md); full attribution and license text are included in [third-party notices](../THIRD_PARTY_NOTICES.md) and `LICENSE-APACHE`. The script is served from the local web root and is not fetched from a third-party host at runtime.

## Cargo replacements

The route matcher and ASCII identifier adapter in `crates/` are original code implementing public interfaces consumed by MIT/Apache dependencies. They do not copy the replaced implementations. The small digest, hashbrown, and hashlink patches retain their upstream MIT/Apache licenses and notices; source tarball hashes and changes are recorded in [vendor/](../vendor/README.md).
