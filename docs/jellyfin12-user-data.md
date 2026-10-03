# Jellyfin 12 user-data edits

Puffinbox applies partial edits to scanned items through `POST /UserItems/{itemId}/UserData`. Omitted and null fields preserve stored values. Explicit edits to `Played` do not count as playback events or supply a last-played date. Music playback still applies its separate [start, progress and stop policy](jellyfin12-playback-behavior.md).

The contract comes from the public [Jellyfin 12.0 OpenAPI schema](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json) and public requests to an isolated official Jellyfin 12.0.0 container. Each observation used a fresh temporary account, which was deleted afterward. Original FLAC bytes, existing accounts and their playback state were unchanged. No Jellyfin implementation code was inspected or copied. The test runtime is external to Puffinbox's distribution.

## Observed behavior

| Input | Result |
| --- | --- |
| `Played`, `IsFavorite`, `PlaybackPositionTicks`, `PlayCount`, `LastPlayedDate` | Each non-null value replaces that field alone. An explicit played flag does not increment the count or change the date. Offset dates normalize to UTC. |
| Missing fields, null fields or an empty object | Existing values stay unchanged. Null does not clear the date or personal rating. |
| `Likes=true` / `Likes=false` | Stores a personal rating of 10 / 1. |
| `Rating` | Accepts values from 0 through 10. An explicit rating takes precedence when `Likes` is also supplied. |
| Reading `Likes` | Null when there is no personal rating; true at ratings of 6.5 or higher, false below 6.5. Boundary observations include 6.4999, 6.5 and 6.5001. |
| `PlayedPercentage` | Ignored on writes. Reads derive it from positive saved position and known runtime, independently of `Played`. Zero position has no percentage, and a position beyond runtime can produce more than 100 percent. |
| Body `ItemId`, `Key`, `UnplayedItemCount` | Do not select another item or replace stored user data. The route selects the item. |

The private ledger under `.local/user-data-contract-20261003` preserves the schema hash and five sets of public observations, including invalid values, rating precedence, date offsets and percentage boundaries. The initial checker used an incorrect fixture title; its temporary account was deleted before any user-data write. The later boundary helper had a syntax error before execution. Those attempts remain separate from the completed observations.

The current-image replay on core `57309b4b` matched 46 cases across eight response fields and fresh independent reads. Two negative-value cases retain the constraints below; three cases requiring negative stored baselines were skipped and are not passing results. Replay accounts were deleted afterward, with original fixtures and existing user state unchanged. Exact observations and image hashes are under `.local/user-data-client-20261003`. Current-source checks, migration, container restart and the official clients' four-track album checks are recorded in [acceptance.md](acceptance.md).

## Puffinbox implementation and limits

Count, date and rating edits share the existing transactional run fence. Concurrent partial updates preserve each other's omitted fields. Personal ratings live in `user_item_data`, independently of metadata's community or parental ratings. Migration `0026` adds a nullable rating column; old rows keep their existing state and have no personal rating.

The authenticated user can edit their own visible items. An administrator can select another user with `userId`; that target user's media permissions still apply. Hidden, denied, disabled-library and restricted items remain unavailable. Legacy user-data aliases use the same implementation. Committed notifications contain the new count, date, rating and derived like value and stay scoped to the affected user.

Puffinbox retains nonnegative count and position constraints and the existing maximum saved-position bound. The reference accepted negative counts and positions, so these invalid-input responses remain a qualified difference. Playback and mark-played actions saturate an edited maximum 32-bit count instead of overflowing. That overflow policy is covered locally but has no reference playback observation.

Reference media-specific `Key` values, virtual-item user-data writes, rating-based catalogue filters and complete user-data behavior remain incomplete. Ended duplicate playback reports also retain Puffinbox's protection for later explicit edits, as described in the music-state record. These changes do not clear the Jellyfin behavior or release gates.
