# Catalogue boolean queries

Public HTTP reads of the isolated official Jellyfin 12.0.0 server established
the parsing behavior below. The reference uses the pinned image recorded in
[the tagged-artist contract](jellyfin12-tagged-artists.md). No Jellyfin
implementation source was read.

Puffinbox's `/Items` and `/Users/{userId}/Items` routes now accept ASCII case
variants and surrounding ASCII whitespace for four boolean query fields.
Existing PascalCase names and camelCase aliases remain supported.

| Field | Missing or empty input | Valid examples |
| --- | --- | --- |
| Recursive | No explicit recursion choice | `true`, `True`, `TRUE`, `tRuE`, ` true ` |
| IsPlayed | No played-state filter | `false`, `False`, `FALSE`, `fAlSe`, tab-surrounded `false` |
| IsFolder | No folder filter | The same boolean forms; Puffinbox's explicit filter remains active |
| EnableTotalRecordCount | Missing defaults to true; empty input returns 400 | The same boolean forms |

Numeric values, `yes`, `null`, malformed booleans and duplicate scalar fields
remain invalid. Typed boolean values and optional null values still deserialize
correctly in the internal query structure. Search text and other fields are not
rewritten. Other endpoints' boolean fields retain their existing parsing and
are outside this change.

The reference probe contains 64 observations across these four fields,
including omitted values, case variants, whitespace, empty strings and invalid
values. The retained Puffinbox baseline rejected 35 forms that the new image
accepts. Each accepted normalized form now returns the complete preceding
canonical Puffinbox response, preserving selection, ordering and saved state.

Fifty reference projections match status, ordered IDs, names, types, totals
and offsets. Fourteen remain qualified: five disabled-count responses omit
TotalRecordCount in Puffinbox, and nine observations retain its explicit
IsFolder filtering or invalid-input rejection. The reference ignored IsFolder
in this fixture. Unrequested SortName presence and other DTO fields are outside
the projection. Six earlier mixed audio/album sorting queries using
`Recursive=True` now match the reference, including explicit SortName fields.
This does not establish complete query or response compatibility.

All thirteen source checks passed: formatting, strict Clippy, 254 standard Rust
cases, 27 disposable database cases, 101 Python cases, strict package/source
guards, notices and certificate checks. Three new unit cases cover case and
whitespace, defaults and invalid input, duplicate aliases, typed booleans and
nulls. The existing container catalogue check now exercises both routes,
complete canonical-response equality, private headers and access revocation.
All 35 container and 29 local HTTPS checks passed.

Both unchanged official web and Qt 6 Desktop completed the original four-track
FLAC album with automatic advancement and fifteen successful playback reports
each. Desktop logged four audio EOF events; advancing web audio was observed.
Independent reads confirmed one added play per track and zero music resume for
each run. Web separately completed the generated album-artist card's two
credited tracks, with seven successful playback reports and exactly one added
play on those two tracks. That shorter run has no active-media DOM snapshot.
Remember Me stayed off, and the owned Desktop process closed with unchanged
profile settings. Audible quality and broader client behavior remain open.

The retained backend preserved its identity, mounts, grants, playlists,
studio favorites and media bytes through explicit container recreation.
All 50 saved rows match the observed plays; none were reset. The joined record
binds 262 frozen source hashes to core
`sha256:edea4423c54a6b40f6a2704acd7c0a5882cd815dd0f06c5705376c5b0e8d90a2`
and operator test image
`sha256:f1a0411e78b0b26c6cc53a5a280da10d557afec4e8bd5af541fd0c538690536f`.
Server SHA-256 is
`28c458415c0d71a8770aefce4c6842feff2e8ae45fee4adc3738afd1a9110c9a`.
Evidence is under `.local/catalog-boolean-*-20261004`; reference reads are
under `.local/catalog-boolean-contract-20261004`. Failed comparison and capture
helpers remain separately preserved. Packaged documentation predates this entry.

The Cargo allowlist and bundled dependencies did not change. Linked runtime
licensing remains uncleared. Puffinbox is partial and unreleased, with all five
release gates open.


The later item-count build replayed the same 64 observations on core `2d3babe8`.
Fifty-five now match, including all five formerly qualified disabled-total
cases. The nine existing IsFolder differences remain qualified. All 50 saved
rows, policy and media hashes were retained without a reset. This replay uses
the original projection; it does not establish complete DTO equivalence.
Records are under `.local/item-total-client-20261004/boolean-comparison.json`.


The repeated-Fields build replayed the same 64 observations on core `9b2cd02c`.
Fifty-five still match and the nine existing folder-filter differences remain
qualified. All 50 saved rows, grants, policy and media hashes were unchanged by
the reads. The original projection and reference record were retained; this
does not establish complete DTO equivalence. Records are under
`.local/repeated-fields-client-20261004/boolean-comparison.json`.


The zero-limit build replayed the same 64 observations on core `e6e5d5b4`. Fifty-five
still match, with nine existing folder-filter differences qualified. All 50
saved rows, policy and media hashes were unchanged by the reads. The original
projection and reference bytes were retained; complete DTO equivalence remains
outside this check. Records are under
`.local/zero-limit-client-20261004/boolean-comparison.json`.
