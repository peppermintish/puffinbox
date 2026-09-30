CREATE TABLE live_tv_sources (
    id UUID PRIMARY KEY,
    library_id UUID NOT NULL REFERENCES libraries(id) ON DELETE RESTRICT,
    name TEXT NOT NULL,
    playlist_url TEXT NOT NULL,
    guide_url TEXT,
    origin_pins JSONB NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    refresh_status TEXT NOT NULL DEFAULT 'queued'
        CHECK (refresh_status IN ('queued', 'running', 'ready', 'failed')),
    refresh_claimed_run_id UUID,
    refresh_started_at TIMESTAMPTZ,
    last_refreshed_at TIMESTAMPTZ,
    last_error_code TEXT
        CHECK (last_error_code IS NULL OR last_error_code IN (
            'fetch-failed', 'invalid-playlist', 'invalid-guide', 'library-unavailable', 'server-restarted'
        )),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (id, library_id),
    UNIQUE (library_id, name),
    CHECK (length(name) BETWEEN 1 AND 160),
    CHECK (length(playlist_url) BETWEEN 1 AND 8192),
    CHECK (guide_url IS NULL OR length(guide_url) BETWEEN 1 AND 8192),
    CHECK (jsonb_typeof(origin_pins) = 'array'),
    CHECK ((refresh_status = 'running') = (refresh_claimed_run_id IS NOT NULL)),
    CHECK ((refresh_status = 'running') = (refresh_started_at IS NOT NULL))
);
CREATE INDEX live_tv_sources_refresh_idx
    ON live_tv_sources (refresh_status, updated_at, id)
    WHERE enabled = TRUE;

CREATE TABLE live_tv_channels (
    item_id UUID PRIMARY KEY,
    library_id UUID NOT NULL REFERENCES libraries(id) ON DELETE RESTRICT,
    source_id UUID NOT NULL,
    source_channel_id TEXT NOT NULL,
    name TEXT NOT NULL,
    group_name TEXT,
    stream_url TEXT NOT NULL,
    logo_url TEXT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    FOREIGN KEY (item_id, library_id) REFERENCES items(id, library_id) ON DELETE CASCADE,
    FOREIGN KEY (source_id, library_id) REFERENCES live_tv_sources(id, library_id) ON DELETE RESTRICT,
    UNIQUE (source_id, source_channel_id),
    UNIQUE (item_id, source_id),
    CHECK (length(source_channel_id) BETWEEN 1 AND 256),
    CHECK (length(name) BETWEEN 1 AND 512),
    CHECK (group_name IS NULL OR length(group_name) <= 512),
    CHECK (length(stream_url) BETWEEN 1 AND 8192),
    CHECK (logo_url IS NULL OR length(logo_url) <= 8192)
);
CREATE INDEX live_tv_channels_list_idx
    ON live_tv_channels (library_id, enabled, name, item_id);

CREATE TABLE live_tv_programs (
    id UUID PRIMARY KEY,
    channel_item_id UUID NOT NULL REFERENCES live_tv_channels(item_id) ON DELETE CASCADE,
    source_program_id TEXT,
    start_at TIMESTAMPTZ NOT NULL,
    end_at TIMESTAMPTZ NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    category TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (end_at > start_at),
    CHECK (end_at <= start_at + INTERVAL '48 hours'),
    CHECK (length(title) BETWEEN 1 AND 512),
    CHECK (description IS NULL OR length(description) <= 16384),
    CHECK (category IS NULL OR length(category) <= 512),
    UNIQUE (channel_item_id, start_at, title)
);
CREATE INDEX live_tv_programs_guide_idx
    ON live_tv_programs (start_at, end_at, channel_item_id);
CREATE INDEX live_tv_programs_channel_idx
    ON live_tv_programs (channel_item_id, start_at, id);

