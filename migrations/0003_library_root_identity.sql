CREATE TABLE library_root_identities (
    library_id UUID NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    root_path_hash CHAR(64) NOT NULL,
    root_path TEXT NOT NULL,
    device_id TEXT NOT NULL,
    inode TEXT NOT NULL,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_verified_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (library_id, root_path_hash)
);
