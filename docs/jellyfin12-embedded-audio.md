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
Embedded credits currently resolve only to visible artist catalog entries in
the same library. Automatic creation of tag-named artist and album entries,
embedded cover art, wider format coverage and general reference equivalence
remain incomplete. Folder-derived relationships and the existing filename
track-index fallback remain qualified differences.

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
