-- A hosted Connect request's idempotency key belongs to its session (the
-- API standard: "API key, member or Connect session"). SQLite can't widen a
-- CHECK in place, so the table is rebuilt with a third caller kind, its rows
-- copied as they are.

CREATE TABLE idempotency_keys_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    workspace_id UUID,
    -- Who sent the key: a member, an API key or a Connect session, by its
    -- UUID, and the environment it acted in (none for a member on an
    -- organization route).
    caller_kind TEXT NOT NULL CHECK (caller_kind IN ('member', 'api_key', 'connect_session')),
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
    -- A Connect session's key always names the session's workspace.
    CHECK (caller_kind <> 'connect_session' OR (workspace_id IS NOT NULL AND caller_environment IS NOT NULL)),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE
);

INSERT INTO idempotency_keys_next
SELECT id, organization_id, workspace_id, caller_kind, caller_id, caller_environment, key, fingerprint, state,
       status, body, lock_token, locked_until, created_at
FROM idempotency_keys;

DROP TABLE idempotency_keys;
ALTER TABLE idempotency_keys_next RENAME TO idempotency_keys;

CREATE UNIQUE INDEX idempotency_keys_by_key ON idempotency_keys (
    organization_id, coalesce(workspace_id, x''), caller_kind, caller_id, coalesce(caller_environment, ''), key
);
CREATE INDEX idempotency_keys_by_age ON idempotency_keys (organization_id, created_at);
