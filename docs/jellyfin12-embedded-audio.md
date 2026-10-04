# Jellyfin 12 embedded audio metadata observations

These observations come from public HTTP requests to an isolated official
Jellyfin 12.0.0 container. No Jellyfin implementation source was used.
The image digest was
`jellyfin/jellyfin@sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`.
The test used four original five-second FLAC files in
`Music/Folder Artist/Folder Album`, with external metadata and image fetchers
disabled. The container had an internal network and no published host port.

The initial catalog read occurred before audio metadata finished updating.
The results below use fresh authenticated reads after the public
`RefreshLibrary` scheduled task returned to `Idle`. Fixture hashes were
unchanged. There was no playback or metadata editing during observation.

| File | Embedded fields | Settled public response |
| --- | --- | --- |
| `blob-a.flac` | Title `Embedded Alpha Title`; artist `Embedded Lead; Embedded Guest`; album artist `Embedded Album Artist`; album `Embedded Album`; date `2021-04-05`; track `3/12`; disc `2/4`; genre `Rock; Jazz` | The tagged title and album; one artist string retaining the semicolon; one album artist; index 3, parent index 2; year 2021 and UTC-midnight premiere date; one genre string retaining the semicolon |
| `blob-b.flac` | Title `Embedded Beta Title`; artist `Embedded Lead`; album `Embedded Album`; date `2023`; track `04/12`; disc `1`; genre `Jazz`; no album artist tag | Album artist defaults to `Embedded Lead`; index 4, parent index 1; year 2023 and premiere date January 1 |
| `blob-c.flac` | Title `Embedded Conflicting Title`; artist `Embedded Lead`; album artist `Embedded Album Artist`; album `Embedded Album`; date `2022`; track `7/12`; disc `3/4`; genre `Embedded Genre` | Embedded values remained selected despite a conflicting sibling `blob-c.nfo` song document |
| `04 Plain Track.flac` | No tags | Filename title; no album or artist credits; null track/disc indexes and year; empty genres; the reference's default year-one premiere timestamp |

Embedded comments did not become an overview. The reference created artists
`Embedded Lead` and `Embedded Lead; Embedded Guest`, album artists
`Embedded Album Artist` and `Embedded Lead`, and one album `Embedded Album`.
This establishes tag-based naming for these fixtures, even though the folder
names differ. It does not establish all supported tag aliases, formats,
multi-valued tags, separator rules or NFO behavior under other library options.

The private fixture, protected synthetic accounts, failed setup observations,
initial catalog read and settled responses are retained under
`.local/embedded-audio-contract-20261003`. The earlier reads are preserved and
are not counted as the settled result. Accounts and downloaded clients are
excluded from source control.

Puffinbox imports bounded embedded title, album, artist, album artist, date,
genre and track/disc fields through its existing confined FFprobe invocation.
Local NFO fields take precedence. That is an explicit Puffinbox policy; the
conflicting song NFO did not override embedded fields in this reference.
Embedded credits resolve to visible same-library physical or persistent
[tag-named artists](jellyfin12-tagged-artists.md). Tag-named album creation,
embedded cover art, wider format coverage and general reference equivalence
remain incomplete. Folder-derived relationships and filename track-index
fallbacks without current embedded metadata remain qualified differences.

## Audio SortName

Public reads of the original four tracks returned SortName prefixes made from
the tagged disc and track numbers. For example, Embedded Beta Title returned
`0001 - 0004 - Embedded Beta Title`. Sorting by SortName put Beta before Alpha,
while sorting by Name used their display titles. The untagged file returned
`04 Plain Track`, with no inferred numeric prefix.

A separate reference library contains nine original one-second FLAC files.
Its observations establish these additional cases:

