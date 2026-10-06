-- Installations (decisions 0031 and 0034): an app connected to one account
-- in one workspace. Each names exactly one app: a catalog app, through
-- (organization_id, catalog_app_id), or its own workspace's, through
-- (organization_id, workspace_id, workspace_app_id). So the database itself
-- refuses another organization's app, or another workspace's.
--
-- The connection's account is kept in columns that are all set or all unset.

CREATE TABLE installations (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    catalog_app_id UUID,
    workspace_app_id UUID,
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 40),
    status TEXT NOT NULL CHECK (status IN ('connected', 'reconnect_required', 'disconnected')),
    configuration_version INTEGER NOT NULL CHECK (configuration_version >= 0),
    account_external_id TEXT CHECK (length(account_external_id) BETWEEN 1 AND 300),
    account_display_name TEXT CHECK (length(account_display_name) BETWEEN 1 AND 300),
    account_email TEXT CHECK (length(account_email) BETWEEN 3 AND 320),
    granted_scopes TEXT NOT NULL CHECK (json_valid(granted_scopes) AND json_type(granted_scopes) = 'array'),
    credential_version INTEGER CHECK (credential_version >= 1),
    connected_at INTEGER,
    expires_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK ((catalog_app_id IS NULL) <> (workspace_app_id IS NULL)),
    CHECK (
        (account_external_id IS NULL) = (account_display_name IS NULL)
        AND (account_external_id IS NULL) = (credential_version IS NULL)
        AND (account_external_id IS NULL) = (connected_at IS NULL)
        AND (account_external_id IS NOT NULL OR (account_email IS NULL AND expires_at IS NULL))
    ),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    -- Catalog apps aren't deleted; were one to be, its installations must be dealt with first.
    FOREIGN KEY (organization_id, catalog_app_id) REFERENCES apps (organization_id, id),
    FOREIGN KEY (organization_id, workspace_id, workspace_app_id)
        REFERENCES workspace_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);

CREATE INDEX installations_by_workspace ON installations (organization_id, workspace_id, created_at);
CREATE INDEX installations_by_catalog_app ON installations (organization_id, catalog_app_id, created_at, id);

-- The credential an installation holds: one sealed value per installation,
-- of the version it was sealed under. Disconnecting or deleting the
-- installation deletes it.
CREATE TABLE installation_credentials (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    installation_id UUID NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    key_id TEXT NOT NULL,
    wrapped_key BLOB NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce) = 12),
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, installation_id)
        REFERENCES installations (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, installation_id)
);
