# Jellyfin 12 artist and album-artist lists

These observations use public HTTP requests to the retained official Jellyfin
12.0.0 test container. No implementation source was inspected. Its image is
`jellyfin/jellyfin@sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`.

The existing fixture has three original five-second FLAC files: a lead song
and a guest song on the lead's shared album, plus the guest's solo album.
Embedded tags identify the performer and album artist. The settled artist
and album entries already existed before these reads. No library, metadata,
media, playback, user-data or permission changes occurred. Independent track
responses and fixture hashes stayed unchanged.

| Scope | Route | Artist | SongCount | AlbumCount | ChildCount |
| --- | --- | --- | ---: | ---: | ---: |
| Library | `/Artists` | Guest | 2 | 2 | 4 |
| Library | `/Artists` | Lead | 1 | 1 | 2 |
| Library | `/Artists/AlbumArtists` | Guest | 1 | 1 | 2 |
| Library | `/Artists/AlbumArtists` | Lead | 2 | 1 | 3 |
| Shared album | `/Artists` | Guest | 1 | 0 | 1 |
| Shared album | `/Artists` | Lead | 1 | 0 | 1 |
| Shared album | `/Artists/AlbumArtists` | Lead | 2 | 0 | 2 |
| Solo album | Either route | Guest | 1 | 0 | 1 |

The parent album is outside its own descendant selection, so its artist list
has zero album counts. The shared album's album-artist list excludes the
guest. Artist runtime remains ten seconds in each response, including
album-scoped reads; it reflects the artist's aggregate related songs rather
than the role-specific count.

Eighteen queries cover both routes, library and album scope, guest search,
two one-item pages, a page beyond the results, zero Limit and disabled total
counts. Counted lists put Guest before Lead and report the full selection
total even for an empty page. Limit=0 returns no items. With
EnableTotalRecordCount=false, the reference returns TotalRecordCount=0 and
Lead before Guest. This does not establish a stable uncounted ordering rule.

Exact requests, responses and hashes are under
`.local/artist-list-contract-20261003`. The first infrastructure guard expected
no port declarations and stopped before authentication. The passing read
verified the older fixture's one loopback binding and internal network,
using HTTP inside the container. That guard failure remains separate.

Puffinbox's list query selects visible source songs and albums in the
requested scope, resolves their visible same-library credits, and counts the
requested role before search and paging. It preserves the existing artist
detail runtime and returns an empty ParentId for role listings, independently
of the physical folder containing the artist. Search treats percent signs as
literal text. The page is
capped at one hundred items; negative offsets and limits are rejected.
Uncounted lists retain Puffinbox's stable stored-name order and return zero
for the total. General name collation and that uncounted order remain
qualified differences.

The PostgreSQL fixture covers the frozen counted selections, both empty-page
cases, role-specific counts, global runtime, hidden and restricted artists,
hidden songs, denied and disabled libraries, literal search and invalid
options. Existing standard item queries and artist details keep their
aggregate credit behavior. Automatic creation of tag-named artists and
albums, broader list filters and full music compatibility remain incomplete.

On core `645b6abe`, sixteen of the eighteen queries matched the paired
Puffinbox catalogue after normalizing the known item IDs. Two disabled-total
queries differ only in order and remain qualified. Compared item fields are
Id, Name, Type, ParentId, SongCount, AlbumCount, ChildCount and RunTimeTicks;
status, total and offset are also checked. Puffinbox uses the same original
FLAC files with local NFO credits and existing artist entries. Its wider
retained library requires a name selector to isolate the reference pair.
These results do not establish tag-named artist creation or full DTO equality.
All compared saved state, permissions and fixture hashes remained unchanged.
Exact comparisons are under `.local/artist-list-client-20261003b`.

Both unchanged official web and Qt 6 Desktop loaded the Artists and Album
artists views, with successful requests for both routes, then completed the
original four-track album. Each run added one play per track without a reset.
Desktop decoded through audio EOF; web's active audio advanced without an
error. The [acceptance record](acceptance.md) binds their screenshots, traces
and saved-state checks to the final source and images.

The first candidate retained physical parent IDs and failed fourteen list
comparisons despite correct counts. Its failed ledger remains separate.
After clearing list ParentId, the first regression assertion incorrectly
required omission instead of accepting null; the corrected nested-folder
case passed the complete source and database suites. An earlier comparison
helper's output filename collision is also retained as a failed record.
