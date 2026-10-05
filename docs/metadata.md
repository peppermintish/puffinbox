# Catalog metadata and artwork

Metadata refreshes are requested by an administrator through
`POST /Puffinbox/Metadata/Refreshes`. A refresh stores provider results and job
state in PostgreSQL; ordinary item responses read a bounded page of those
results in one batch query. `GET /Items`, `GET /Items/{itemId}`, and search
hints expose the supported display fields directly on each item.

Completed library scans also queue the local NFO and poster importer. Music
scans queue embedded audio tags as well. These automatic jobs use the same
registered-root reads, bounded pages, persistent state and policy checks as
manual refreshes; remote providers and plugins still require an explicit
request. Scan completion and metadata completion are separate states, so
display fields can appear after the scan finishes.

Repeated scans share a queued library/provider job without requesting an extra
pass. A scan during processing or retry requests a full follow-up pass, including
new items before the old cursor. Scan-triggered refreshes read both local metadata
and images, including changes or removal of an existing NFO or poster. The
database regression exercises scan-to-title/rating/artwork delivery, queued-job
coalescing, rescans, sidecar removal and restricted-user reads.

Administrators can also use Jellyfin's `POST /Items/{itemId}/Refresh` for local
metadata and images. Both refresh modes default to `None`. `Default` reloads
local inputs; `FullRefresh` retains existing provider metadata or artwork
unless its corresponding replacement flag is true. Missing provider values
can be populated without replacement. `None` and `ValidationOnly` authenticate
and validate the enabled catalogue item without importing local fields. The
observed existing-album behavior and its limits are recorded in
[the item refresh contract](jellyfin12-item-refresh.md).

Metadata and image work are independent and remain part of the persistent job.
A metadata-only refresh keeps previously imported artwork, including when its
sidecar is missing. An image-only refresh leaves metadata and parental labels
unchanged. Repeated requests reuse an active item/provider job; a request
during processing schedules another pass. Audio metadata work also requests
the embedded tag provider. Remote providers and plugins retain the explicit
extension endpoint and its consent requirements. Trickplay regeneration is
unsupported and returns an error.

For each display field, Puffinbox prefers local NFO data, then current embedded
audio tags, then enabled plugins
whose installed module and manifest hashes still match, then TVMaze data.
Metadata can override an item's displayed name and overview. Genre, premiere
date, production year, tags, official content label, and community score are included when present.
TVMaze's community score is display-only. It is not an age classification and
does not affect parental policy. Parental policy reads only recognized local
NFO labels under `US-MPAA-v1`; unknown labels remain unrated.
Date-only provider premiere values are exposed as UTC midnight timestamps in
the item DTO because the wire field is a date-time and the stored value has no
time-of-day or source timezone.

The primary artwork tag is the SHA-256 digest of the stored artwork bytes.
Clients can request `GET /Items/{itemId}/Images/Primary` or use the
Puffinbox-specific metadata artwork route. The server rechecks the caller's
current item and library permissions before evaluating conditional requests.
It returns a strong ETag and `private, max-age=0, must-revalidate`, allowing a
client-private cache to reuse unchanged artwork only after authorization and
revalidation. Artwork is limited to validated JPEG, PNG, and WebP payloads and
is served with its detected MIME type.

For file items, local NFO and poster sidecars are read relative to the
registered library directory. Movie and episode sidecars use the media stem
(`Name.nfo` and `Name-poster.jpg`, `.png`, or `.webp`). Movies also accept
`movie.nfo` in the media file's directory, ahead of the basename NFO. The
basename is used only when `movie.nfo` is absent. A malformed, unsafe, or
unavailable selected NFO does not trigger fallback; rejected documents
clear the previous parental label.
Series, album, and
artist folders use `tvshow.nfo`, `album.nfo`, or `artist.nfo`, and standard
folder/poster image names. A poster can be imported without an NFO document.
If a poster is unreadable but its NFO is valid, the metadata and policy label
are still saved while the prior artwork is retained.
If the NFO is missing while a poster cannot be safely read, stale NFO display
fields and policy labels are cleared while previously stored artwork is kept.

## Embedded audio tags

A completed music library scan queues an `embedded-audio` refresh. An
administrator can also request this provider for a library or item through
the metadata refresh route. Jobs read audio items in bounded UUID pages,
persist their cursor and retry temporary probe failures. A scan that overlaps
an active job requests another full pass, including new tracks before the
previous cursor. Startup recovery preserves the queued work.

The external FFprobe process uses the same registered roots, forced concrete
demuxer, inherited input descriptor, sandbox, timeout and output limits as
playback probing. File metadata is checked before and after probing and again
against the catalog in the write transaction. Stored tags carry their source
library, path hash, size and modification time. Display, filters, search,
sorting and credits discard a row when a rescan changes that snapshot.
Size and modification time do not constitute a content hash.

Supported fields are title, album, artist, album artist, date/year, genre,
track and disc numbers. A date can be an ISO calendar date or a four-digit
year; a year becomes January 1. Positive indexes accept a number or a
`number/total` pair. Text is limited to 512 bytes, control characters are
rejected, and semicolons remain part of the stored string. Missing album
artist defaults to the artist. Format tags precede tags from the default
audio stream. Comments, content ratings and embedded images are not imported.
Embedded data never supplies parental policy.

Imported titles participate in audio title sorting and search. Album labels
and track/disc ordering use the same fields as item display. Artist credits
resolve only to existing visible artist entries in the same library; unknown
or hidden explicit credits do not fall back to an unrelated folder artist.
When no embedded title is available, the refresh stores a bounded filename
stem without its extension. This fallback is selected only if no valid
provider supplies an actual title; it does not lower the priority of other
embedded fields. Display, title ordering and metadata-title search use the
same selection rule.
Tag-based artist and album creation remains incomplete. The tested reference
and remaining differences are in
[jellyfin12-embedded-audio.md](jellyfin12-embedded-audio.md).

## Catalog filters

`GET /Items/Filters` returns distinct genre names, tags, official ratings, and
production years. `GET /Items/Filters2` returns genre names with opaque IDs
and tags. Both routes require authentication, use `no-store`, and apply the
selected user's current library, path, and parental restrictions. An
administrator can select another user; an ordinary user cannot.

The scope accepts `userId`, `parentId`, `includeItemTypes`, `mediaTypes`, and
`recursive`, with PascalCase aliases. Recursion defaults to true. Choices
come from the full authorized scope before item pagination. More than 4,096
distinct choices in a category returns 503 rather than a truncated menu.

Item browsing accepts pipe-separated `Genres`, `GenreIds`, `Tags`, and
`OfficialRatings`, plus comma-separated `Years`. Values within a category
are alternatives; different categories are combined. Genre names and IDs
can be used together. Name matching ignores case in PostgreSQL. An unknown
genre ID returns no items. Each selection accepts at most 32 values, and
names are limited to 128 bytes. Years must be between 1800 and 2300.

Genres use the same provider precedence as item display metadata. Tags and
official ratings come from local NFO data. A preferred premiere date supplies
the production year; the local NFO `<year>` is the fallback. Movie sidecars
can include repeated `<genre>` and `<tag>` elements. Tags are decoded,
bounded, and deduplicated during import.

Audio and subtitle language choices are empty because stream languages are
not yet stored in the catalog. Nonempty language selections return 400.
The filter routes also reject Live TV classification selectors such as
`isAiring` and `isSports`; their behavior remains unsupported here.
