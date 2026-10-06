-- Client configurations (decision 0031): a catalog app's per environment, a
-- workspace app's in its workspace's environment. Each references its app
-- with the full scope, so it can't name another organization's or
-- workspace's app, and goes with it.
--
-- The OAuth client secret is sealed (decision 0033): a data key wrapped by
-- the key service, a nonce and the ciphertext, bound to a context naming the
-- owner and `secret_version`. Its last four characters are kept apart, for
-- display. Either every secret column is set or none is.

CREATE TABLE app_configurations (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    app_id UUID NOT NULL,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    version INTEGER NOT NULL CHECK (version >= 1),
    mcp_server_url TEXT CHECK (length(mcp_server_url) BETWEEN 1 AND 500),
    oauth_client_id TEXT CHECK (length(oauth_client_id) BETWEEN 1 AND 300),
    oauth_scopes TEXT NOT NULL CHECK (json_valid(oauth_scopes) AND json_type(oauth_scopes) = 'array'),
    secret_key_id TEXT,
    secret_wrapped_key BLOB,
    secret_nonce BLOB CHECK (secret_nonce IS NULL OR length(secret_nonce) = 12),
    secret_ciphertext BLOB,
    secret_last4 TEXT,
    secret_updated_at INTEGER,
    -- Counts every secret ever written here, so it never goes back.
    secret_version INTEGER NOT NULL DEFAULT 0 CHECK (secret_version >= 0),
    updated_at INTEGER NOT NULL,
    -- Nobody, once that user is deleted, or when an API key wrote it.
    updated_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL,
    CHECK (
        (secret_key_id IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_wrapped_key IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_nonce IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_last4 IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_updated_at IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_ciphertext IS NULL OR secret_version >= 1)
    ),
    FOREIGN KEY (organization_id, app_id) REFERENCES apps (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, app_id, environment)
);

CREATE TABLE workspace_app_configurations (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    app_id UUID NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    mcp_server_url TEXT CHECK (length(mcp_server_url) BETWEEN 1 AND 500),
    oauth_client_id TEXT CHECK (length(oauth_client_id) BETWEEN 1 AND 300),
    oauth_scopes TEXT NOT NULL CHECK (json_valid(oauth_scopes) AND json_type(oauth_scopes) = 'array'),
    secret_key_id TEXT,
    secret_wrapped_key BLOB,
    secret_nonce BLOB CHECK (secret_nonce IS NULL OR length(secret_nonce) = 12),
    secret_ciphertext BLOB,
    secret_last4 TEXT,
    secret_updated_at INTEGER,
    secret_version INTEGER NOT NULL DEFAULT 0 CHECK (secret_version >= 0),
    updated_at INTEGER NOT NULL,
    updated_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL,
    CHECK (
        (secret_key_id IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_wrapped_key IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_nonce IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_last4 IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_updated_at IS NULL) = (secret_ciphertext IS NULL)
        AND (secret_ciphertext IS NULL OR secret_version >= 1)
    ),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, app_id)
        REFERENCES workspace_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, app_id)
);