| Disc | Track | Title | SortName |
| --- | --- | --- | --- |
| Missing | Missing | Sort Plain | `Sort Plain` |
| Missing | 2 | Sort Track Only | `0002 - Sort Track Only` |
| 2 | Missing | Sort Disc Only | `0002 - Sort Disc Only` |
| 1 | 2 | Sort Both | `0001 - 0002 - Sort Both` |
| 10001 | 10002 | Sort Large | `10001 - 10002 - Sort Large` |
| 0 | 0 | Sort Zero | `0000 - 0000 - Sort Zero` |
| 0 | 2 | Sort Zero Disc | `0000 - 0002 - Sort Zero Disc` |
| 1 | 1 | The Zebra | `0001 - 0001 - The Zebra` |
| 1 | 3 | A, Small: Test! | `0001 - 0003 - A, Small: Test!` |

The fixture records ascending and descending SortName, Name, IndexNumber and
combined disc/track ordering. Original reference files and item responses
stayed unchanged; no playback or user-data edits occurred. The added library
is retained for replay. Exact files, tags, hashes and public responses are
under `.local/audio-sort-contract-20261003`. No implementation source was read.

Puffinbox's audio SortName uses the same valid provider fields as display
metadata, preserving each present number with at least four digits. Missing
numbers add no prefix; filename-derived track indexes do not add a prefix.
Embedded zero indexes are retained. Name ordering continues to use display
titles. Invalid or stale provider values do not participate. Local NFO parsing
still accepts positive indexes only. Forced sort titles, broader collation,
negative indexes and complete reference ordering remain unvalidated or
qualified; numeric sorts on core `6a8a4e3b` retain Puffinbox's preceding null-last
policy.

On core `6a8a4e3b`, nine byte-identical reference files were copied into the
retained client's existing music grant and imported automatically after one
scan. All nine SortName, title and disc/track fields matched fresh reads.
Ascending and descending SortName and Name orders matched, as did the
SortName page derived from the full reference order. IndexNumber and combined
disc/track sorts retain qualified null and tie differences. Existing file
hashes, grants and saved user data stayed unchanged. Exact mappings, queries
and responses are under `.local/audio-sort-client-20261003`.

## Numeric music ordering

Thirteen further public queries used the same nine reference files, without
adding a library or changing media or user data. Missing IndexNumber and
ParentIndexNumber values sort first in ascending order and last in descending
order. Explicit zero remains a number, after missing values in ascending
order. Ties use SortName ascending by default, including when the requested
numeric direction is descending. For example, the three tracks numbered 2
remain Sort Zero Disc, Sort Both and Sort Track Only in both numeric
directions. An explicit descending SortName reverses those ties; an explicit
Name uses display-title order instead.

The observations also cover separate disc/track/name directions, a page of
missing indexes and a page crossing equal numeric values. Every query reports
the same nine-item total. Independent before/after item reads and file hashes
were unchanged, including the original reference album. Exact queries, ordered
IDs, fields and fixture joins are under `.local/audio-numeric-contract-20261003`.
This contract covers the synthetic audio fixture; it does not establish wider
collation or episode sorting compatibility. No Jellyfin implementation source
was read.

On core `dcd0bad2`, all thirteen numeric queries and five preceding SortName,
Name and numeric reads matched the exact reference order and total. All nine
title, SortName and disc/track fields matched too. The same image matched all
29 preserved personal-filter observations. The upgrade and read-only ordering
checks retained every compared user-data field, grant and media hash, with no
state reset. Records are under `.local/audio-numeric-client-20261003`.

The item query uses missing-first ascending and missing-last descending
numeric order. It appends ascending SortName for numeric ties when the query
does not already supply SortName, before its stable item-ID tie. PostgreSQL
coverage includes missing and explicit zero values, mixed directions, both
pages, display Name ordering, personal filters, stale provider identity,
malformed metadata and hidden parents. The numeric null rule also applies to
episodes; the public observation above establishes only this audio contract.

## Physical album names

Fresh public reads on 2026-10-04 used the unchanged reference container and all
thirteen original FLAC files. The physical folder `Folder Album` returns Name
`Embedded Album` and SortName `embedded album`; the other physical album returns
`Sort Contract Album`. The original four tracks retain the first album's ID,
including the plain track whose public Album field remains null.

