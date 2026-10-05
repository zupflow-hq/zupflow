-- API keys, stored as a keyed hash. And instance settings: values that belong
-- to this Zupflow installation, not to any tenant.

CREATE TABLE api_keys (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    prefix TEXT NOT NULL CHECK (length(prefix) BETWEEN 1 AND 40),
    last4 TEXT NOT NULL CHECK (length(last4) = 4),
    -- HMAC-SHA256 of the secret under the server's key; unique, because the
    -- hash alone finds the key when a request presents it.
    secret_hash BLOB NOT NULL UNIQUE CHECK (typeof(secret_hash) = 'blob' AND length(secret_hash) = 32),
    -- No cascade: a key outlives its creator's membership, and a user who
    -- created keys can't be deleted without deciding what happens to them.
    created_by_user_id UUID NOT NULL REFERENCES users (id),
    created_at INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at INTEGER,
    UNIQUE (organization_id, id)
);

CREATE INDEX api_keys_by_organization ON api_keys (organization_id, created_at);

CREATE TABLE instance_settings (
    name TEXT NOT NULL PRIMARY KEY CHECK (length(name) BETWEEN 1 AND 100),
    value BLOB NOT NULL
);
