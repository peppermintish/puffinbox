-- Studios are names shared across libraries, resolved from visible metadata.
-- They are not playable filesystem items. Always authorize the current catalog
-- before reading or changing this user-owned preference.
CREATE TABLE user_studio_favorites (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    studio_id UUID NOT NULL,
    PRIMARY KEY (user_id, studio_id)
);
