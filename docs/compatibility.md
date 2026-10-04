# Capability and validation matrix

Puffinbox is partial and unreleased. The target is the pinned public Jellyfin 12 API schema; version reporting does not certify compatibility. The current report matches 107 of 364 exact method/path declarations. Many missing operations and behavior differences remain.

## Current evidence

The 2026-10-05 corrected rustls source passed formatting, strict Clippy, 247 standard Rust cases, 29 database cases, 27 Python cases and nine source browser groups. The rebuilt operator image passed 35 semantic container checks, including active FFmpeg shutdown and restart/resume. The core passed 29 local HTTPS proxy checks and image/license inspection. See [acceptance](acceptance.md) for exact image IDs, fingerprints and reproduction.

Both official clients completed the synthetic four-track FLAC album with fifteen successful playback reports each and independently verified play counts. Official web displayed advancing HLS video and decoded audio. Desktop's MP4 requests and playback reports succeeded, but its captured video surface was black; visual video acceptance remains open. Older client observations apply only to their named images.

Current local checks clear the [core distribution license gate](licensing.md) under the corrected compatible permissive policy. Cloud validation of the correction is pending. Behavioral compatibility, full features, external deployment/security and production scale remain open. No release is ready.

## Requested scope

| Area | Implemented and tested scope | Remaining limits |
| --- | --- | --- |
| Playback/transcoding | Device-profile negotiation, authenticated originals/ranges, progress and resume, known-duration H.264/AAC HLS, cancellation and restart. Current synthetic MP4/MKV/FLAC acceptance includes source timestamps and nonsequential HLS segments. | External FFmpeg/ffprobe required. HLS is bounded to four hours and 2 GiB per session without cache eviction. Broad codecs, HDR, hardware acceleration, track changes, audio continuity and general Desktop video need acceptance. |
| Subtitles | Embedded text extraction and selected delivery have current container coverage. | Broader formats, styling, seeking and client track switching remain incomplete or unvalidated. Earlier visible desktop cues belong to their old image. |
| Multi-user | Sessions/revocation, library and parental policies, downloads, private/shared playlists and persistent user/client preferences have database coverage. Administrator role changes protect the last enabled administrator. | Local PIN authentication is unavailable; not every view/playback preference is applied. Full policy DTO round trips, wider clients and external security review remain open. |
| Remote access | Public origin, trusted proxies, secure cookies, CORS and remote-account policy are configurable. New users default to remote access disabled. Twenty-nine current local HTTPS checks cover spoof rejection, scoped tokens and active-session policy changes. | Synthetic local addresses do not validate Internet deployment, every proxy configuration or external security review. |
| Metadata | Local NFO, embedded audio, bounded provider/catalog paths, refresh jobs and permission-aware genres/tags/ratings/years/studios. Real metadata precedence and policies have database coverage. | Provider/artwork coverage, stream-language and Live TV classification filters, metadata writes and TVmaze attribution display remain incomplete. Menus are bounded to 4,096 choices. |
| Live TV/DVR | IPTV source/guide/policy/catalog/timer lifecycle, retries and concurrent recording ownership have database coverage; bundled browser checks pass. | Physical tuners, real providers, long recordings, faults and native clients need acceptance. |
| DLNA | SSDP and bounded ContentDirectory have local tests. | Renderer control, AVTransport, DLNA transcoding, physical devices and IPv6 discovery remain incomplete. |
| Plugins | Bounded compiled-Wasm metadata hooks have database and preceding container trust/enable/refresh/restart/disable coverage. | Text Wasm and Jellyfin .NET plugins are unsupported. Full ecosystem and external plugin acceptance remain open. |
| Music/playlists | Direct audio and bounded AAC conversion, owned/shared ordered queues, paging, sorting, explicit credits, embedded display metadata, user-data and scoped Similar/Instant Mix/Collections have database and current media coverage. Both official clients completed the tagged four-track FLAC album on the current image. | Broader codecs/metadata, audible continuity, tag-named album creation, duplicate names, public/video playlists and native sharing management remain incomplete. Instant Mix has a stable affinity rule, 100-track cap and at most 32 album/artist seeds; it differs from reference recommendation behavior. Collections includes containing BoxSet ancestors, not arbitrary managed membership. |
| Photos | Authenticated raster preview, original primary images, download headers and offline packages have database/container coverage. A preceding Desktop image displayed one synthetic PNG. | Resizing, thumbnails, broad formats, photo management and a fresh current-image Desktop photo check remain open. |
| Books | Bounded PDF/EPUB reader and library/parental/download policies have source and database coverage. | CMaps, base fonts and optional decoders await restoration and validation under the corrected policy. Some PDFs cannot render fully. EPUB media, annotations, DRM, conversion and broad clients remain incomplete. |
| Offline sync | Browser-origin IndexedDB transfers, chunk/whole-file hashes, interrupted resume, cached ranges and account changes have browser coverage. | Storage pressure/eviction, cross-device sync and external clients need acceptance. Server deletion does not erase a downloaded copy. |
| Deployment | Static non-root core image with full notices; separate PostgreSQL service and operator FFmpeg image. Active HLS shutdown/resume passed on the current operator image. | Production operations and broader platforms remain unvalidated. Operator FFmpeg keeps its external distribution/codec obligations. |
| Scale | [20,000- and 99,000-file scanner fixtures](scanner-scale.md) passed exact indexing and stale cleanup on one local host. | Empty-file fixtures do not establish 1 PB storage, billions of files or thousands of simultaneous streams. |

## Qualified reference behavior

The retained contract documents record exact projections and their limitations: [music credits](jellyfin12-music-credits.md), [tagged artists](jellyfin12-tagged-artists.md), [artist lists](jellyfin12-artist-lists.md), [album counts](jellyfin12-album-counts.md), [embedded audio](jellyfin12-embedded-audio.md), [empty credits](jellyfin12-empty-music-credits.md), [catalog booleans](jellyfin12-catalog-booleans.md), [user data](jellyfin12-user-data.md), [playback](jellyfin12-playback-behavior.md), [studios](jellyfin12-studios.md) and [refresh](jellyfin12-item-refresh.md).

Scoped zero-limit, repeated-Fields, count and album-queue projections have preceding-image reference comparisons; they do not cover every query combination. Folder filtering, physical artist-parent layouts, combined Ids/SearchTerm, other zero-limit filters, specialized paging, wider arrays/Fields, legacy album fallback, collation and default catalog membership remain qualified. Arbitrary virtual-item writes and full metadata mutation are incomplete. Duplicate ended stops preserve later user edits, differing from the observed reference.

The present official home requests to `/Shows/NextUp` and `/SyncPlay/List` return 404. Complete episode recommendation/history and synchronized playback are absent. The [release gates](release-gates.json) remain open until the full requested behavior is implemented and validated.

## Licensing

Project code is MIT OR Apache-2.0. Compatible permissive dependencies retain their own notices; standard rustls provider internals are accepted. All project Cargo targets forbid unsafe Rust, and project C/assembly is removed. PostgreSQL, operator FFmpeg and official clients remain separate external inputs. See [licensing](licensing.md), [current acceptance](acceptance.md), and [historical validation](validation-history.md).
