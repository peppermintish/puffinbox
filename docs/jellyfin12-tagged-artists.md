# Artists from music tags

Public HTTP reads of the isolated official Jellyfin 12.0.0 server established
artist identities for the credits in the thirteen original FLAC fixtures.
The reference image is
`jellyfin/jellyfin@sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`.
No Jellyfin implementation source was read.

The embedded fixture has three names absent from its physical artist folders:
`Embedded Lead`, `Embedded Lead; Embedded Guest`, and `Embedded Album Artist`.
The semicolon remains part of a single artist name. The reference exposes these
names as artists with stable IDs, distinct performer and album-artist roles,
and album membership. Selecting a generated artist as ParentId returns no
physical children; artist selectors return its credited tracks and albums.
The album's artist roles aggregate the visible tracks.

| Comparison on core `0862dc01` | Result |
| --- | --- |
| Artist, album-artist, contributing-artist and parent membership | 22 exact scoped comparisons |
| Five artist details: name, type, SortName and child/song/album counts | All matched |
| Four name-scoped performer and album-artist lists | All matched |
| Mixed audio/album SortName, Name and IsFolder/SortName, both directions | Six exact comparisons |
| Earlier empty-credit contract | Five exact comparisons |
| Earlier performer/album-artist list contract | Sixteen exact comparisons; two uncounted orders remain qualified |

The membership comparison maps IDs between servers and restricts reference
requests to the same fixture IDs within the existing music grant. It compares
ordered IDs, types, music credits, audio name/sort/index fields, totals, offsets
and statuses. The two physical folder names have explicit aliases because the
isolated Puffinbox directories were named differently. This does not establish
whole-response equivalence. Album display names, the plain track's Album
fallback, parent DTO fields, artwork, genres and global unscoped defaults are
outside this comparison.

Six fresh reference reads with lowercase `Recursive=true` supplied the mixed
sort contract. Explicit SortName ordering preserves audio title case; the
physical album's stored SortName remains lowercase. Name ordering uses display
titles. The original reference reads used capitalized `Recursive=True`, which
the reference accepted and Puffinbox rejected with 400. That query difference
is retained as an open compatibility issue. Wider collation remains untested.
A current embedded source with no track number now leaves IndexNumber absent;
it does not infer `4` from the plain track's filename. Missing or stale
embedded metadata still permits the existing filename fallback.

Metadata writes for Audio items now register up to 32 bounded names per role
from local NFO or current embedded credits. Each missing name receives an
opaque, persistent identity in its library. An existing same-name physical
artist retains precedence. Registration shares the metadata transaction and
source fence. Migration `0027` adds the identity and source tables; existing
metadata needs a later refresh or scan to register names.

Generated artists are visible only while a current, permitted Audio item
credits them. Local NFO precedence, source identity, hidden paths, rating and
library rules apply before listing or resolving an artist, including for
administrators. Removing a credit hides its generated artist but retains the
identity and saved favorites for reappearance. Scanner stale cleanup retains
these identities and removes stale source associations. A metadata JSON flag
cannot manufacture an artist identity or bypass visibility.

The database regression covers registration, role changes, case variants,
bounded names, stale sources, NFO precedence, hidden physical names, disabled
libraries, parental rules, cross-library sources, favorites and a real rescan.
All 27 disposable database cases passed: 24 integration cases and three unit
cases. Formatting, strict Clippy, 251 standard Rust cases, 101 Python cases,
strict package/source guards, notices and certificate checks passed. Both CI
and the gated release workflow run the new database unit case explicitly.

The first import on the retained client backend created three identities.
Later upgrades and thirteen public refresh jobs retained those exact IDs and
all compared user data. The final image was identical to the preceding tested
image, so the backend was explicitly recreated to validate restart rather than
counting an unchanged container as an upgrade. Identity, mounts, grants,
playlists, studio favorites and media bytes stayed unchanged.

The final source snapshot passed all thirteen source checks, thirty-five
container checks and twenty-nine local HTTPS checks. Container checks include
active FFmpeg shutdown, video resume and metadata/user-data persistence after
restart. Earlier failed attempts remain preserved: a missing Rust test field,
an outdated sort expectation, two acceptance-harness problems and an upgrade
assertion that required a new container ID despite an identical image.

Both official web and Qt 6 Desktop loaded Artists and Album artists and a
generated artist's album page. Both completed the
original four-track FLAC album with automatic advancement and fifteen
successful playback responses each. Desktop logged four audio EOF events with
WASAPI; web audio advanced unpaused without a media error. Independent reads
confirmed PlayCount 14 to 15 after Desktop, then 15 to 16 after web, for each
track, with Played true and zero music resume. Remember Me stayed off. The
owned Desktop window/process closed and its profile settings stayed unchanged.

The core image is
`sha256:0862dc0196d67779a3b68537adda6876b465b4b30d0cca147a1a840578cf1d2d`;
the separate operator test image is
`sha256:f2f3eab55155b224af5a7afd492c7ac6b65c3b3be72fd00bc6ae2f076db3c98a`.
Server SHA-256 is
`6b417346bccc1aaf6523ac7fd599687a3e96e020a5de82612233671959a106ed`.
The joined local records are under `.local/tag-artists-*-20261004f`, with 262
matching frozen source hashes. The initial retained import is recorded under
`.local/tag-artists-client-20261004`. Packaged documentation predates this entry.

Album naming and creation, album-only metadata artist registration, embedded
images, duplicate-name semantics, wider tag formats, audible quality and
complete music behavior remain open. The Cargo allowlist and bundled
components did not change. Linked runtime licensing remains uncleared;
Puffinbox is partial and unreleased, with all five release gates open.
