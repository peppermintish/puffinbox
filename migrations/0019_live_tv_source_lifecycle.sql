ALTER TABLE live_tv_sources
    ADD COLUMN deleted_at TIMESTAMPTZ;

ALTER TABLE live_tv_sources
    ALTER COLUMN playlist_url DROP NOT NULL,
    ALTER COLUMN origin_pins DROP NOT NULL;

ALTER TABLE live_tv_sources
    DROP CONSTRAINT live_tv_sources_playlist_url_check,
    DROP CONSTRAINT live_tv_sources_origin_pins_check,
    DROP CONSTRAINT live_tv_sources_library_id_name_key;

ALTER TABLE live_tv_sources
    ADD CONSTRAINT live_tv_sources_playlist_url_check
        CHECK (playlist_url IS NULL OR length(playlist_url) BETWEEN 1 AND 8192),
    ADD CONSTRAINT live_tv_sources_origin_pins_check
        CHECK (origin_pins IS NULL OR jsonb_typeof(origin_pins) = 'array'),
    ADD CONSTRAINT live_tv_sources_lifecycle_check
        CHECK (
            (deleted_at IS NULL AND playlist_url IS NOT NULL AND origin_pins IS NOT NULL)
            OR (deleted_at IS NOT NULL AND playlist_url IS NULL AND guide_url IS NULL
                AND origin_pins IS NULL AND enabled = FALSE AND refresh_status <> 'running')
        );

CREATE UNIQUE INDEX live_tv_sources_active_library_name_idx
    ON live_tv_sources (library_id, name)
    WHERE deleted_at IS NULL;

ALTER TABLE live_tv_channels
    ALTER COLUMN stream_url DROP NOT NULL,
    DROP CONSTRAINT live_tv_channels_stream_url_check,
    ADD CONSTRAINT live_tv_channels_stream_url_check
        CHECK (stream_url IS NULL OR length(stream_url) BETWEEN 1 AND 8192),
    ADD CONSTRAINT live_tv_channels_stream_url_enabled_check
        CHECK (stream_url IS NOT NULL OR enabled = FALSE);

-- Disabled channel rows are retained only for timer/recording history. Remove
-- stale feed URLs and logos unless an active capture still needs the URL for
-- recording or restart recovery.
UPDATE live_tv_channels c
SET stream_url=NULL,logo_url=NULL,updated_at=NOW()
WHERE c.enabled=FALSE
  AND NOT EXISTS (
      SELECT 1 FROM live_tv_recordings r
      WHERE r.channel_item_id=c.item_id AND r.status IN ('recording','publishing')
  );

CREATE FUNCTION clear_disabled_live_tv_channel_urls_after_capture()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.status IN ('recording','publishing')
       AND NEW.status NOT IN ('recording','publishing') THEN
        UPDATE live_tv_channels c
        SET stream_url=NULL,logo_url=NULL,updated_at=NOW()
        WHERE c.item_id=NEW.channel_item_id
          AND c.enabled=FALSE
          AND NOT EXISTS (
              SELECT 1 FROM live_tv_recordings r
              WHERE r.channel_item_id=c.item_id
                AND r.status IN ('recording','publishing')
          );
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER clear_disabled_live_tv_channel_urls_after_capture
AFTER UPDATE OF status ON live_tv_recordings
FOR EACH ROW
EXECUTE FUNCTION clear_disabled_live_tv_channel_urls_after_capture();
