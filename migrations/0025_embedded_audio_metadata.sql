ALTER TABLE metadata_refresh_runs DROP CONSTRAINT metadata_refresh_runs_provider_key_check;
ALTER TABLE metadata_refresh_runs ADD CONSTRAINT metadata_refresh_runs_provider_key_check CHECK (
    provider_key IN ('local-nfo','embedded-audio','tvmaze')
    OR provider_key ~ '^plugin:[a-z0-9][a-z0-9._-]{0,63}$'
);
ALTER TABLE metadata_refresh_runs ADD COLUMN rerun_requested BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE item_metadata DROP CONSTRAINT item_metadata_provider_key_check;
ALTER TABLE item_metadata ADD CONSTRAINT item_metadata_provider_key_check CHECK (
    provider_key IN ('local-nfo','embedded-audio','tvmaze')
    OR provider_key ~ '^plugin:[a-z0-9][a-z0-9._-]{0,63}$'
);

-- Embedded metadata belongs to the exact catalog file snapshot that was
-- probed. Readers discard it as soon as a scan records a different snapshot.
ALTER TABLE item_metadata ADD COLUMN source_library_id UUID;
ALTER TABLE item_metadata ADD COLUMN source_path_hash CHAR(64);
ALTER TABLE item_metadata ADD COLUMN source_size_bytes BIGINT;
ALTER TABLE item_metadata ADD COLUMN source_date_modified TIMESTAMPTZ;
ALTER TABLE item_metadata ADD COLUMN search_document TSVECTOR
    GENERATED ALWAYS AS (to_tsvector('simple', COALESCE(title,''))) STORED;
CREATE INDEX item_metadata_title_search_idx ON item_metadata USING GIN(search_document);
ALTER TABLE item_metadata ADD CONSTRAINT item_metadata_source_identity_check CHECK (
    (provider_key='embedded-audio' AND source_library_id IS NOT NULL
        AND source_path_hash IS NOT NULL AND source_path_hash ~ '^[a-f0-9]{64}$'
        AND source_size_bytes IS NOT NULL AND source_size_bytes>=0
        AND source_date_modified IS NOT NULL
        AND content_rating IS NULL AND policy_rating_value IS NULL)
    OR (provider_key<>'embedded-audio' AND source_library_id IS NULL
        AND source_path_hash IS NULL AND source_size_bytes IS NULL
        AND source_date_modified IS NULL)
);
