-- Tag names have persistent identities independent of filesystem folders.
-- Retain inactive identities so favorites survive a later reappearance.
CREATE TABLE music_tag_artists (
    artist_id UUID PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED,
    library_id UUID NOT NULL REFERENCES libraries(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (name=btrim(name) AND octet_length(name) BETWEEN 1 AND 512),
    name_norm TEXT GENERATED ALWAYS AS (lower(name)) STORED,
    UNIQUE (library_id,name_norm)
);

CREATE TABLE music_tag_artist_sources (
    item_id UUID NOT NULL,
    provider_key TEXT NOT NULL CHECK (provider_key IN ('local-nfo','embedded-audio')),
    role TEXT NOT NULL CHECK (role IN ('artists','albumArtists')),
    artist_id UUID NOT NULL REFERENCES music_tag_artists(artist_id) ON DELETE CASCADE,
    source_name TEXT NOT NULL CHECK (octet_length(source_name) BETWEEN 1 AND 512),
    PRIMARY KEY (item_id,provider_key,role,artist_id,source_name),
    FOREIGN KEY (item_id,provider_key) REFERENCES item_metadata(item_id,provider_key) ON DELETE CASCADE
);
CREATE INDEX music_tag_artist_sources_artist_idx ON music_tag_artist_sources(artist_id,item_id);
