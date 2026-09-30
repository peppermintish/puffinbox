-- Provider metadata is additive and remains separate from scanner-owned
-- items.metadata_json. Scope IDs are immutable job snapshots rather than
-- foreign keys so deleting a library/item does not invalidate job history.
CREATE TABLE metadata_refresh_runs (
    id UUID PRIMARY KEY,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('library', 'item')),
    scope_library_id UUID,
    scope_library_name TEXT CHECK (scope_library_name IS NULL OR length(scope_library_name) <= 256),
    scope_item_id UUID,
    requested_by UUID REFERENCES users(id) ON DELETE SET NULL,
    provider_key TEXT NOT NULL CHECK (
        provider_key IN ('local-nfo', 'tvmaze')
        OR provider_key ~ '^plugin:[a-z0-9][a-z0-9._-]{0,63}$'
    ),
    status TEXT NOT NULL CHECK (
        status IN ('queued', 'running', 'retry_wait', 'completed', 'completed_with_errors', 'failed', 'cancelled')
    ),
    -- Every active worker claim is bound to the server process run that owns
    -- it. Startup recovery clears claims from older runs before requeueing.
    claimed_run_id UUID,
    -- Jobs walk stable UUID keysets in bounded pages instead of materializing
    -- the entire library or imposing an arbitrary whole-job item ceiling.
    cursor_item_id UUID,
    upper_item_id UUID,
    batch_limit SMALLINT NOT NULL DEFAULT 250 CHECK (batch_limit BETWEEN 1 AND 500),
    items_seen BIGINT NOT NULL DEFAULT 0 CHECK (items_seen >= 0),
    items_succeeded BIGINT NOT NULL DEFAULT 0 CHECK (items_succeeded >= 0),
    items_errors BIGINT NOT NULL DEFAULT 0 CHECK (items_errors >= 0),
    attempt_count SMALLINT NOT NULL DEFAULT 0 CHECK (attempt_count BETWEEN 0 AND 5),
    next_attempt_at TIMESTAMPTZ,
    last_error_code TEXT CHECK (last_error_code IS NULL OR length(last_error_code) <= 80),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (
        (scope_kind = 'library' AND scope_library_id IS NOT NULL AND scope_item_id IS NULL)
        OR (scope_kind = 'item' AND scope_item_id IS NOT NULL AND scope_library_id IS NULL)
    ),
    CHECK (scope_kind <> 'library' OR scope_library_name IS NOT NULL),
    CHECK ((status = 'running') = (claimed_run_id IS NOT NULL)),
    CHECK (items_succeeded + items_errors <= items_seen)
);

CREATE INDEX metadata_refresh_queue_idx
    ON metadata_refresh_runs (next_attempt_at, created_at, id)
    WHERE status IN ('queued', 'retry_wait');

CREATE INDEX metadata_refresh_claimed_run_idx
    ON metadata_refresh_runs (claimed_run_id, id)
    WHERE status = 'running';

CREATE INDEX metadata_refresh_library_idx
    ON metadata_refresh_runs (scope_library_id, created_at DESC, id)
    WHERE scope_kind = 'library';

CREATE INDEX metadata_refresh_item_idx
    ON metadata_refresh_runs (scope_item_id, created_at DESC, id)
    WHERE scope_kind = 'item';

CREATE UNIQUE INDEX metadata_refresh_active_scope_idx
    ON metadata_refresh_runs (scope_library_id, provider_key)
    WHERE scope_kind = 'library' AND status IN ('queued', 'running', 'retry_wait');

CREATE UNIQUE INDEX metadata_refresh_active_item_idx
    ON metadata_refresh_runs (scope_item_id, provider_key)
    WHERE scope_kind = 'item' AND status IN ('queued', 'running', 'retry_wait');

