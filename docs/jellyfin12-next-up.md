# Next Up contract

The endpoint is `GET /Shows/NextUp`. Its request parameters are defined by the
[public Jellyfin 12 schema](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json).
The implementation uses public API observations of an isolated official
Jellyfin `12.0.0` runtime, image
`sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`.
No Jellyfin implementation source was inspected or copied.

The original fixture contains eleven episodes across four shows, including a
special, a second season, an untouched show and a completed show. Private
observation records are under `.local/next-up-contract-20261005/`; file hashes
were checked after each sequence of public user-data changes.

## Observed selection

- An untouched library returns no candidates. An explicit `seriesId` can return
  the first regular episode of an untouched show.
- The next unwatched episode follows the highest watched episode in season and
  episode order. Older unwatched gaps are skipped; a completed show is omitted.
- A dated history entry can start a show even at zero resume position. A favorite
  with no playback history does not start it.
- `enableResumable=false` removes a resumable candidate without advancing to a
  later episode.
- With `enableRewatching=true`, a previously watched episode following the most
  recently watched episode can also appear. An unfinished show can therefore
  contribute both a rewatch candidate and its next unwatched episode.
- Specials do not displace the regular episode sequence in this fixture.
- Watched-show ordering uses watched history; playback activity determines the
  date cutoff. Library-root `parentId` scopes results. An item parent returns no
  candidates in the observed operation.
- `limit=0` returns the complete observed result. Paging retains the total and
  requested start index. Disabling counts returns `TotalRecordCount: 0`.

## Puffinbox scope

User selection, library grants, enabled libraries, hidden paths and parental
ratings are applied before selecting candidates and counting results. Ancestors
must also be visible. Images and user-data inclusion flags are honored; filesystem
paths follow the selected user's existing administrator policy.

The official clients send `fields` and `enableImageTypes` as repeated query
values as well as comma-separated lists. Both forms are accepted without merging
scalar user or paging selectors. Encoded delimiters remain inside their values.
Field projection retains the catalog's existing limits.

Episode ordering currently requires a numbered Season folder and a filename
containing `S<number>E<number>`. Unknown numbers and season zero are excluded.
Custom NFO numbering, multi-episode files, missing/virtual episodes, grouped
series and wider date/order combinations remain unvalidated. Series and season
labels currently follow their visible catalog folders.

Pages are bounded to the configured maximum, at most 100 items. An omitted or
zero limit that would exceed that bound returns a conflict and requires explicit
paging. This is a declared operational limit rather than unbounded enumeration.

`tests/postgres_next_up.rs` covers the observed selections and adds cross-user,
library, rating, projection and live-policy checks. It requires an explicitly
disposable PostgreSQL database and is included in both CI workflow lists. Current
validation results and artifact scope belong in [acceptance](acceptance.md).
