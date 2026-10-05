# Series sorting in catalogue queries

The official web and Desktop Favorites views request
`SortBy=SeriesSortName,SortName`. Puffinbox previously rejected this field and
returned 400, leaving Desktop's Favorites page blank. The catalogue now
accepts the field in either case and sorts episodes by their visible series
before applying the requested secondary keys. Season items use their visible
parent series. Items without a visible series have a null series key.

The contract comes from twelve public HTTP reads of the isolated official
Jellyfin 12.0.0 reference recorded in [the Next Up contract](jellyfin12-next-up.md).
No Jellyfin implementation source was inspected. Four episode queries establish
ascending, descending and independently directed secondary sorting. A single
`SeriesSortName` key retains an ascending name tie-break even when the series
direction is descending. Eight unchanged Favorites queries returned 200 for
episodes, movies, series, seasons, audio, albums, artists and photos. Only the
episode query returned a favorite in that fixture, so empty selections do not
establish ordering for the other types.

Puffinbox applies the selected user's library and item policies before ordering,
totals and paging. Series keys come from the same permission-filtered catalogue
relation as other parent keys. The database regression gives episode titles an
order that deliberately disagrees with series names, checks all four observed
orders, and excludes a classified episode and a private-library episode. It also
checks the official Favorites query, page boundaries and empty type selections.

General catalogue queries currently use each item's classification; Next Up
also gates its enclosing series and season. Wider classification inheritance
needs a separate reference test. Disabled-count episode Favorites reads returned
zero in the reference; Puffinbox retains its existing page-size count on ordinary
catalogue lists. That count difference is outside the ordering match. Wider
series-name normalization, metadata sort titles, mixed-type null ordering and
malformed catalogue ancestry remain unvalidated.

The private observations and database runs are under
`.local/next-up-contract-20261005/`. The first database attempt used a series-only
rating fixture where the catalogue required an item classification; the second
expected an omitted total instead of the existing page-size result. Both failed
records are preserved. The corrected fixture and qualified count assertion
passed without changing catalogue policy or counting behavior. Rebuilt-image
and official Favorites results belong in [acceptance.md](acceptance.md).
