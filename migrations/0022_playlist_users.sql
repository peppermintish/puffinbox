CREATE TABLE IF NOT EXISTS playlist_users (
    playlist_id UUID NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    can_edit BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (playlist_id, user_id)
);

CREATE INDEX IF NOT EXISTS playlist_users_user_idx ON playlist_users (user_id, playlist_id);
