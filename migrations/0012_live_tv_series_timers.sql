CREATE TABLE live_tv_series_timers (
    id UUID PRIMARY KEY,
    owner_user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    channel_item_id UUID NOT NULL REFERENCES live_tv_channels(item_id) ON DELETE RESTRICT,
    program_id UUID REFERENCES live_tv_programs(id) ON DELETE SET NULL,
    name TEXT NOT NULL,
    match_title TEXT NOT NULL,
    match_title_key TEXT NOT NULL,
    start_at TIMESTAMPTZ NOT NULL,
    end_at TIMESTAMPTZ NOT NULL,
    days_mask SMALLINT NOT NULL DEFAULT 127 CHECK (days_mask BETWEEN 1 AND 127),
    padding_before_seconds INTEGER NOT NULL DEFAULT 0
        CHECK (padding_before_seconds BETWEEN 0 AND 3600),
    padding_after_seconds INTEGER NOT NULL DEFAULT 0
        CHECK (padding_after_seconds BETWEEN 0 AND 3600),
    output_library_id UUID NOT NULL REFERENCES libraries(id) ON DELETE RESTRICT,
    rating_system TEXT CHECK (rating_system IS NULL OR length(rating_system) <= 64),
    content_rating TEXT CHECK (content_rating IS NULL OR length(content_rating) <= 64),
    policy_rating_scale TEXT CHECK (
        policy_rating_scale IS NULL OR policy_rating_scale = 'US-PARENTAL-v1'
    ),
    policy_rating_value SMALLINT CHECK (
        policy_rating_value IS NULL OR policy_rating_value BETWEEN 0 AND 100
    ),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CHECK (end_at > start_at),
    CHECK (end_at - start_at
        + (padding_before_seconds + padding_after_seconds) * INTERVAL '1 second'
        <= INTERVAL '4 hours'),
    CHECK (length(name) BETWEEN 1 AND 512 AND name !~ '[[:cntrl:]]'),
    CHECK (length(match_title) BETWEEN 1 AND 512),
    CHECK (length(match_title_key) BETWEEN 1 AND 1024),
    CHECK ((policy_rating_scale IS NULL) = (policy_rating_value IS NULL))
);

CREATE UNIQUE INDEX live_tv_series_timers_active_key_idx
    ON live_tv_series_timers (owner_user_id, channel_item_id, match_title_key)
    WHERE enabled = TRUE;
CREATE INDEX live_tv_series_timers_owner_idx
    ON live_tv_series_timers (owner_user_id, enabled, name, id);

ALTER TABLE live_tv_timers
    ADD COLUMN series_timer_id UUID
        REFERENCES live_tv_series_timers(id) ON DELETE SET NULL;

CREATE INDEX live_tv_timers_series_schedule_idx
    ON live_tv_timers (series_timer_id, status, start_at, id)
    WHERE series_timer_id IS NOT NULL;

CREATE INDEX live_tv_programs_series_match_idx
    ON live_tv_programs (
        channel_item_id,
        (lower(regexp_replace(btrim(title), '[[:space:]]+', ' ', 'g'))),
        start_at
    );