CREATE TABLE item_metadata (
    item_id UUID NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    provider_key TEXT NOT NULL CHECK (
        provider_key IN ('local-nfo', 'tvmaze')
        OR provider_key ~ '^plugin:[a-z0-9][a-z0-9._-]{0,63}$'
    ),
    external_id TEXT CHECK (external_id IS NULL OR length(external_id) <= 256),
    title TEXT CHECK (title IS NULL OR length(title) <= 512),
    overview TEXT CHECK (overview IS NULL OR length(overview) <= 20000),
    premiere_date DATE,
    genres JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(genres) = 'array' AND jsonb_array_length(genres) <= 128),
    metadata_json JSONB NOT NULL DEFAULT '{}'::JSONB
        CHECK (jsonb_typeof(metadata_json) = 'object' AND pg_column_size(metadata_json) <= 65536),
    -- content_rating preserves provider/NFO text. The separate policy fields
    -- are populated only by explicitly recognized local US-MPAA-v1 labels.
    content_rating TEXT CHECK (content_rating IS NULL OR length(content_rating) <= 64),
    policy_rating_scale TEXT CHECK (policy_rating_scale IS NULL OR policy_rating_scale = 'US-MPAA-v1'),
    policy_rating_value SMALLINT CHECK (policy_rating_value IS NULL OR policy_rating_value BETWEEN 0 AND 100),
    artwork_mime TEXT CHECK (artwork_mime IS NULL OR artwork_mime IN ('image/jpeg', 'image/png', 'image/webp')),
    artwork_size INTEGER CHECK (artwork_size IS NULL OR artwork_size BETWEEN 1 AND 4194304),
    artwork_sha256 CHAR(64) CHECK (artwork_sha256 IS NULL OR artwork_sha256 ~ '^[0-9a-f]{64}$'),
    artwork_bytes BYTEA CHECK (artwork_bytes IS NULL OR octet_length(artwork_bytes) BETWEEN 1 AND 4194304),
    attribution_name TEXT CHECK (attribution_name IS NULL OR length(attribution_name) <= 256),
    attribution_url TEXT CHECK (attribution_url IS NULL OR length(attribution_url) <= 1024),
    attribution_license TEXT CHECK (attribution_license IS NULL OR length(attribution_license) <= 64),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (item_id, provider_key),
    CHECK ((artwork_size IS NULL) = (artwork_mime IS NULL)),
    CHECK ((artwork_size IS NULL) = (artwork_sha256 IS NULL)),
    CHECK ((artwork_size IS NULL) = (artwork_bytes IS NULL)),
    CHECK (artwork_size IS NULL OR artwork_size = octet_length(artwork_bytes)),
    CHECK ((policy_rating_scale IS NULL) = (policy_rating_value IS NULL))
);

CREATE TABLE trusted_plugins (
    plugin_id TEXT PRIMARY KEY CHECK (plugin_id ~ '^[a-z0-9][a-z0-9._-]{0,63}$'),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 128),
    version TEXT NOT NULL CHECK (length(version) BETWEEN 1 AND 64),
    api_version SMALLINT NOT NULL CHECK (api_version = 1),
    manifest_sha256 CHAR(64) NOT NULL CHECK (manifest_sha256 ~ '^[0-9a-f]{64}$'),
    binary_sha256 CHAR(64) NOT NULL CHECK (binary_sha256 ~ '^[0-9a-f]{64}$'),
    -- These are operator-supplied manifest claims; recording them does not
    -- verify third-party licensing or provenance.
    declared_license TEXT NOT NULL CHECK (length(declared_license) BETWEEN 1 AND 128),
    declared_provenance TEXT NOT NULL CHECK (length(declared_provenance) BETWEEN 1 AND 1024),
    enabled BOOLEAN NOT NULL DEFAULT FALSE,
    status TEXT NOT NULL DEFAULT 'disabled' CHECK (status IN ('disabled', 'enabled', 'invalid', 'error')),
    last_error_code TEXT CHECK (last_error_code IS NULL OR length(last_error_code) <= 80),
    installed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
