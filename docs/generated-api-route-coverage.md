# Generated route declaration comparison

Generated at 2026-09-30T21:05:52.972148+00:00 from [https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json).

- OpenAPI version: `12.0.0`
- Schema paths: `294`
- Schema operations: `364`
- Unique declared server method/path pairs: `159`
- Declared pairs matching a schema method/path: `74`
- Declared pairs outside the schema: `85`

This is a source declaration comparison only. It does not establish that a matching route starts, authenticates correctly, returns the required shape, enforces policy, or behaves like Jellyfin.

## Declared route pairs

| Declaration | Source | Schema path/method match |
| --- | --- | --- |
| `GET /` | `src/api.rs` | custom / not in target schema |
| `GET /health` | `src/api.rs` | custom / not in target schema |
| `GET /health/ready` | `src/api.rs` | custom / not in target schema |
| `GET /System/Info/Public` | `src/api.rs` | match |
| `GET /Branding/Configuration` | `src/api.rs` | match |
| `GET /QuickConnect/Enabled` | `src/api.rs` | match |
| `GET /Users/Public` | `src/api.rs` | match |
| `GET /users/public` | `src/api.rs` | custom / not in target schema |
| `GET /System/Info` | `src/api.rs` | match |
| `GET /Localization/ParentalRatings` | `src/api.rs` | match |
| `GET /Startup/Configuration` | `src/api.rs` | match |
| `POST /Startup/User` | `src/api.rs` | match |
| `POST /Users/AuthenticateByName` | `src/api.rs` | match |
| `POST /Users/authenticatebyname` | `src/api.rs` | custom / not in target schema |
| `GET /Users/Me` | `src/api.rs` | match |
| `POST /Users/Me/MediaAccessToken` | `src/api.rs` | Puffinbox extension / outside target schema |
| `POST /Users/Me/Logout` | `src/api.rs` | custom / not in target schema |
| `GET /Users` | `src/api.rs` | match |
| `POST /Users` | `src/api.rs` | match |
| `DELETE /Users/{user_id}` | `src/api.rs` | match |
| `GET /Users/{user_id}` | `src/api.rs` | match |
| `POST /Users/{user_id}` | `src/api.rs` | custom / not in target schema |
| `GET /Users/{user_id}/Policy` | `src/api.rs` | custom / not in target schema |
| `POST /Users/{user_id}/Policy` | `src/api.rs` | match |
| `DELETE /Library/VirtualFolders` | `src/api.rs` | match |
| `GET /Library/VirtualFolders` | `src/api.rs` | match |
| `POST /Library/VirtualFolders` | `src/api.rs` | match |
| `GET /Library/MediaFolders` | `src/api.rs` | match |
| `POST /Library/Refresh` | `src/api.rs` | match |
| `POST /Library/VirtualFolders/Refresh` | `src/api.rs` | custom / not in target schema |
| `GET /Library/ScanStatus` | `src/api.rs` | custom / not in target schema |
| `GET /Puffinbox/Libraries/RootIdentities` | `src/api.rs` | custom / not in target schema |
| `POST /Puffinbox/Libraries/Roots/Rebind` | `src/api.rs` | custom / not in target schema |
| `GET /UserViews` | `src/api.rs` | match |
| `GET /Items/Latest` | `src/api.rs` | match |
| `GET /UserItems/Resume` | `src/api.rs` | match |
| `GET /Shows/{series_id}/Seasons` | `src/api.rs` | match |
| `GET /Shows/{series_id}/Episodes` | `src/api.rs` | match |
| `GET /Persons` | `src/api.rs` | match |
| `GET /Persons/{name}` | `src/api.rs` | match |
| `GET /Artists` | `src/api.rs` | match |
| `GET /Artists/AlbumArtists` | `src/api.rs` | match |
| `GET /Items` | `src/api.rs` | match |
| `GET /Items/Counts` | `src/api.rs` | match |
| `GET /Items/{item_id}` | `src/api.rs` | match |
| `GET /Items/{item_id}/UserData` | `src/api.rs` | custom / not in target schema |
| `POST /Items/{item_id}/UserData` | `src/api.rs` | custom / not in target schema |
| `GET /UserItems/{item_id}` | `src/api.rs` | custom / not in target schema |
| `POST /UserItems/{item_id}` | `src/api.rs` | custom / not in target schema |
| `GET /UserItems/{item_id}/UserData` | `src/api.rs` | match |
| `POST /UserItems/{item_id}/UserData` | `src/api.rs` | match |
| `DELETE /UserPlayedItems/{item_id}` | `src/api.rs` | match |
| `POST /UserPlayedItems/{item_id}` | `src/api.rs` | match |
| `DELETE /UserFavoriteItems/{item_id}` | `src/api.rs` | match |
| `POST /UserFavoriteItems/{item_id}` | `src/api.rs` | match |
| `GET /Search/Hints` | `src/api.rs` | match |
| `GET /Sessions` | `src/api.rs` | match |
| `POST /Sessions/Capabilities/Full` | `src/api.rs` | match |
| `POST /Sessions/Logout` | `src/api.rs` | match |
| `POST /Sessions/Playing` | `src/api.rs` | match |
| `POST /Sessions/Playing/Progress` | `src/api.rs` | match |
| `POST /Sessions/Playing/Stopped` | `src/api.rs` | match |
| `GET /Books/{item_id}/Reader` | `src/media_features/books.rs` | custom / not in target schema |
| `GET /Books/{item_id}/Document` | `src/media_features/books.rs` | custom / not in target schema |
| `HEAD /Books/{item_id}/Document` | `src/media_features/books.rs` | custom / not in target schema |
| `GET /Books/{item_id}/Epub` | `src/media_features/books.rs` | custom / not in target schema |
| `HEAD /Books/{item_id}/Epub` | `src/media_features/books.rs` | custom / not in target schema |
| `GET /Books/{item_id}/Download` | `src/media_features/books.rs` | custom / not in target schema |
| `HEAD /Books/{item_id}/Download` | `src/media_features/books.rs` | custom / not in target schema |
| `GET /Puffinbox/Dlna/Pairings` | `src/media_features/dlna.rs` | custom / not in target schema |
| `POST /Puffinbox/Dlna/Pairings` | `src/media_features/dlna.rs` | custom / not in target schema |
| `DELETE /Puffinbox/Dlna/Pairings/{pairing_id}` | `src/media_features/dlna.rs` | custom / not in target schema |
| `GET /Puffinbox/Dlna/description.xml` | `src/media_features/dlna.rs` | custom / not in target schema |
| `GET /Puffinbox/Dlna/{pairing_id}/description.xml` | `src/media_features/dlna.rs` | custom / not in target schema |
| `GET /Puffinbox/Dlna/{pairing_id}/ContentDirectory/scpd.xml` | `src/media_features/dlna.rs` | custom / not in target schema |
| `POST /Puffinbox/Dlna/{pairing_id}/ContentDirectory/control` | `src/media_features/dlna.rs` | custom / not in target schema |
| `GET /Puffinbox/Dlna/{pairing_id}/ConnectionManager/scpd.xml` | `src/media_features/dlna.rs` | custom / not in target schema |
| `POST /Puffinbox/Dlna/{pairing_id}/ConnectionManager/control` | `src/media_features/dlna.rs` | custom / not in target schema |
| `GET /Puffinbox/Dlna/{pairing_id}/media/{item_id}` | `src/media_features/dlna.rs` | custom / not in target schema |
| `HEAD /Puffinbox/Dlna/{pairing_id}/media/{item_id}` | `src/media_features/dlna.rs` | custom / not in target schema |
| `GET /LiveTv/Channels` | `src/media_features/livetv_api.rs` | match |
| `GET /LiveTv/Programs` | `src/media_features/livetv_api.rs` | match |
| `GET /LiveTv/Timers/Defaults` | `src/media_features/livetv_api.rs` | match |
| `GET /LiveTv/Timers` | `src/media_features/livetv_api.rs` | match |
| `POST /LiveTv/Timers` | `src/media_features/livetv_api.rs` | match |
| `DELETE /LiveTv/Timers/{timer_id}` | `src/media_features/livetv_api.rs` | match |
| `GET /LiveTv/Timers/{timer_id}` | `src/media_features/livetv_api.rs` | match |
| `POST /LiveTv/Timers/{timer_id}` | `src/media_features/livetv_api.rs` | match |
| `GET /LiveTv/SeriesTimers` | `src/media_features/livetv_api.rs` | match |
| `POST /LiveTv/SeriesTimers` | `src/media_features/livetv_api.rs` | match |
| `DELETE /LiveTv/SeriesTimers/{timer_id}` | `src/media_features/livetv_api.rs` | match |
| `GET /LiveTv/SeriesTimers/{timer_id}` | `src/media_features/livetv_api.rs` | match |
| `POST /LiveTv/SeriesTimers/{timer_id}` | `src/media_features/livetv_api.rs` | match |
| `GET /LiveTv/Recordings` | `src/media_features/livetv_api.rs` | match |
| `GET /LiveTv/TunerHosts` | `src/media_features/livetv_api.rs` | custom / not in target schema |
| `GET /Admin/LiveTv/Sources` | `src/media_features/livetv_api.rs` | custom / not in target schema |
| `POST /Admin/LiveTv/Sources` | `src/media_features/livetv_api.rs` | custom / not in target schema |
| `DELETE /Admin/LiveTv/Sources/{source_id}` | `src/media_features/livetv_api.rs` | custom / not in target schema |
| `POST /Admin/LiveTv/Sources/{source_id}` | `src/media_features/livetv_api.rs` | custom / not in target schema |
| `POST /Admin/LiveTv/Sources/{source_id}/Refresh` | `src/media_features/livetv_api.rs` | custom / not in target schema |
| `GET /LiveTv/Channels/{item_id}/master.m3u8` | `src/media_features/livetv_runtime.rs` | custom / not in target schema |
| `HEAD /LiveTv/Channels/{item_id}/master.m3u8` | `src/media_features/livetv_runtime.rs` | custom / not in target schema |
| `GET /LiveTv/Channels/{item_id}/hls/{session_id}/playlist.m3u8` | `src/media_features/livetv_runtime.rs` | custom / not in target schema |
| `GET /LiveTv/Channels/{item_id}/hls/{session_id}/{segment_name}` | `src/media_features/livetv_runtime.rs` | custom / not in target schema |
| `DELETE /LiveTv/Channels/{item_id}/hls/{session_id}` | `src/media_features/livetv_runtime.rs` | custom / not in target schema |
| `POST /LiveTv/Channels/{item_id}/hls/{session_id}/keepalive` | `src/media_features/livetv_runtime.rs` | custom / not in target schema |
| `GET /Videos/{item_id}/stream` | `src/media_features/mod.rs` | match |
| `HEAD /Videos/{item_id}/stream` | `src/media_features/mod.rs` | match |
| `GET /Audio/{item_id}/stream` | `src/media_features/mod.rs` | match |
| `HEAD /Audio/{item_id}/stream` | `src/media_features/mod.rs` | match |
| `GET /Items/{item_id}/File` | `src/media_features/mod.rs` | match |
| `HEAD /Items/{item_id}/File` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Items/{item_id}/Download` | `src/media_features/mod.rs` | match |
| `HEAD /Items/{item_id}/Download` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Items/{item_id}/PlaybackInfo` | `src/media_features/mod.rs` | match |
| `POST /Items/{item_id}/PlaybackInfo` | `src/media_features/mod.rs` | match |
| `GET /Videos/{item_id}/{media_source_id}/Subtitles/{route_index}/Stream.vtt` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Videos/{item_id}/{media_source_id}/Subtitles/{route_index}/Stream.srt` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Videos/{item_id}/{media_source_id}/Subtitles/{route_index}/{start_position_ticks}/Stream.vtt` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Videos/{item_id}/{media_source_id}/Subtitles/{route_index}/{start_position_ticks}/Stream.srt` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Videos/{item_id}/master.m3u8` | `src/media_features/mod.rs` | custom / not in target schema |
| `HEAD /Videos/{item_id}/master.m3u8` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Audio/{item_id}/master.m3u8` | `src/media_features/mod.rs` | custom / not in target schema |
| `HEAD /Audio/{item_id}/master.m3u8` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Videos/{item_id}/hls/{session_id}/playlist.m3u8` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Audio/{item_id}/hls/{session_id}/playlist.m3u8` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Videos/{item_id}/hls/{session_id}/{segment_name}` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Audio/{item_id}/hls/{session_id}/{segment_name}` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Videos/{item_id}/hls/{session_id}/subtitle.m3u8` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Videos/{item_id}/hls/{session_id}/subtitle.vtt` | `src/media_features/mod.rs` | custom / not in target schema |
| `DELETE /Videos/{item_id}/hls/{session_id}` | `src/media_features/mod.rs` | custom / not in target schema |
| `DELETE /Audio/{item_id}/hls/{session_id}` | `src/media_features/mod.rs` | custom / not in target schema |
| `POST /Videos/{item_id}/hls/{session_id}/keepalive` | `src/media_features/mod.rs` | custom / not in target schema |
| `POST /Audio/{item_id}/hls/{session_id}/keepalive` | `src/media_features/mod.rs` | custom / not in target schema |
| `GET /Puffinbox/Metadata/Refreshes` | `src/metadata/mod.rs` | custom / not in target schema |
| `POST /Puffinbox/Metadata/Refreshes` | `src/metadata/mod.rs` | custom / not in target schema |
| `GET /Puffinbox/Metadata/Items/{item_id}` | `src/metadata/mod.rs` | custom / not in target schema |
| `GET /Puffinbox/Metadata/Items/{item_id}/Artwork` | `src/metadata/mod.rs` | custom / not in target schema |
| `GET /Items/{item_id}/Images/Primary` | `src/metadata/mod.rs` | custom / not in target schema |
| `GET /Puffinbox/Offline/Settings` | `src/offline.rs` | custom / not in target schema |
| `GET /Puffinbox/Offline/Packages` | `src/offline.rs` | custom / not in target schema |
| `POST /Puffinbox/Offline/Packages` | `src/offline.rs` | custom / not in target schema |
| `DELETE /Puffinbox/Offline/Packages/{package_id}` | `src/offline.rs` | custom / not in target schema |
| `GET /Puffinbox/Offline/Packages/{package_id}` | `src/offline.rs` | custom / not in target schema |
| `GET /Puffinbox/Offline/Packages/{package_id}/Content` | `src/offline.rs` | custom / not in target schema |
| `PUT /Puffinbox/Offline/Users/{user_id}/Quota` | `src/offline.rs` | custom / not in target schema |
| `GET /Playlists` | `src/playlists.rs` | custom / not in target schema |
| `POST /Playlists` | `src/playlists.rs` | match |
| `DELETE /Playlists/{playlist_id}` | `src/playlists.rs` | custom / not in target schema |
| `GET /Playlists/{playlist_id}` | `src/playlists.rs` | match |
| `POST /Playlists/{playlist_id}` | `src/playlists.rs` | match |
| `DELETE /Playlists/{playlist_id}/Items` | `src/playlists.rs` | match |
| `GET /Playlists/{playlist_id}/Items` | `src/playlists.rs` | match |
| `POST /Playlists/{playlist_id}/Items` | `src/playlists.rs` | match |
| `POST /Playlists/{playlist_id}/Items/{entry_id}/Move/{new_index}` | `src/playlists.rs` | match |
| `GET /Puffinbox/Plugins` | `src/plugins.rs` | custom / not in target schema |
| `POST /Puffinbox/Plugins/TrustStaged` | `src/plugins.rs` | custom / not in target schema |
| `POST /Puffinbox/Plugins/{plugin_id}/Enable` | `src/plugins.rs` | custom / not in target schema |
| `POST /Puffinbox/Plugins/{plugin_id}/Disable` | `src/plugins.rs` | custom / not in target schema |

The matched-count denominator is the full target schema's operation count, not this project's declarations. Unsupported operations are not implied to be implemented.
