-- Connect (decision 0005): an organization's allowlists per environment, and
-- the sessions its users connect accounts through.

CREATE TABLE connect_settings (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    return_urls TEXT NOT NULL CHECK (json_valid(return_urls) AND json_type(return_urls) = 'array'),
    allowed_origins TEXT NOT NULL CHECK (json_valid(allowed_origins) AND json_type(allowed_origins) = 'array'),
    updated_at INTEGER NOT NULL,
    UNIQUE (organization_id, environment)
);

-- A session names exactly one app, as an installation does (0031), and for a
-- reconnect the installation it renews. Its link's reference is stored only
-- as a SHA-256, unique across the deployment: the hosted page finds its
-- session by that hash alone, the one lookup that starts without a tenant.
CREATE TABLE connect_sessions (
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
    CHECK ((intent = 'reconnect') = (installation_id IS NOT NULL)),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, catalog_app_id) REFERENCES apps (organization_id, id),
    FOREIGN KEY (organization_id, workspace_id, workspace_app_id)
        REFERENCES workspace_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, installation_id)
        REFERENCES installations (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);

CREATE INDEX connect_sessions_by_workspace ON connect_sessions (organization_id, workspace_id, created_at);
