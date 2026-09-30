-- Policy checks may resolve many catalog items against their completed DVR
-- snapshot. Keep that lookup indexed without indexing unfinished recordings.
CREATE INDEX live_tv_recordings_item_policy_idx
    ON live_tv_recordings (item_id)
    WHERE status = 'completed'
      AND policy_rating_scale = 'US-PARENTAL-v1';
