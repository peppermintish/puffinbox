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

Reference media-specific `Key` values, virtual-item user-data writes and complete user-data behavior remain incomplete. Ended duplicate playback reports also retain Puffinbox's protection for later explicit edits, as described in the music-state record. These changes do not clear the Jellyfin behavior or release gates.

## Personal catalogue filters

Scanned-item queries accept `Filters=Likes`, `Dislikes` and `IsFavoriteOrLikes`, including the legacy user-item route. Likes selects personal ratings of 6.5 or higher. Dislikes selects everything outside that set, including null ratings and items without a saved user-data row. This differs from reading `Likes`, which remains null on an unrated item.

The official Jellyfin 12.0.0 fixture returned favorites only for IsFavoriteOrLikes, excluding an unfavorited track even when its personal rating was 10. Puffinbox follows that observed selection. It does not infer a favorite-or-liked union from the enum name. Combining either favorite selector with Likes or Dislikes applies both conditions; combining Likes and Dislikes returns 400 in either order. Repeated Likes and lowercase filter values are accepted.

The public reference ledger under `.local/personal-filter-contract-20261003` covers 23 selections and a repeat with six additional unrated-row and maximum-rating observations. It includes the 6.4999/6.5 boundary, favorite combinations, played state, resume, paging, exclusions, search and administrator-selected user data. Each run used a fresh temporary account and deleted it afterward; original files and existing user data were unchanged. The first helper failed while parsing a non-JSON error body. A subsequent helper edit had a syntax error before execution. Both failed records remain separate from the completed observations.

Selections apply before totals and paging and use the selected user's data and media permissions. Private, hidden, disabled and restricted items remain excluded. Virtual playlist catalogue filters remain unavailable and return 400 instead of silently ignoring a personal filter. Community-rating, critic-rating and broader catalogue filters remain incomplete; these observations cover the scanned-audio fixture only.

On core `3e5f0684`, the replay matched 28 selections and totals from 29 reference observations. Both catalogues were restricted to the same four original FLAC files. Several unpaged results differ in order, and one page remains qualified: the reference's Audio SortName includes padded disc/track prefixes, while Puffinbox currently sorts audio by display title. The page matched Puffinbox's independently read full filtered order; reference paging compatibility remains open. Exact queries, ID mappings and orders are under `.local/personal-filter-client-20261003`. Temporary replay accounts were deleted, with original files and existing client state unchanged.
