# Capability and validation matrix

Puffinbox is partial and unreleased. Local checks on 2026-10-01 passed; their scope and reproduction steps are in [acceptance.md](acceptance.md). Passing checks for individual paths does not establish a complete Jellyfin replacement.

## Current-source gates

The Rust workspace passes formatting, strict Clippy, and 211 standard tests. All 20 PostgreSQL integration cases and the database-backed recorder unit test passed against a disposable database. Python, browser, and book-reader checks passed. The current static container passed all 20 HTTP and restart acceptance checks. Source and scratch-container TLS checks passed.

The MIT/Apache-2.0 Cargo allowlist passes without exceptions. The linked Rust Unicode tables carry Unicode-3.0, and musl has additional component licenses. The runtime boundary is still open; see [licensing.md](licensing.md). No release tag or archive has been created. [Cloud CI passed at `fe76ccc`](https://github.com/peppermintish/puffinbox/actions/runs/36780885839); the newer preference and catalog changes await their own cloud result.

## Jellyfin API target

The target is the public [Jellyfin 12.0.0 OpenAPI document](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json). The [route report](generated-api-route-coverage.md) finds 79 exact method/path declarations among 364 operations. This counts declarations, not compatible behavior. Response semantics, query combinations, errors, and client expectations must be tested separately.

## Requested scope

| Area | Current behavior | Remaining limits |
| --- | --- | --- |
| Playback and transcoding | Device-profile negotiation, original-file streaming and byte ranges, playback progress/resume, H.264/AAC HLS, job cancellation and graceful restart. The 20-check container run covers synthetic MP4 and MKV files. | FFmpeg/ffprobe are external. Broad codecs, seeking, audio-track changes, hardware acceleration, and native clients remain unvalidated. |
| Subtitles | Embedded text extraction and selected subtitle delivery are covered by synthetic HLS acceptance. | Broader subtitle formats, styling, seeking, and client track switching need acceptance. Earlier visible desktop cues apply only to their old test image. |
| Multi-user access | Sessions, revocation, administrator actions, library visibility, parental categories, playback/download policy, private playlists, and per-user/client preferences have PostgreSQL coverage. New users default to remote access disabled. | Preferences persist, but not every playback and view setting is applied yet. Local PIN authentication is unavailable. External security review and broad client behavior remain unvalidated. |
| Remote access | Public origin, trusted proxies, secure cookies, CORS, and remote-access policy are configurable. Compose publishes to loopback by default. | No external-network or TLS reverse-proxy acceptance result is available. |
| Metadata | Local NFO and bounded provider/catalog paths, refresh jobs, and stored artwork metadata. | Provider coverage and artwork workflows are limited. TVmaze data requires attribution; end-user credit display has not been validated. |
| Live TV/DVR | IPTV sources, guide/catalog state, policies, schedules, recording lifecycle, retries, and concurrent timer ownership have database coverage. | Physical tuners, real providers, long recordings, recovery under faults, and native clients need acceptance. |
| DLNA | SSDP discovery and bounded ContentDirectory behavior have local tests. | AVTransport, renderer control, DLNA transcoding, physical devices, IPv6 discovery, and broad interoperability are incomplete. |
| Plugins | Bounded Wasm extensions and plugin persistence have local coverage. | Jellyfin .NET plugins are unsupported. A full extension ecosystem and external plugin acceptance are incomplete. |
| Music | Audio catalog/playback and private ordered playlists; the browser can create, edit, reorder, remove, and play queues. | Sharing, video playlists, richer music browsing/metadata, and native playlist acceptance are incomplete. Collection listing and playlist deletion include Puffinbox extensions. |
| Photos | Safe raster preview, download headers, and offline packages. | Broad photo management, thumbnails, and native-client rendering remain unvalidated. |
| Books | A bounded PDF/EPUB reader with library, parental, and download policy checks; 8 Node reader cases pass. | PDF assets exclude non-allowlisted CMaps/fonts/decoders. Some PDFs cannot render fully. EPUB media, annotations, DRM, conversion, and broad client support are incomplete. |
| Offline sync | Browser-origin IndexedDB transfers with chunk and whole-file SHA-256, interrupted resume, cached streaming, ranges, and account-change handling. | Copies belong to the browser/site origin. Real storage pressure, eviction, cross-device sync, and external clients need acceptance. Server deletion does not erase a downloaded copy. |
| Deployment | A static Linux server image, external PostgreSQL, and a separately assembled FFmpeg test runtime. Active HLS shutdown and resume passed on the current image. | Whole-runtime license closure and production operations remain open. |
| Official clients | Official Jellyfin web authenticated, registered session capabilities, and listed three synthetic movies through a localhost proxy. The installed player exposed Puffinbox's login form. The original Puffinbox web client completed a synthetic MP4. | Official web details and playback remain incomplete. The installed player's captured display was blank; native playback is unvalidated. |
| Scale | An earlier bounded [2,048-file scanner benchmark](scanner-scale.md) measured about 4,567 rows per second. | No result supports a 1 PB library, billions of files, or thousands of concurrent streams. |

## Licensing closure

Project-authored code and original adapters are MIT OR Apache-2.0. Bundled browser assets have their own accepted notices. Cargo license closure passes; runtime closure does not. External PostgreSQL, FFmpeg distributions, and metadata data have separate boundaries. See [the licensing inventory](licensing.md) and [release gates](release-gates.json).

Earlier source checkpoints and image-specific client observations are recorded in [validation-history.md](validation-history.md). They do not validate the current source.
