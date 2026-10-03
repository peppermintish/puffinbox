# Jellyfin 12 artist selections

An isolated official Jellyfin 12.0.0 runtime distinguishes a track performer from its album artist. A guest track on another artist's album appears in the guest's songs and albums, and the shared album appears under “Appears On.” It does not become one of the guest's own albums.

The reference used three original five-second FLAC files with embedded tags: a lead track and a guest track on a shared album, plus the guest's solo track on a separate album. Public API reads supplied the following results; no Jellyfin implementation was inspected or copied.

| Selection for the guest artist | Audio results | MusicAlbum results |
| --- | --- | --- |
| ArtistIds | Guest and solo tracks | Shared and solo albums |
| AlbumArtistIds | Solo track | Solo album |
| ContributingArtistIds | Guest track | Shared album |
| ExcludeArtistIds | Lead track and unrelated music | Albums without that artist's credits |

The lead artist matches both tracks on the shared album through ArtistIds and AlbumArtistIds, and has no guest contribution there. Positive artist selectors return no MusicArtist items. ExcludeArtistIds does not remove an artist item merely because its own ID was supplied. Unknown positive IDs return no results; unknown exclusions leave the album list unchanged.

The reference's guest artist response has SongCount=2, AlbumCount=2 and ChildCount=4. The lead has SongCount=2, AlbumCount=1 and ChildCount=3. Each artist's RunTimeTicks aggregates the two related five-second tracks. These are catalogue counts, independent of playback state.

Puffinbox resolves explicit local NFO artist and albumartist names to existing visible MusicArtist items in the same library. Each role accepts up to 32 distinct names of 512 bytes. Without explicit names, the visible artist/album folder relationship supplies the fallback. An explicit hidden credit cannot fall back to a different artist or disclose its name. Artist selections and exclusions run before counting and paging. Details distinguish ArtistItems from AlbumArtists, and artist counts include only visible related songs and albums.

The PostgreSQL fixture covers both item-query routes, combined selections, paging, unknown and malformed IDs, denied libraries, hidden and restricted artists, hidden tracks, and disabled libraries. On core `ed473c4b`, the same original reference files with local NFO credits passed 24 public selector reads and both artist count checks. The unchanged official web and Qt 6 Desktop show the guest's own album under Albums and the shared album under Appears On, excluding unrelated albums. Web shared-album details distinguish each track's performer from its lead album artist.

Desktop's guest-artist Play control includes the guest track on the shared album and the solo track. Both original FLAC files decoded through audio EOF and advanced automatically; all seven playback responses returned 204. Independent reads confirmed one added play per track, Played=true and zero saved resume. A separate web replay completed the original three-track AIFF album through full-duration AAC HLS on the same image. Source/image/fixture hashes, traces, screenshots and saved-state results are under `.local/artist-credits-client-20261003`, `.local/artist-credits-native-20261003` and `.local/artist-credits-web-20261003`.

Embedded tag import, automatic creation of missing artists, ambiguous duplicate catalogue names and complete music metadata compatibility remain incomplete or unvalidated. Similar and Instant Mix still rank relationships from the visible folder catalogue; they do not yet use explicit guest credits. Audible quality and broader formats remain unvalidated.

Reference image and fixture hashes, all 30 public selector requests and artist DTOs are under `.local/jellyfin12-reference-20261003/artist-selectors.json`. The preceding image's missing-count and ignored-selector baseline is `.local/music-play-state-client-20261003/artist-before.json`.
