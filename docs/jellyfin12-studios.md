# Studios

Puffinbox resolves studio names from local NFO metadata. A visible item's `Studios` entries carry stable, opaque IDs. `StudioIds` selects matching items before counts and paging. Names shared by several libraries refer to the same studio, but each user sees only their permitted items and counts.

`GET /Studios` supports parent and item-type scope, search, name bounds, paging, favorite selection, and the image/user-data/total options. `GET /Studios/{name}` and `GET /Items/{studioId}` return a visible studio with its item counts. Studio favorites persist per user through `POST` and `DELETE /UserFavoriteItems/{studioId}`; user-data reads return that preference. Removing a library grant removes its studio names, counts and detail access even when a favorite remains saved.

Local NFO accepts up to 32 distinct `<studio>` values of 512 UTF-8 bytes each. Empty values are ignored; control characters and oversized inputs are rejected. The [official NFO documentation](https://jellyfin.org/docs/general/server/metadata/nfo/) describes repeated studio tags. Other metadata providers do not currently supply studio credits. Pages are bounded by the configured catalog page limit and a maximum of 100 studios.

## Reference observations

The contract comes from the [public Jellyfin 12.0 schema](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json) and HTTP observations of an isolated official 12.0.0 server. Its image was `sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`. No server or client implementation was inspected.

Two original one-second color movies carried NFO credits for Alpha Film Studio, Beta Film Studio and Guest Film Studio. Scoped studio queries returned those three names. Name reads counted one, two and one movies respectively. The review user's favorite toggle and favorite-only query returned Beta Film Studio. A disabled total option returned `TotalRecordCount: 0`, which Puffinbox also returns for this endpoint.

An independent filename control imported a basename NFO by itself, then restored `movie.nfo` while keeping that basename file. A full public metadata refresh selected `movie.nfo`. Puffinbox follows that precedence and falls back to the basename only when the standard file is absent. The control restored the reference's original files and metadata; its record is `movie-nfo-precedence-control.json` beside the other observations.

The official web studio view requests `IsFolder,SortName`. Item queries now support that sort with the existing bounded field list and per-field directions. The opaque reference placed files before concrete folders in ascending order and reversed those groups in descending order. Two virtual music-artist responses omitted `IsFolder` and sorted with files; Puffinbox's sort currently covers stored catalog items. The read-only observations are in `folder-sort.json`; the separate output qualification records a console-summary failure after its HTTP ledger had already been saved.

The reference's default root list disagreed with direct name lookups and was initially stale after the movie scan. Earlier temporary studio edits on music tracks also did not appear in that list, despite correct direct counts. Neither attaching a name to two tracks nor restarting the isolated reference resolved those music observations. They do not establish a minimum-count rule. Puffinbox enumerates names from current visible metadata instead of reproducing the stale results. Unknown or inaccessible names return 404 here; the reference created an empty studio response for an unknown name.

The private observations are preserved in `.local/jellyfin12-reference-20261003/studio-read-only.json`, `studio-multi-value.json`, `studio-counts.json`, `studio-after-restart.json` and `studio-movies.json`. The movie helper's `libraryOptions` ledger field was overwritten by its later query-option list; the helper source preserves the actual library creation options. The original movie/NFO hashes, movie DTOs and public query responses remain recorded. These are synthetic contract observations, not comprehensive Jellyfin acceptance.

## Validation and limits

The PostgreSQL regression covers shared-name IDs, duplicate credits, counts, item selectors, metadata precedence, parent scope, type exclusions, names, paging, favorites, selected-user authorization, library revocation, hidden paths, ratings and disabled libraries. The NFO regression covers repeated names and input bounds. Full local source checks passed with 243 Rust tests, 24 database cases and 79 Python checks, formatting, strict Clippy, both target package audits and selected-source guards.

Core `30ccd3a0` passed 31 container and 29 HTTPS checks, 23 independent studio requests, and studio favorite persistence across restart. Official web showed all three studios but opening one returned 400 for `IsFolder,SortName`; that failed view is preserved under `.local/studio-nfo2-web-20261003`.

Current core `3f06f4ac` passed 31 container and 29 HTTPS checks, 24 independent studio requests, and favorite persistence across restart. Official web and Qt 6 Desktop displayed all three studios and Beta Film Studio's two movies with the default folder sort. Web's studio favorite control toggled on and off; independent reads confirmed its original value and unchanged movie state. The original media hashes, grants and server identity were retained. Evidence is under `.local/studio-sort-client-20261003`, `.local/studio-sort-web-20261003` and `.local/studio-sort-native-20261003`.

Desktop's first movie failed in MPV video-output initialization in the software-rendering profile; no decoded playback or queue advancement is established. A separate fresh OpenGL profile remained blank at startup. Both failures are preserved, and the underlying graphics cause remains unresolved. Studio artwork, metadata edits through the Jellyfin item-update API, played-state writes, broader locale behavior, general `/Items` enumeration of virtual studios and scale remain incomplete or unvalidated. The project remains unreleased.
