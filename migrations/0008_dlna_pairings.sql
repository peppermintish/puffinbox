-- DLNA is opt-in and each renderer is paired to one user and one exact LAN
-- IPv4 address. The application rechecks user and item policy on every
-- ContentDirectory and media request; this row is only the pairing record.
CREATE TABLE dlna_pairings (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    client_address INET NOT NULL,
    device_name TEXT NOT NULL CHECK (
        length(device_name) BETWEEN 1 AND 128
        AND device_name !~ '[[:cntrl:]]'
    ),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ,
    CHECK (family(client_address) = 4),
    CHECK (masklen(client_address) = 32),
    CHECK (client_address <> '0.0.0.0'::inet),
    CHECK (NOT (client_address <<= '224.0.0.0/4'::inet)),
    CHECK (client_address <> '255.255.255.255'::inet)
);

CREATE UNIQUE INDEX dlna_pairings_client_address_idx
    ON dlna_pairings (client_address);
CREATE INDEX dlna_pairings_address_enabled_idx
    ON dlna_pairings (client_address, id)
    WHERE enabled = TRUE;
