-- A prior guide refresh could null program_id while retaining the recurring
-- timer's series_timer_id and airing interval. Prefer in-progress recordings
-- and cancel scheduled duplicates. If corrupt historical data contains more
-- than one active recording for an airing, preserve all of them: the migration
-- cannot safely choose which already-published/claimed recording to discard.
-- When no recording exists, retain the earliest scheduled row.
WITH ranked AS (
    SELECT id,
           status,
           ROW_NUMBER() OVER (
               PARTITION BY series_timer_id, start_at, end_at
               ORDER BY CASE status WHEN 'recording' THEN 0 ELSE 1 END,
                        created_at, id
           ) AS duplicate_number
    FROM live_tv_timers
    WHERE series_timer_id IS NOT NULL AND status IN ('scheduled', 'recording')
)
UPDATE live_tv_timers AS timer
SET status = 'cancelled',
    finished_at = COALESCE(timer.finished_at, NOW()),
    last_error_code = COALESCE(timer.last_error_code, 'cancelled'),
    updated_at = NOW()
FROM ranked
WHERE ranked.id = timer.id
  AND ranked.duplicate_number > 1
  AND ranked.status = 'scheduled';

CREATE UNIQUE INDEX live_tv_timers_series_airing_scheduled_idx
    ON live_tv_timers (series_timer_id, start_at, end_at)
    WHERE series_timer_id IS NOT NULL AND status = 'scheduled';