Twenty-four queries cover Name, SortName, Album and IndexNumber in both
directions, with a full page and each one-item page. Name and SortName reverse
with descending order. The two albums have null Album and IndexNumber fields,
so those sorts retain the ascending default name tie in both directions.
Five unscoped album searches return one album for Embedded and Sort and no
albums for Folder, Alpha or an impossible name.

Three additional queries combine the two album IDs with a search term. The
reference returns both albums for Embedded and Sort and neither for Folder.
Puffinbox still intersects IDs with its search result; two selections differ.
These observations remain qualified and are excluded from the 36 matching
responses below. Exact reads are under `.local/album-tag-name-contract-20261004`.
No reference media, grants or metadata were changed. The initial global Alpha
search finds an older Alpha recent album only in the retained client catalog.
Ten fresh searches scoped to the two corresponding physical artists match;
the original global difference remains preserved.

Puffinbox keeps each physical album's identity and derives its display name
from a single agreed nonempty current embedded name on permitted tracks.
Provider titles, including local NFO titles, take precedence. Hidden and
restricted tracks, denied libraries, stale source snapshots, malformed values
and ambiguous names cannot supply the name. A missing or ambiguous name keeps
the folder name and its stored sort name. This conservative ambiguity policy
has database coverage; it is not established as Jellyfin's ambiguity behavior.
The same name supplies DTOs, exact-name selection, album search and ordering
before paging. Audio Album sorting can use its own visible parent's name.
On preceding core `aa0e55dc`, the plain track's returned Album fell back to its
physical folder name and differed from the reference null. Both detail and Play queues now start
with the plain track, followed by Beta, Alpha and Conflicting. Two queue reads
match ordered IDs, names and numeric fields. Unrequested SortName presence and
disabled-total semantics remain qualified; their broader failed comparison is
preserved.

On preceding core `aa0e55dc`, both album details, all 24 order/page queries and all ten
artist-scoped searches match the projected reference fields and ordered IDs. Both
official clients display Embedded Album and complete its four-track FLAC queue.
Each client adds one play per track; all 50 saved rows, grants, IDs and fixture
hashes remain intact. Records are under `.local/album-tag-name-client-20261004`.
Tag-named album creation, merged folders, conflicting tags, album-only artist
registration, parent DTO naming, artwork, wider formats and full music behavior
remain incomplete or unvalidated.

## Audio Album and requested sort name

Fresh public reads on 2026-10-04 used the unchanged reference container and its
thirteen original FLAC files. Audio details include SortName. The plain track
omits Album while retaining AlbumId. Four ordinary album-child lists select no
Fields, SortName alone, Genres/Overview, or a larger set containing SortName.
SortName appears only in the two lists requesting it. The original media hashes,
reference grants and metadata are unchanged. Exact reads are under
`.local/audio-album-field-contract-20261004`.

Puffinbox's current permitted embedded audio metadata supplies Album only when
it contains a valid album name. AlbumId still comes from permitted navigation.
The existing physical-folder fallback remains for legacy and stale metadata;
that fallback is not established as reference behavior. Ordinary GET /Items
and GET /Users/{userId}/Items select SortName through Fields without changing
their database sort or paging keys. Detail endpoints retain SortName. Playlist
branches and specialized latest, similar and artist endpoints retain their
preceding behavior; other ItemFields are not implemented by this change.

Preceding core `6c44dc12` matches all seventeen detail/list field observations. Its two
original album-queue projections match ordered IDs, Name, Album, SortName and
numeric fields. Disabled-total semantics are excluded: the reference returns
four while this build omits the count. Twelve additional reference queries on
both ordinary list routes use enabled/disabled counts and two-item pages at
offsets zero, two and four. Each returns a total of four, including the empty
page. The later ordinary-count implementation and wider scoped replay are recorded below.
Exact records are under `.local/album-queue-count-contract-20261004`.

Both official web and Qt 6 Desktop completed the four-track FLAC queue on the
new image with fifteen successful playback reports each. Fresh reads matched
all 50 saved rows to one added play per track per client; grants, identity and
fixture bytes were retained without a reset. Joined records are under
`.local/audio-album-field-client-20261004`. Wider field selection, count semantics,
legacy metadata behavior and complete music compatibility remain partial.


