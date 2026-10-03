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
