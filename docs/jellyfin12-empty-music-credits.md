# Empty music credits

Five public HTTP reads of the original untagged `04 Plain Track.flac` in
the isolated official Jellyfin 12.0.0 fixture returned empty artist credits.
The reference image was
`jellyfin/jellyfin@sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`.
No Jellyfin implementation source was read.

| Request | Reference result |
| --- | --- |
| Track detail | `Artists`, `ArtistItems` and `AlbumArtists` are empty arrays |
| Track selected by the physical folder artist's `ArtistIds` | No items; total and start index zero |
| Track selected by the physical folder artist's `AlbumArtistIds` | No items; total and start index zero |
| `/Artists`, scoped to the album and searched for `Folder` | No items; total and start index zero |
| `/Artists/AlbumArtists`, with the same scope and search | No items; total and start index zero |

The reference's thirteen existing track responses and original fixture
hashes were unchanged before and after these reads. There was no library
refresh, metadata edit, playback, permission change or saved-state edit.
Requests and responses are retained in the ignored local record
`.local/empty-music-credit-contract-20261003/results.json`.

The preceding Puffinbox image, core `645b6abe`, differed in all five cases.
It inferred credits from the physical artist folder despite having current
embedded metadata with empty credit arrays. The replacement selects a
metadata role before resolving its names. A current embedded empty role
therefore remains empty. Nonempty local NFO roles still take precedence;
missing or stale embedded roles still permit the existing folder fallback.
Explicit names that cannot resolve to visible artists do not fall back to
another artist.

The database regression covers empty detail fields, both selectors, scoped
role-list counts, local NFO precedence and a changed source identity. These
changes do not create missing tag-named artists or establish complete music
DTO compatibility. Album naming, wider tag formats and metadata-provider
differences remain open.

Core `444d7012` passed all five normalized reference comparisons, alongside
sixteen exact artist-list comparisons and the two previously qualified
uncounted-order cases. The retained backend upgrade preserved its server
identity, mounts, media hashes, user data, grants, playlists and studio
favorites without a refresh or reset.

All thirteen source checks, thirty-five container checks and twenty-nine
local HTTPS checks passed. Both official web and Qt 6 Desktop completed the
original four-track FLAC queue with automatic advancement and fifteen
successful playback responses each. Desktop logged four audio EOF events;
web audio advanced unpaused without a media error. Independent reads
confirmed PlayCount 12 to 13 after Desktop, then 13 to 14 after web, for
every track, with Played true and zero saved resume.

Source, image, container, HTTPS and retained-client records are under
`.local/empty-music-credit-*-20261003`; the client evidence join verifies
260 frozen source hashes and continuous playback state. Packaged
documentation predates this entry. Audible quality, general Desktop video
acceptance and broader music behavior remain unvalidated. The project
remains partial and unreleased.