## Ordinary item totals

Public reads on the unchanged official Jellyfin 12.0.0 runtime cover both
ordinary list routes, default/enabled/disabled count flags, two-item pages and
empty final pages. Album and artist children, recursive trees, meaningful and
whitespace-only searches, explicit IDs, combined parent/ID selection and the
original music library supplied 252 observations. The reference's media bytes,
metadata and grants were unchanged. Exact records are under
`.local/item-total-contract-20261004`.

For the observed modes, missing or enabled flags return the full visible total.
A meaningful normalized search also returns that total with counting off.
Nonrecursive parent children without explicit IDs do likewise. Other ordinary
lists with counting off return their page size, including zero on an empty
page. Explicit IDs retain page counts even with a parent and nonrecursive mode.
Whitespace-only searches do not force a full recursive count.

Puffinbox implements those modes after visibility checks on GET /Items and
GET /Users/{userId}/Items. Playlist branches and specialized artist/latest
responses retain their preceding behavior. SQL visibility, ordering and
selection are unchanged. PostgreSQL coverage checks default/true/false flags,
both routes, three offsets, eight selection modes, hidden and restricted tracks
and unchanged saved user data.

Core `2d3babe8` matches all 168 selected count projections. The other 84 retained
queries have predefined qualifications for extra catalog membership, physical
artist selection and different music-library layout; they are not passing
comparisons. All 50 saved rows are unchanged by the reads. The preceding image
matched 112 of those 168, with 56 disabled-count differences. Seventeen field
projections remain matching. Two album queue projections now include matching
TotalRecordCount and StartIndex as well as their recorded item fields.

Desktop completed one four-track FLAC replay and official web completed three,
with fifteen successful reports each and one added play per track per run.
The final web replay includes advancing, unpaused media without an error; the
first two media observations were incomplete and remain qualified. Joined
records bind all 50 saved rows to those accumulated plays without a reset.
Desktop's main album list still returned 400 for repeated Fields values, as in
its preceding trace. The album was reached through the home row. This is queue
playback evidence, not album-list navigation acceptance.

Thirty-six further reference reads of Limit=0 remain outside this implementation.
The original nonrecursive album-child mode returns all four children, while
recursive and explicit-ID modes return empty pages; enabled totals remain four
and disabled recursive totals become zero. Puffinbox's minimum limit of one
still differs. Wider zero-limit behavior, combined IDs/search selection, other
ItemFields, complete collation and full music compatibility remain open.


## Repeated Fields

The official Desktop Albums request sends `fields=MediaSourceCount` and
`fields=PrimaryImageAspectRatio` as separate query values. The preceding
Puffinbox binder rejected it with a duplicate-field 400, leaving the main
Albums list empty. The same failure appears in the preceding client traces.

Twenty-four public reads on the unchanged official Jellyfin 12.0.0 runtime
cover both ordinary list routes, Audio album children and MusicAlbum artist
children. CSV, repeated lower-case values, requested SortName, mixed aliases,
duplicate members and empty members all return 200. The selected projection
contains status, ordered Id/Name/Type, SortName presence/value, TotalRecordCount
and StartIndex. Known parent Album differences were excluded before the new
image comparison; the initial wider comparison remains preserved. No reference
implementation source was read and no reference fixture or grant changed.

Puffinbox now combines only repeated `Fields`/`fields` values before normal
deserialization on GET /Items and GET /Users/{userId}/Items. Each decoded key
and value is re-encoded, so embedded ampersands, equals signs and plus signs
cannot become query options. Duplicate scalar options keep their existing
rejection. Two binder regressions cover delimiter preservation and scalar
validation. Twenty database-backed CSV/repeated equivalence checks cover both
routes and item types. SQL selection, visibility, sorting and paging are
unchanged; no Cargo dependency or feature changed.

