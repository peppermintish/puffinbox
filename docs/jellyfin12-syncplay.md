# SyncPlay groups

Puffinbox implements the initial group APIs: `GET /SyncPlay/List`,
`GET /SyncPlay/{id}`, and `POST /SyncPlay/New`, `/Join` and `/Leave`.
This is a foundation for synchronized playback. Queue management, timing,
readiness, buffering and playback commands remain incomplete.

The contract was checked through the official public
[Jellyfin 12 schema](https://repo.jellyfin.org/releases/openapi/stable/jellyfin-openapi-12.0.json)
and HTTP/WebSocket observations from the isolated reference image
`sha256:baba630419915985442f315f08b0cf46d9f4c8a0cc4bd38e94a6d35751dd5ef5`.
No server implementation was used. Private observations are retained under
`.local/next-up-contract-20261005/syncplay-contract/`.

Groups start in `Idle`. Creation and joining send `GroupJoined` followed by the
initial `Stop` command. Existing members receive `UserJoined`; leaving sends
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
Revoked, expired and disabled sessions are removed on subsequent checks.
Empty groups disappear. A replacement server run fences old actions.

There are at most 128 groups, 32 sessions per group and four groups per user.
Groups live in this server process and disappear on restart. Closing a socket
alone does not revoke its token or immediately remove membership. Broader
disconnect handling and official-client SyncPlay acceptance remain open.

The database regression covers initial messages, separate sessions with the
same claimed device ID, repeated joins, policy changes, token revocation and
expiry, disabled users, cleanup, group limits and replacement-run rejection.
It also checks malformed joins, the observed Unicode name limit, response
creation timestamps, and policy changes that commit while a request is waiting.
The reference's generated IDs, timestamps and error-body details are not
claimed as byte-for-byte matches. Puffinbox rejects control characters in group
names, honors explicit SyncPlay restrictions on administrators, and requires
media-playback permission for group access; the observed reference permits
administrator overrides and group access with playback disabled.

These checks do not establish synchronized playback. Shared queues must enforce
every participant's current library and parental restrictions before they can
be exposed or played. That behavior and the remaining SyncPlay operations still
need implementation and validation.
