ALTER TABLE auth_tokens
    ADD COLUMN capabilities JSONB NOT NULL DEFAULT '{}'::JSONB
    CHECK (jsonb_typeof(capabilities) = 'object');
