-- Preserve independently requested metadata and image work across retries
-- and server restarts. Existing extension jobs keep their full-refresh scope.
ALTER TABLE metadata_refresh_runs
    ADD COLUMN refresh_metadata BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN refresh_images BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN replace_metadata BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN replace_images BOOLEAN NOT NULL DEFAULT TRUE;
