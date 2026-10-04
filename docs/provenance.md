# Public interface and dependency provenance

This record describes the public material and runtime behavior used while developing Puffinbox. It is a project record, not a legal opinion or a compatibility certification.

## Jellyfin interface references

- Public route and schema comparison: [Jellyfin 12.0.0 stable OpenAPI document](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json) and [Jellyfin API reference](https://api.jellyfin.org/).
- Installed desktop client interface documentation: [Jellyfin Media Player 1.12.0 for web developers](https://raw.githubusercontent.com/jellyfin/jellyfin-media-player/v1.12.0/for-web-developers.md) and [Jellyfin Media Player 1.12.0 client API](https://raw.githubusercontent.com/jellyfin/jellyfin-media-player/v1.12.0/client-api.md).
- HLS protocol and encoder options: [RFC 8216](https://www.rfc-editor.org/rfc/rfc8216.html) and [FFmpeg format documentation](https://ffmpeg.org/ffmpeg-formats.html).
- Socket protocol and public message types: [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455.html), [Jellyfin SDK WebSockets guide](https://kotlin-sdk.jellyfin.org/guide/websockets.html), [UserDataChangedMessage](https://typescript-sdk.jellyfin.org/interfaces/generated-client.UserDataChangedMessage.html), [UserDataChangeInfo](https://typescript-sdk.jellyfin.org/interfaces/generated-client.UserDataChangeInfo.html), and [ForceKeepAliveMessage](https://typescript-sdk.jellyfin.org/interfaces/generated-client.ForceKeepAliveMessage.html).

These sources describe public HTTP routes and the documented desktop bridge. Puffinbox's server handlers, browser interface, and native-player adapter are original project code. The public API version and route declarations do not establish semantic compatibility for operations that have not been tested.

## Runtime observations

On 2026-09-27, the installed Jellyfin Media Player 1.12.0 on Windows was used to load the server-hosted `/web/` page and exercise its login flow. The client uses the page supplied by the server. On 2026-09-29, against rebuilt image `664a5a3`, the installed client played the 180-second subtitle-selection fixture natively and displayed English cues at their expected source times. In a separate run it also rendered cue 10 after switching from Server default to English mid-playback. During that switch, the player progress display reset to a 0:49 HLS window while the burned-in source timestamp continued; no seeking behavior is inferred. These are narrow observations for one fixture and client, not broad API, codec, or client compatibility. Seeking and audio-track selection remain unvalidated.

On 2026-10-01, official compiled Jellyfin web assets were loaded through a localhost proxy against the synthetic test server. API response bodies were forwarded unchanged. Authentication, movie details, media-category resume lists, HLS decoding, pause, and stop worked. The initial offset-stream resume exposed a timeline mismatch. Full-duration playlists then passed backward/forward seeking and resume through fresh library navigation; stopping saved the matching source time. Authenticated socket notifications subsequently refreshed cached details after a separate session changed the saved position from 48 to 160 seconds. Resume used 160 seconds, and a second cached resume used the stopped position of 196.5659039 seconds. A brief playback error notice followed by successful decoding remains under investigation. The socket proxy forwarded the wire data unchanged and recorded only selected message types and synthetic user-data fields. Public demo `PlaybackInfo` responses were compared as HTTP data; no implementation source was read. The demo reported 12.1.0, while the API schema target remains 12.0.0.

The installed desktop player also decoded the synthetic HLS fixture through Puffinbox's original web interface and the documented native bridge. The initial bounded-clip review saved 125.873 seconds. With full-duration HLS, the native load start option overshot to a segment boundary, and an in-place backward seek failed. Waiting for the decoded origin before resuming, then reopening the stream for backward seeks, passed a 160-second resume, paused seeks to zero and 48 seconds, and a forward seek to 99 seconds. Stopping saved 48 seconds with the item unwatched. The source-time display matched the burned-in timecode. These observations do not validate general codecs, audio continuity, or track changes.

Development of the interface relied on the public documentation linked above and direct interaction with the installed client. No Jellyfin server or client GPL implementation source was inspected, copied, or adapted for this project.

## Bundled browser dependency

The local HLS adapter is the official `hls.js` v1.7.3 npm distribution. Its registry tarball integrity, local SHA-256, upstream reference, and Apache-2.0 license are recorded in [the bundled dependency record](../web/vendor/README.md); full attribution and license text are included in [third-party notices](../THIRD_PARTY_NOTICES.md) and `LICENSE-APACHE`. The script is served from the local web root and is not fetched from a third-party host at runtime.

## Cargo dependencies

Cargo resolves upstream implementations from the lockfile. The former license-exclusion forks, ASCII-only URL adapter and exact-license runtime experiments were retired on 2026-10-05. One SQLx PostgreSQL patch preserves valid Unicode database roles with PostgreSQL-compatible password preparation; its verified upstream archive, changed-file hashes and MIT/Apache notices are retained in `vendor/sqlx-postgres/`. Dependencies retain compatible permissive terms and their notices; full license generation and the source/TLS policy check run in CI. See [licensing.md](licensing.md) and [supplemental notices](../vendor/README.md).
