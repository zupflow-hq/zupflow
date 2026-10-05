-- Idempotency keys (decision 0016). Two tables, so neither has a nullable
-- scope in its key: organization-scoped keys, which carry the workspace on
-- workspace routes and go with it, and the keys a user sends when creating an
-- organization, before there is one.

CREATE TABLE idempotency_keys (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    workspace_id UUID,
    -- Who sent the key: a member or an API key, by its UUID, and the
    -- environment it acted in (none for a member on an organization route).
    caller_kind TEXT NOT NULL CHECK (caller_kind IN ('member', 'api_key')),
    caller_id UUID NOT NULL CHECK (typeof(caller_id) = 'blob' AND length(caller_id) = 16),
    caller_environment TEXT CHECK (caller_environment IS NULL OR caller_environment IN ('sandbox', 'production')),
    key TEXT NOT NULL CHECK (length(key) BETWEEN 1 AND 255),
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) BETWEEN 1 AND 400),
    state TEXT NOT NULL CHECK (state IN ('running', 'completed')),
    status INTEGER CHECK (status IS NULL OR status BETWEEN 100 AND 499),
    body BLOB,
    lock_token BLOB CHECK (lock_token IS NULL OR length(lock_token) = 16),
    locked_until INTEGER,
    created_at INTEGER NOT NULL,
    CHECK ((state = 'running') = (lock_token IS NOT NULL AND locked_until IS NOT NULL)),
    CHECK ((state = 'completed') = (status IS NOT NULL)),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE
);

-- One row per key and caller, a missing workspace or environment counting as
-- one value. Every lookup names these same expressions, so it uses this index.
CREATE UNIQUE INDEX idempotency_keys_by_key ON idempotency_keys (
    organization_id, coalesce(workspace_id, x''), caller_kind, caller_id, coalesce(caller_environment, ''), key
);
CREATE INDEX idempotency_keys_by_age ON idempotency_keys (organization_id, created_at);

CREATE TABLE user_idempotency_keys (
    user_id UUID NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    key TEXT NOT NULL CHECK (length(key) BETWEEN 1 AND 255),
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) BETWEEN 1 AND 400),
    state TEXT NOT NULL CHECK (state IN ('running', 'completed')),
    status INTEGER CHECK (status IS NULL OR status BETWEEN 100 AND 499),
    body BLOB,
    lock_token BLOB CHECK (lock_token IS NULL OR length(lock_token) = 16),
    locked_until INTEGER,
    created_at INTEGER NOT NULL,
    CHECK ((state = 'running') = (lock_token IS NOT NULL AND locked_until IS NOT NULL)),
    CHECK ((state = 'completed') = (status IS NOT NULL)),
    PRIMARY KEY (user_id, key)
);
