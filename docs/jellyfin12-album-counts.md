# Music album counts and duration

Physical `MusicAlbum` items expose `ChildCount` and `RunTimeTicks` in item details
and catalogue lists. The count includes direct `Audio` children in the same
library that the requesting user can see. Duration sums those children's stored
runtime ticks. Missing durations contribute zero; a total exceeding the signed
64-bit range saturates at its maximum. Album responses omit the artist-only
`SongCount` and `AlbumCount` fields.

This is a partial album contract. It does not create albums from embedded tags,
change folder-derived album names, count nested albums or establish behavior
for every child type.

## Public reference

Queries against the isolated official Jellyfin 12.0.0 image
`sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`
returned these values for the original synthetic FLAC fixtures:

| Album | ChildCount | RunTimeTicks |
| --- | ---: | ---: |
| Embedded Album | 4 | 200,000,000 |
| Sort Contract Album | 9 | 90,000,000 |

The reference details and album lists omit `SongCount` and `AlbumCount`.
Observations preserve exact queries, response status and fixture hashes under
`.local/music-album-count-contract-20261004` and
`.local/music-album-count-contract-20261004b`. Only public HTTP responses were
read; Jellyfin implementation code was not inspected. The second run used the
temporary non-administrator's own session after each policy change. Its account
was removed afterward, and the original track responses and media bytes stayed
unchanged.

Reference details returned 404 after the album's library grant was removed,
and the album list omitted it. However, the observed `ParentId` child query still
returned its tracks after grant removal. Puffinbox retains its permission fence:
an inaccessible album and its parent-scoped child queries return 404. This
difference remains qualified; the reference observation is not permission to
expose those tracks.

## Validation and limits

The PostgreSQL catalogue regression checks the two detail routes and both
catalogue list routes. It covers ordinary counts, hidden paths, restricted
ratings, missing and zero durations, overflow, immediate grant revocation and
disabled libraries. An empty album remains hidden from a regular user under
Puffinbox's existing folder visibility rule; the administrator receives zero
count and duration. Overflow and empty-album behavior have local regression
coverage but were not independently established against the reference.

The synthetic container acceptance check reads the four-track album's count and
duration before and after restart. Those assertions accompany the original
embedded metadata and unchanged-media checks.

See [acceptance](acceptance.md) for tested image and client identities. Album
display names, artwork, creation from tags, wider formats and complete music
behavior remain incomplete or unvalidated. These checks do not clear runtime
licensing or any release gate.
