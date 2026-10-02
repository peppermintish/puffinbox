ALTER TABLE playback_sessions
    ADD COLUMN stop_reported_at TIMESTAMPTZ,
    ADD COLUMN stop_user_data_updated_at TIMESTAMPTZ,
    ADD COLUMN stop_completed BOOLEAN NOT NULL DEFAULT FALSE;
