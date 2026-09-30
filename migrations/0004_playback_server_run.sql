ALTER TABLE playback_sessions
    ADD COLUMN instance_run_id UUID;

UPDATE playback_sessions
SET instance_run_id = '00000000-0000-0000-0000-000000000000'
WHERE instance_run_id IS NULL;

ALTER TABLE playback_sessions
    ALTER COLUMN instance_run_id SET NOT NULL;

CREATE INDEX playback_sessions_run_active_idx
    ON playback_sessions (instance_run_id, user_id, device_id)
    WHERE ended_at IS NULL;
