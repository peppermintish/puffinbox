ALTER TABLE users
    ADD COLUMN configuration JSONB NOT NULL DEFAULT '{}'::JSONB
    CHECK (jsonb_typeof(configuration) = 'object');

CREATE TABLE user_display_preferences (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    preference_id TEXT NOT NULL CHECK (octet_length(preference_id) BETWEEN 1 AND 128),
    client TEXT NOT NULL CHECK (octet_length(client) BETWEEN 1 AND 128),
    preferences JSONB NOT NULL CHECK (jsonb_typeof(preferences) = 'object'),
    PRIMARY KEY (user_id, preference_id, client)
);
