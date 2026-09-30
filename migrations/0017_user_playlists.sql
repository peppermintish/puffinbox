CREATE TABLE IF NOT EXISTS playlists (
    id UUID PRIMARY KEY,
    owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 255),
    media_type TEXT NOT NULL DEFAULT 'Audio' CHECK (media_type = 'Audio'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS playlists_owner_name_idx
    ON playlists (owner_user_id, lower(name), id);

CREATE TABLE IF NOT EXISTS playlist_items (
    id UUID PRIMARY KEY,
    playlist_id UUID NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    item_id UUID NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT playlist_items_position_unique UNIQUE (playlist_id, position)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX IF NOT EXISTS playlist_items_item_idx ON playlist_items (item_id, playlist_id);
