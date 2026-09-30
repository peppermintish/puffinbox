CREATE TABLE IF NOT EXISTS instance_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS users (
    id UUID PRIMARY KEY,
    username TEXT NOT NULL,
    username_norm TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    is_admin BOOLEAN NOT NULL DEFAULT FALSE,
    disabled BOOLEAN NOT NULL DEFAULT FALSE,
    enable_remote_access BOOLEAN NOT NULL DEFAULT TRUE,
    allow_media_playback BOOLEAN NOT NULL DEFAULT TRUE,
    restrict_libraries BOOLEAN NOT NULL DEFAULT FALSE,
    max_parental_rating SMALLINT CHECK (max_parental_rating BETWEEN 0 AND 100),
    block_unrated_items TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS users_username_norm_idx ON users (username_norm);

CREATE TABLE IF NOT EXISTS libraries (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    collection_type TEXT NOT NULL DEFAULT 'mixed',
    locations JSONB NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    recursive BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (jsonb_typeof(locations) = 'array'),
    UNIQUE (name)
);

CREATE TABLE IF NOT EXISTS user_library_access (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    library_id UUID NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, library_id)
);
CREATE INDEX IF NOT EXISTS user_library_access_library_idx ON user_library_access (library_id, user_id);

CREATE TABLE IF NOT EXISTS items (
    id UUID PRIMARY KEY,
    library_id UUID NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    parent_id UUID,
    name TEXT NOT NULL,
    sort_name TEXT NOT NULL,
    item_type TEXT NOT NULL,
    path TEXT NOT NULL,
    path_hash CHAR(64) NOT NULL,
    container TEXT,
    size_bytes BIGINT CHECK (size_bytes IS NULL OR size_bytes >= 0),
    runtime_ticks BIGINT CHECK (runtime_ticks IS NULL OR runtime_ticks >= 0),
    date_added TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    date_modified TIMESTAMPTZ,
    rating SMALLINT CHECK (rating BETWEEN 0 AND 100),
    overview TEXT,
    metadata_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    last_seen_scan UUID,
    search_document TSVECTOR GENERATED ALWAYS AS (
        to_tsvector('simple', coalesce(name, '') || ' ' || coalesce(overview, ''))
    ) STORED,
    UNIQUE (library_id, path_hash),
    UNIQUE (id, library_id),
    FOREIGN KEY (parent_id, library_id) REFERENCES items(id, library_id) ON DELETE CASCADE,
    CHECK (jsonb_typeof(metadata_json) = 'object')
);
CREATE INDEX IF NOT EXISTS items_library_parent_sort_idx ON items (library_id, parent_id, sort_name, id);
CREATE INDEX IF NOT EXISTS items_global_sort_idx ON items (sort_name, id, library_id);
CREATE INDEX IF NOT EXISTS items_library_rating_sort_idx ON items (library_id, rating, sort_name, id);
CREATE INDEX IF NOT EXISTS items_library_scan_idx ON items (library_id, last_seen_scan);
CREATE INDEX IF NOT EXISTS items_type_idx ON items (item_type, id);
CREATE INDEX IF NOT EXISTS items_rating_idx ON items (rating, library_id);
CREATE INDEX IF NOT EXISTS items_search_document_idx ON items USING GIN (search_document);

CREATE TABLE IF NOT EXISTS user_item_data (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id UUID NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    played BOOLEAN NOT NULL DEFAULT FALSE,
    play_count INTEGER NOT NULL DEFAULT 0 CHECK (play_count >= 0),
    is_favorite BOOLEAN NOT NULL DEFAULT FALSE,
    playback_position_ticks BIGINT NOT NULL DEFAULT 0 CHECK (playback_position_ticks >= 0),
    last_played_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, item_id)
);
CREATE INDEX IF NOT EXISTS user_item_data_item_idx ON user_item_data (item_id, user_id);
CREATE INDEX IF NOT EXISTS user_item_data_favorites_idx ON user_item_data (user_id, is_favorite, item_id);

CREATE TABLE IF NOT EXISTS auth_tokens (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash CHAR(64) NOT NULL UNIQUE,
    client TEXT NOT NULL DEFAULT 'unknown',
    device_name TEXT NOT NULL DEFAULT 'unknown',
    device_id TEXT NOT NULL DEFAULT 'unknown',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS auth_tokens_user_active_idx ON auth_tokens (user_id, expires_at) WHERE revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS auth_tokens_expiry_idx ON auth_tokens (expires_at);

CREATE TABLE IF NOT EXISTS login_throttles (
    bucket_hash CHAR(64) PRIMARY KEY,
    failures INTEGER NOT NULL DEFAULT 0,
    window_started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    locked_until TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS library_scan_state (
    library_id UUID PRIMARY KEY REFERENCES libraries(id) ON DELETE CASCADE,
    scan_id UUID,
    status TEXT NOT NULL DEFAULT 'idle',
    files_seen BIGINT NOT NULL DEFAULT 0,
    directories_seen BIGINT NOT NULL DEFAULT 0,
    items_indexed BIGINT NOT NULL DEFAULT 0,
    errors BIGINT NOT NULL DEFAULT 0,
    skipped_entries BIGINT NOT NULL DEFAULT 0,
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    last_error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS playback_sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    item_id UUID REFERENCES items(id) ON DELETE SET NULL,
    device_id TEXT NOT NULL DEFAULT 'unknown',
    device_name TEXT NOT NULL DEFAULT 'unknown',
    client TEXT NOT NULL DEFAULT 'unknown',
    play_method TEXT,
    position_ticks BIGINT NOT NULL DEFAULT 0 CHECK (position_ticks >= 0),
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_activity_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS playback_sessions_active_idx ON playback_sessions (user_id, last_activity_at) WHERE ended_at IS NULL;
