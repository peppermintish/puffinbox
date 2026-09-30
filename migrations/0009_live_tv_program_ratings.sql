-- XMLTV labels are preserved for display. Only the explicitly recognized
-- rating systems/labels are normalized to this versioned parental ordinal;
-- unknown systems remain unrated and follow each user's unrated policy.
ALTER TABLE live_tv_programs
    ADD COLUMN rating_system TEXT CHECK (
        rating_system IS NULL OR length(rating_system) <= 64
    ),
    ADD COLUMN content_rating TEXT CHECK (
        content_rating IS NULL OR length(content_rating) <= 64
    ),
    ADD COLUMN policy_rating_scale TEXT CHECK (
        policy_rating_scale IS NULL OR policy_rating_scale = 'US-PARENTAL-v1'
    ),
    ADD COLUMN policy_rating_value SMALLINT CHECK (
        policy_rating_value IS NULL OR policy_rating_value BETWEEN 0 AND 100
    ),
    ADD CONSTRAINT live_tv_program_rating_pair_check CHECK (
        (policy_rating_scale IS NULL) = (policy_rating_value IS NULL)
    );

CREATE INDEX live_tv_program_policy_idx
    ON live_tv_programs (policy_rating_value, start_at, channel_item_id);

-- Timers and recordings keep the classification that was visible when they
-- were created/captured even when a later guide refresh removes that program.
ALTER TABLE live_tv_timers
    ADD COLUMN rating_system TEXT CHECK (
        rating_system IS NULL OR length(rating_system) <= 64
    ),
    ADD COLUMN content_rating TEXT CHECK (
        content_rating IS NULL OR length(content_rating) <= 64
    ),
    ADD COLUMN policy_rating_scale TEXT CHECK (
        policy_rating_scale IS NULL OR policy_rating_scale = 'US-PARENTAL-v1'
    ),
    ADD COLUMN policy_rating_value SMALLINT CHECK (
        policy_rating_value IS NULL OR policy_rating_value BETWEEN 0 AND 100
    ),
    ADD CONSTRAINT live_tv_timer_rating_pair_check CHECK (
        (policy_rating_scale IS NULL) = (policy_rating_value IS NULL)
    );

ALTER TABLE live_tv_recordings
    ADD COLUMN rating_system TEXT CHECK (
        rating_system IS NULL OR length(rating_system) <= 64
    ),
    ADD COLUMN content_rating TEXT CHECK (
        content_rating IS NULL OR length(content_rating) <= 64
    ),
    ADD COLUMN policy_rating_scale TEXT CHECK (
        policy_rating_scale IS NULL OR policy_rating_scale = 'US-PARENTAL-v1'
    ),
    ADD COLUMN policy_rating_value SMALLINT CHECK (
        policy_rating_value IS NULL OR policy_rating_value BETWEEN 0 AND 100
    ),
    ADD CONSTRAINT live_tv_recording_rating_pair_check CHECK (
        (policy_rating_scale IS NULL) = (policy_rating_value IS NULL)
    );
