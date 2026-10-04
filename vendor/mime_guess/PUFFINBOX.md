This is the MIT-licensed mime_guess 2.0.5 package, adapted for Puffinbox.
The upstream LICENSE and the complete src/mime_types.rs table are unchanged.
The crate archive SHA-256 is
`f7c44f8e672c00fe5308fa235f821cb4198414e1c77935c1ab6948d3fd78550e`;
its upstream commit is `805964fb54871f0154ee155bf21729b77ffd4a1c`.

An original ASCII comparison replaces unicase in forward and reverse lookup.
All registered extensions and MIME tokens in this version are ASCII. Lookups
ignore ASCII letter case; non-ASCII extensions are unknown. Unicode filenames
with ASCII extensions keep working. Unicode lookalike extensions such as `ſvg`
and `Kml` no longer match ASCII extensions.

The public API, ordered MIME table and optional reverse mappings remain.
The historical, undeclared PHF branch and unused benchmark/criterion dependency
are omitted. Cargo.toml.orig is retained as an upstream provenance record and is
not the active manifest. Tests cover the full forward and reverse table, wildcard
projections, Unicode paths, unknown extensions and served media headers.

The new ASCII helper and tests are original Puffinbox work under MIT OR Apache-2.0.
The existing upstream code remains under its MIT notice. Exact retained-file
hashes are recorded in ../dependency-replacements.json. This patch excludes one
Unicode data dependency; it does not clear the complete runtime licensing gate.

The table cites MimeTypeMap and mime-db as data sources. Their MIT notices are
preserved in LICENSE-MIMETYPEMAP and LICENSE-MIME-DB, from revisions
`45622b360000f1450c8241c5e83ad61f46b902d8` and
`424fb61ca34d480d3f25dd945acc44f37c360f56` respectively. The latter is mime-db
v1.34.0, named by the cited converter's manifest. These notices do not establish
an exact historical input join for every table entry. The cited converter is
WTFPL-licensed; it is not copied, run or bundled here. Historical extraction and
subsequent manual updates remain part of the open data-provenance review.
