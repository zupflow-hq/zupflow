-- OAuth for hosted Connect (decision 0037), and the template snapshot an
-- installation keeps (decision 0036).

-- How many authorizations a session has started. Only the latest one's
-- `state` completes it, and a start names the attempt it follows, so two
-- racing starts can't both win.
ALTER TABLE connect_sessions ADD COLUMN authorization_attempt INTEGER NOT NULL DEFAULT 0
    CHECK (authorization_attempt >= 0);

-- A session's authorization in progress: the hash of the `state` sent to the
-- provider, unique across the deployment because the callback finds its
-- session by that hash alone, and what the callback needs (the PKCE verifier
-- and the link's reference), sealed under the attempt's context. At most one
-- per session; the callback deletes it as it claims it, so a `state` works
-- once.
CREATE TABLE connect_authorizations (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    session_id UUID NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt >= 1),
    state_hash BLOB NOT NULL UNIQUE CHECK (typeof(state_hash) = 'blob' AND length(state_hash) = 32),
    key_id TEXT NOT NULL,
    wrapped_key BLOB NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce) = 12),
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, session_id)
        REFERENCES connect_sessions (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, session_id)
);

-- The app's authentication template and version an installation connected
-- under. Both are set by every connect and reconnect from now on; an
-- installation that connected before has neither, never one without the
-- other. (SQLite checks a column's constraint against the whole row.)
ALTER TABLE installations ADD COLUMN auth_template TEXT
    CHECK (auth_template IS NULL OR (json_valid(auth_template) AND json_type(auth_template) = 'object'));
ALTER TABLE installations ADD COLUMN app_version INTEGER
    CHECK ((app_version IS NULL) = (auth_template IS NULL) AND (app_version IS NULL OR app_version >= 0));
