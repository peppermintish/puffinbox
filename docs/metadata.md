# Catalog metadata and artwork

Metadata refreshes are requested by an administrator through
`POST /Puffinbox/Metadata/Refreshes`. A refresh stores provider results and job
state in PostgreSQL; ordinary item responses read a bounded page of those
results in one batch query. `GET /Items`, `GET /Items/{itemId}`, and search
hints expose the supported display fields directly on each item.

For each display field, Puffinbox prefers local NFO data, then enabled plugins
whose installed module and manifest hashes still match, then TVMaze data.
Metadata can override an item's displayed name and overview. Genre, premiere
date, official content label, and community score are included when present.
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
(`Name.nfo` and `Name-poster.jpg`, `.png`, or `.webp`). Series, album, and
artist folders use `tvshow.nfo`, `album.nfo`, or `artist.nfo`, and standard
folder/poster image names. A poster can be imported without an NFO document.
If a poster is unreadable but its NFO is valid, the metadata and policy label
are still saved while the prior artwork is retained.
If the NFO is missing while a poster cannot be safely read, stale NFO display
fields and policy labels are cleared while previously stored artwork is kept.
