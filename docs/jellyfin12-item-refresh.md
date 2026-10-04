# Item metadata refresh

The target request is `POST /Items/{itemId}/Refresh` in the public
[Jellyfin 12 OpenAPI schema](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json).
It requires an administrator, returns an empty 204 after accepted work and
returns 404 for a missing item. Local album sidecars are described in
[Jellyfin's NFO documentation](https://jellyfin.org/docs/general/server/metadata/nfo/).
No Jellyfin implementation was inspected or copied.

## Reference observations

A separate pinned Jellyfin 12 container used one original plain FLAC and an
original `album.nfo`. Remote metadata and image fetchers were disabled. The
container had no published port and used a private internal network. The
earlier reference fixtures and retained Puffinbox client database were not changed.

An explicit full refresh with metadata replacement imported the album title,
overview and year. Artist fields and both artist lists remained empty despite
artist text in the album sidecar. Puffinbox's existing local provider matched
those projected fields; its public refresh URL initially returned 404.

Ten request-binding observations cover the four named modes, lowercase names,
numeric mode values, replacement booleans and invalid options. Invalid mode
and boolean text returned 400. A missing item returned 404 and a synthetic
non-administrator returned 403.

Five sidecar revisions separately tested metadata effects. Each began with a
verified original baseline and captured ten public reads over five seconds.

| Metadata request, with image mode None | Existing album result |
| --- | --- |
| None | Existing title, overview and year retained |
| ValidationOnly | Existing title, overview and year retained |
| Default | Revised title, overview and year imported |
| FullRefresh without replacement | Existing title, overview and year retained |
| FullRefresh with replacement | Revised title, overview and year imported |

Original sidecar bytes and the original projected album values were restored
afterward. Private records, fixture hashes, exact image identity and the
request operation are under `.local/album-nfo-contract-20261004`.

## Puffinbox scope

The route queues bounded local provider work with persistent metadata/image
options. Duplicate requests coalesce; requests against running work request
another pass. Existing extension jobs retain their original full-refresh
defaults. Metadata-only writes preserve artwork, image-only writes preserve
metadata, and rejected NFO documents still clear the local parental label.
Enabled-library and active-server-run constraints apply before mutations.

The PostgreSQL HTTP regression failed on the original 404 before the route was
added. It also exercises permissions, invalid options, duplicate work,
metadata-only artwork preservation, image-only field preservation, replacement
and missing-sidecar cleanup.

Core `271aed34`, separate operator image `587d6927` and server `99440a3e`
passed 35 isolated container checks and 29 HTTPS checks. On a separately owned
album database, ten binding results, all five existing-album metadata effects
and the complete recorded album projection matched the preserved reference.
The original sidecar bytes were restored. Album values and server identity
survived restart without resetting state. Evidence is under
`.local/item-refresh-contract-current-20261004` and the matching image,
container and HTTPS directories.

Both unchanged official clients completed the original four-track FLAC album
on this build, with automatic advancement and fifteen successful playback
reports each. Desktop logged four audio EOF events; web audio advanced without
a media error. Independent reads confirmed exactly one added play per track
per client, Played=true and zero music resume. All 50 saved item rows matched
the expected plays without a reset. The retained backend kept its identity,
grants, mounts, playlists, studio favorites and original media hashes. Desktop
closed normally with Remember Me off and unchanged settings. Joined evidence
binds 263 source fingerprints under `.local/item-refresh-client-20261004`.
These playback checks establish regression coverage for this binary; they do
not establish client use of the refresh request itself.

These observations establish existing-album local metadata behavior. They do
not establish recursive refresh, every field or item type, remote-provider
selection, image-mode equivalence, trickplay, absent-album creation, embedded
tag refresh equivalence or complete Jellyfin validation semantics. Puffinbox
returns 400 for trickplay regeneration. FullRefresh without replacement uses
provider-row availability; partially populated provider rows need further
comparison. Runtime licensing and all release gates remain open.