Core `9b2cd02c` matches all 24 scoped binding projections, compared with four
on the preceding image and twenty duplicate-field errors. All 168 scoped count
projections, seventeen audio-field projections and both original detail-page
album queues still match. Nine folder-filter and 84 count-catalog/layout
qualifications remain. Broader DTO equivalence, other array options and field
selection beyond the recorded projection remain incomplete.

Both official clients display all eight retained albums in the main list.
Desktop opened Embedded Album from it and decoded all four original FLAC tracks
through EOF in the expected order. Web's detail-page Play completed the same
queue with advancing, unpaused audio and no media error. A separate web artwork
click activated the overlay Play control and completed a different order; that
observation remains qualified. All three runs produced fifteen successful
reports each and added exactly one play per track each. Joined evidence binds
all 50 saved rows to those plays without a reset. Identity, grants, playlists,
favorites and fixture hashes remain intact. Desktop closed normally with its
settings hash unchanged and Remember Me off.

Reference records are under `.local/repeated-fields-contract-20261004`; joined
runtime evidence is under `.local/repeated-fields-client-20261004`. The card
observation is under `.local/repeated-fields-web-card-20261004`. Complete music
behavior, card-order equivalence, combined IDs/search, zero limits, audible
quality, final positions and general native video remain open.


## Zero limits

The unchanged official Jellyfin 12.0.0 runtime supplied 168 further public reads
on both ordinary list routes. They cover explicit or omitted recursion,
nonrecursive/recursive album and artist parents, explicit Audio IDs with and
without a parent, searches, whitespace-only searches, empty matches, count
flags and offsets zero and 99. Six positive-limit search controls are included.
No reference implementation source was read; media, metadata and grants stayed
unchanged. Records are under `.local/zero-limit-contract-20261004`.

For the observed album-child modes, Limit=0 with nonrecursive selection and no
explicit IDs returns every remaining child. Search terms are ignored, including
a term with no matches. The full total remains present with counting off.
Recursive or explicit-ID zero-limit requests return no items. Their default or
enabled totals count the visible matches; disabled totals are zero, even for
meaningful searches. StartIndex is retained. Positive-limit search controls
continue filtering and return full totals.

Puffinbox implements those paging modes after user and parent visibility checks
on GET /Items and GET /Users/{userId}/Items. The database omits LIMIT for the
unlimited child mode and retains OFFSET, sorting, visibility and metadata
conditions. Other zero-limit lists use LIMIT 0. Search validation still runs
before the ordinary child mode ignores the term. Playlist branches and
specialized endpoints retain their preceding pagination behavior. No Cargo
dependency or feature changed.

The database regression covers both routes, count modes, offsets, searches and
explicit IDs, with hidden and restricted tracks excluded. An additional 105
rows produce 109 visible children: the positive limit remains capped at 100,
while zero returns all 109 and an offset of 106 returns the final three. Saved
play counts, positions, ratings and favorites remain unchanged. The focused
case first failed on the preceding one-item response, then passed after repair.

Core `e6e5d5b4` matches all 144 selected pagination/count projections among the
168 reads. The other 24 artist-parent queries retain their predefined physical
layout qualification. The preceding core `9b2cd02c` matched 64 of those 144,
with 80 differences preserved. All preceding scoped count, repeated-Fields,
audio-field and detail-queue projections still match. Nine folder-filter and
84 preceding count-catalog/layout qualifications remain.

Both official clients opened Embedded Album from the eight-album main list and
completed its original four-track FLAC detail-page queue in order. Each returned
fifteen successful reports and added one play per track. Desktop decoded through
audio EOF; web audio advanced unpaused without an error. All 50 saved rows match
the two observed plays per track without a reset. Identity, grants, playlists,
favorites and media bytes remain intact. Desktop closed normally with unchanged
settings and Remember Me off. Joined records are under
`.local/zero-limit-client-20261004`.

Unlimited responses are currently assembled in memory. The 109-child fixture
does not establish large-response or operational-scale acceptance. Library-root
layouts, other filters at zero limits, playlist/specialized pagination, combined
IDs/search selection, card queue ordering, broader DTO fields, audible quality,
final positions and complete music behavior remain open.
