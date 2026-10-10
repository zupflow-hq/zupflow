-- A completed create session names the installation it made, as a reconnect
-- names the one it renews, so the hosted page and the Console can open it.
-- SQLite can't change a CHECK in place, so the table is rebuilt with the
-- rule relaxed for completed creates, its rows copied as they are.

CREATE TABLE connect_sessions_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    catalog_app_id UUID,
    workspace_app_id UUID,
    configuration_version INTEGER NOT NULL CHECK (configuration_version >= 0),
    intent TEXT NOT NULL CHECK (intent IN ('create', 'reconnect')),
    installation_id UUID,
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 40),
    status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'expired', 'cancelled', 'failed')),
    return_url TEXT CHECK (length(return_url) BETWEEN 1 AND 2000),
    requested_scopes TEXT NOT NULL CHECK (json_valid(requested_scopes) AND json_type(requested_scopes) = 'array'),
    reference_hash BLOB NOT NULL UNIQUE CHECK (typeof(reference_hash) = 'blob' AND length(reference_hash) = 32),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    completed_at INTEGER,
    failure_code TEXT CHECK (length(failure_code) BETWEEN 1 AND 60),
    CHECK ((catalog_app_id IS NULL) <> (workspace_app_id IS NULL)),
    -- A reconnect always names its installation; a create names one only once it has made it.
    CHECK (intent = 'create' OR installation_id IS NOT NULL),
    CHECK (intent = 'reconnect' OR installation_id IS NULL OR status = 'completed'),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, catalog_app_id) REFERENCES apps (organization_id, id),
    FOREIGN KEY (organization_id, workspace_id, workspace_app_id)
        REFERENCES workspace_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, installation_id)
        REFERENCES installations (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);

INSERT INTO connect_sessions_next
SELECT id, organization_id, workspace_id, environment, catalog_app_id, workspace_app_id, configuration_version,
       intent, installation_id, label, status, return_url, requested_scopes, reference_hash, created_at,
       expires_at, completed_at, failure_code
FROM connect_sessions;

DROP TABLE connect_sessions;
ALTER TABLE connect_sessions_next RENAME TO connect_sessions;

CREATE INDEX connect_sessions_by_workspace ON connect_sessions (organization_id, workspace_id, created_at);
