# SyncPlay

Puffinbox implements the 22 public group, queue and playback-control operations,
plus `/GetUtcTime` and its legacy `/GetUTCTime` spelling. Source/database protocol
checks cover the flows described here. Packaged-image and official-client
SyncPlay acceptance remain pending; synchronized playback is not yet validated.

The contract was checked through the official public
[Jellyfin 12 schema](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json)
and HTTP/WebSocket observations from the isolated reference image
`sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`.
No server implementation was used. Private observations are retained under
`.local/next-up-contract-20261005/syncplay-contract/`.

Groups start in `Idle`. Creation and joining an empty group send `GroupJoined`
followed by `Stop`. Joining a loaded group sends its queue. Joining during playback
pauses peers while the new participant loads. Existing members receive `UserJoined`; leaving sends
`GroupLeft` and `UserLeft`. Missing joins return 204 with `GroupDoesNotExist`,
and leaving without membership returns 204 with `NotInGroup`. The socket
envelopes use 32-character group IDs; `GroupLeft` carries the hyphenated ID.
Omitted or null join IDs use the same missing-group response. Names are trimmed
and may be empty; the 200-unit UTF-16 limit includes 100 supplementary emoji.
Participant names are deduplicated when
several sessions belong to the same user. Repeated joins resend the group state.

Membership belongs to the authenticated token session. Reusing or claiming
another device ID cannot select another session's membership or notifications.
The persisted `SyncPlayAccess` policy supports `CreateAndJoinGroups`,
`JoinGroups` and `None`; ordinary users cannot change their own policy.
Each action and socket delivery rechecks active tokens and current user policy.
Expiry is evaluated against the current database clock after lock waits; a
transaction's earlier start time cannot authorize an expired session.
Revoked, expired and disabled sessions are removed on subsequent checks.
Empty groups disappear. A replacement server run fences old actions.

There are at most 128 groups, 32 sessions per group, four groups per user,
512 entries per queue and 4,096 entries across the registry. Groups live in this
server process and disappear on restart. Closing the last socket removes that
session's membership while preserving its login token. Another socket using the
same token keeps membership alive. HTTP-only groups remain valid until that
session first uses a socket. Cleanup retains its socket slot while awaiting
database work, so disconnects cannot spawn unbounded background tasks.

Each queue entry has its own `PlaylistItemId`; duplicate media items remain
distinct entries. Set, append, insert next, move, remove, select, next and previous
update the shared queue. Repeat One keeps the same entry for next and previous.
Shuffle keeps the current entry first; returning to Sorted restores the edited
order. Moves clamp to the first or last position. Broader edits while shuffle is
active still need comparison.

Loading and seeking wait for readiness. Once required participants are ready,
the server schedules `Unpause` at least one second ahead, with reported latency
included. Buffering freezes the server position and pauses peers while retaining
their readiness. Ready reports that drift beyond half a second receive `Seek`;
reports after playback starts receive the scheduled command again. Pause, stop,
seek, readiness and ignore-wait use the shared state. The clock returns request
reception and response transmission timestamps. Wall-clock corrections, unusual
latency, audible synchronization and long sessions still need client validation.

Every queue mutation and socket delivery reloads current participant policies,
enabled libraries, Live TV availability and item/parent policy ratings. Hidden
paths, inaccessible media and incomplete or cyclic ancestry are rejected.
Ancestry is bounded to 64 items. Users who lose access are removed before more
queue data is sent. Inaccessible queued groups are hidden from list/detail/join.
Separate queue and command revisions discard stale commands while retaining the
queue needed by a newer seek. A queue replacement invalidates older queue frames.

The database regressions cover initial messages, separate sessions with the
same claimed device ID, repeated joins, policy changes, token revocation and
expiry, disabled users, cleanup, group limits and replacement-run rejection.
It also checks malformed joins, the observed Unicode name limit, response
creation timestamps, and policy changes that commit while a request is waiting.
They also cover duplicate entries, queue edits, repeat and shuffle restoration,
readiness, buffering, scheduled commands, drift correction, UTC clock, parent
ratings, library-grant revocation, multiple sockets and last-socket cleanup.
The reference's generated IDs, timestamps and error-body details are not
claimed as byte-for-byte matches. Puffinbox rejects control characters in group
names, honors explicit SyncPlay restrictions on administrators, and requires
media-playback permission for group access; the observed reference permits
administrator overrides and group access with playback disabled. Puffinbox
rejects negative positions, excessive latency reports, oversized queues and
missing or inaccessible media with generic errors. The observed reference
accepted negative start positions and silently ignored a nonexistent-only queue.
JSON response profiles, wider queue edge cases and full client behavior remain
unvalidated. These checks do not establish synchronized playback or clear the
release gates.
