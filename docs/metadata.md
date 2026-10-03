# Catalog metadata and artwork

Metadata refreshes are requested by an administrator through
`POST /Puffinbox/Metadata/Refreshes`. A refresh stores provider results and job
state in PostgreSQL; ordinary item responses read a bounded page of those
results in one batch query. `GET /Items`, `GET /Items/{itemId}`, and search
hints expose the supported display fields directly on each item.

For each display field, Puffinbox prefers local NFO data, then enabled plugins
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
