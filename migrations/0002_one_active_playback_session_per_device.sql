WITH ranked_active AS (
    SELECT id,
           ROW_NUMBER() OVER (
               PARTITION BY user_id, device_id
               ORDER BY last_activity_at DESC, id
           ) AS rank
    FROM playback_sessions
    WHERE ended_at IS NULL
)
UPDATE playback_sessions AS sessions
SET ended_at = NOW(), last_activity_at = NOW()
FROM ranked_active
WHERE sessions.id = ranked_active.id
  AND ranked_active.rank > 1;

CREATE UNIQUE INDEX playback_sessions_one_active_device_idx
    ON playback_sessions (user_id, device_id)
    WHERE ended_at IS NULL;
