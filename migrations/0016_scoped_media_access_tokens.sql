CREATE TABLE IF NOT EXISTS media_access_tokens (
    token_hash CHAR(64) PRIMARY KEY,
    parent_token_id UUID NOT NULL REFERENCES auth_tokens(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS media_access_tokens_parent_created_idx
    ON media_access_tokens (parent_token_id, created_at DESC);
CREATE INDEX IF NOT EXISTS media_access_tokens_expiry_idx
    ON media_access_tokens (expires_at);
