# Metadata provider and plug-in extensions

These extension routes are an original Puffinbox interface. They do not make a general Jellyfin plug-in compatibility claim.

## Local NFO metadata

The `local-nfo` provider reads a bounded sidecar beside a file, or a named sidecar inside a registered Series or album directory. It accepts only the supported top-level XML fields: `title`, `originaltitle`, `plot`, `outline`, `premiered`, `releasedate`, `year`, `mpaa`, `genre`, and `tvmazeid`. It rejects DTDs, malformed documents, path escapes, links, and non-regular files. A genuinely missing sidecar removes that provider's prior row. A malformed or unsafe sidecar clears only its unusable local rating classification and records an error; transient storage failures preserve the last good row and are retried.

The first parental-classification mapping is `US-MPAA-v1`: `G` = 0, `PG` = 25, `PG-13` = 50, `R` = 75, and `NC-17` = 100. These are ordinal policy thresholds, not ages or Jellyfin rating scores. Other, absent, or ambiguous labels remain unrated. Unrated-item access follows the account's explicit `BlockUnratedItems` policy.

## Optional TVMaze data

TVMaze lookup is disabled unless an administrator explicitly requests a refresh with `AllowCcBySaProvider: true`. This imports third-party data under TVMaze's CC BY-SA data terms, separate from Puffinbox's software license. The provider stores attribution fields and exposes the community rating as a score; it never maps that score to a parental classification. The provider does not fetch TVMaze artwork.

Only `Series` items are queried. An exact normalized title is accepted only when one result matches. No exact match is treated as no match; duplicate exact titles produce `ambiguous-title-requires-tvmazeid`. To select a show explicitly, place its positive numeric TVMaze ID in the Series directory's `tvshow.nfo`, for example:

```xml
<tvshow>
  <title>Example Series</title>
  <tvmazeid>12345</tvmazeid>
</tvshow>
```

The provider contacts only `api.tvmaze.com` over HTTPS. It disables redirects and environment proxy use, filters DNS results to public addresses, pins the approved addresses for the request, and applies response-size and timeout limits. It accepts no operator-supplied URL.

## Operator-staged metadata hooks

There is no remote plug-in upload or command-execution endpoint. An operator stages a Wasm module and manifest under the server data directory before trusting it:

```text
<PUFFINBOX_DATA_DIR>/plugins/<plugin-id>/manifest.json
<PUFFINBOX_DATA_DIR>/plugins/<plugin-id>/metadata-enricher.wasm
```

The module must be compiled WebAssembly version 1, use `module.wasm` or `metadata-enricher.wasm`, match the SHA-256 in its manifest, and implement the `metadata.enrich.v1` Wasm ABI. Text modules (`.wat`) are rejected, including text renamed to `.wasm`. Earlier unreleased checkpoints accepted text; compile those hooks and update their manifests before staging them. The API accepts only the explicit metadata input/output schema. Modules receive no host imports, filesystem, network, clock, or command interface. Module bytes, JSON input/output, parser limits, Wasm resources, execution fuel, and worker concurrency are bounded.

The hook input is the serialized production catalog projection. Its JSON keys are `id`, `name`, `itemType`, and nullable `overview`, for example:

```json
{"id":"item-uuid","name":"Example film","itemType":"Movie","overview":null}
```

The exported `enrich(i32 input_ptr, i32 input_len, i32 output_ptr, i32 output_capacity) -> i32` function returns the number of output bytes written. The output is a JSON object with optional `overview` and `genres` fields; unknown fields are rejected. Input is capped at 8 KiB, output at 48 KiB, linear memory at 1 MiB, and each invocation receives a finite instruction-fuel budget.

After staging, an administrator authenticates and reviews the declared metadata, then calls:

1. `POST /Puffinbox/Plugins/TrustStaged` with `{"PluginId":"example-metadata-enricher"}`.
2. `POST /Puffinbox/Plugins/example-metadata-enricher/Enable`.
3. `POST /Puffinbox/Metadata/Refreshes` with an item or library scope and `Providers:["plugin:example-metadata-enricher"]`.

Stage the original demonstration binary [metadata-enricher.wasm](metadata-enricher.wasm) with the adjacent manifest. Its SHA-256 is `7f8e3f558994efa0055e3dbc0b151aef45c803899ae0ef577a6889139c71a2a6`. Run `python3 examples/plugins/build_example.py` to reproduce that binary with the original fixed example encoder; [metadata-enricher.wat](metadata-enricher.wat) describes the same hook for review. The encoder does not parse arbitrary text modules. The manifest's license and provenance strings are operator-supplied claims, not legal verification or a substitute for reviewing module source and dependencies. The example contains no third-party code or metadata.

Plugin output is stored in a separate provider row. Disabling or re-trusting a plug-in hides the old row from preferred item display until a refresh under the currently enabled, trusted module succeeds. A failed refresh keeps the last successful metadata unless the provider deliberately reports no result.