CREATE TABLE live_tv_timers (
    id UUID PRIMARY KEY,
    owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_item_id UUID NOT NULL REFERENCES live_tv_channels(item_id) ON DELETE RESTRICT,
    program_id UUID REFERENCES live_tv_programs(id) ON DELETE SET NULL,
    start_at TIMESTAMPTZ NOT NULL,
    end_at TIMESTAMPTZ NOT NULL,
    padding_before_seconds INTEGER NOT NULL DEFAULT 0 CHECK (padding_before_seconds BETWEEN 0 AND 3600),
    padding_after_seconds INTEGER NOT NULL DEFAULT 0 CHECK (padding_after_seconds BETWEEN 0 AND 3600),
    output_library_id UUID NOT NULL REFERENCES libraries(id) ON DELETE RESTRICT,
    status TEXT NOT NULL DEFAULT 'scheduled'
        CHECK (status IN ('scheduled', 'recording', 'completed', 'failed', 'interrupted', 'cancelled')),
    claimed_run_id UUID,
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    last_error_code TEXT
        CHECK (last_error_code IS NULL OR last_error_code IN (
            'source-unavailable', 'quota-exceeded', 'publish-failed', 'server-restarted', 'cancelled'
        )),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (end_at > start_at),
    CHECK ((status = 'recording') = (claimed_run_id IS NOT NULL)),
    CHECK (status NOT IN ('recording', 'completed') OR started_at IS NOT NULL),
    CHECK (status NOT IN ('scheduled', 'cancelled') OR started_at IS NULL),
    CHECK ((status IN ('completed', 'failed', 'interrupted', 'cancelled')) = (finished_at IS NOT NULL))
);
CREATE UNIQUE INDEX live_tv_timers_active_program_idx
    ON live_tv_timers (owner_user_id, program_id)
    WHERE program_id IS NOT NULL AND status IN ('scheduled', 'recording');
CREATE INDEX live_tv_timers_due_idx
    ON live_tv_timers (start_at, end_at, id)
    WHERE status = 'scheduled';
CREATE INDEX live_tv_timers_owner_idx
    ON live_tv_timers (owner_user_id, start_at, id);

CREATE TABLE live_tv_recordings (
    id UUID PRIMARY KEY,
    timer_id UUID NOT NULL UNIQUE REFERENCES live_tv_timers(id) ON DELETE RESTRICT,
    channel_item_id UUID NOT NULL REFERENCES live_tv_channels(item_id) ON DELETE RESTRICT,
    library_id UUID NOT NULL REFERENCES libraries(id) ON DELETE RESTRICT,
    channel_name TEXT NOT NULL,
    title TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    item_id UUID REFERENCES items(id) ON DELETE SET NULL,
    status TEXT NOT NULL DEFAULT 'recording'
        CHECK (status IN ('recording', 'publishing', 'completed', 'failed', 'interrupted')),
    claimed_run_id UUID,
    byte_count BIGINT NOT NULL DEFAULT 0 CHECK (byte_count >= 0),
    sha256 CHAR(64),
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at TIMESTAMPTZ,
    last_error_code TEXT
        CHECK (last_error_code IS NULL OR last_error_code IN (
            'source-unavailable', 'quota-exceeded', 'checksum-failed', 'publish-failed', 'server-restarted'
        )),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (length(relative_path) BETWEEN 1 AND 2048),
    CHECK (length(channel_name) BETWEEN 1 AND 512),
    CHECK (length(title) BETWEEN 1 AND 512),
    CHECK ((status IN ('recording', 'publishing')) = (claimed_run_id IS NOT NULL)),
    CHECK ((status IN ('completed', 'failed', 'interrupted')) = (finished_at IS NOT NULL)),
    -- A catalog item may later be removed while the recording history and
    -- checksum remain. The ON DELETE SET NULL reference must not make that
    -- historical row invalid.
    CHECK (status <> 'completed' OR sha256 IS NOT NULL),
    CHECK (sha256 IS NULL OR sha256 ~ '^[0-9a-f]{64}$')
);
CREATE INDEX live_tv_recordings_list_idx
    ON live_tv_recordings (library_id, started_at DESC, id);
CREATE INDEX live_tv_recordings_active_idx
    ON live_tv_recordings (status, started_at, id)
    WHERE status IN ('recording', 'publishing');
