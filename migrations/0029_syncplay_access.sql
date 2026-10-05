ALTER TABLE users ADD COLUMN sync_play_access TEXT NOT NULL
    DEFAULT 'CreateAndJoinGroups'
    CHECK (sync_play_access IN ('CreateAndJoinGroups', 'JoinGroups', 'None'));
