-- Sign-in through the MCP server (decision 0039).
--
-- The clients Zupflow registers with MCP servers' authorization servers
-- (RFC 7591): one per catalog app, environment and issuer, or per workspace
-- app and issuer, with the endpoints discovery found there. A different
-- issuer later is a new registration; the old one stays. The unique key is
-- what keeps concurrent first sign-ins to one registration.
--
-- What the server issued beside the client ID (a client secret, a
-- registration access token) is sealed together as one value under the
-- registration's own context: a data key wrapped by the key service, a nonce
-- and the ciphertext. Either every sealed column is set or none is, and a
-- client that authenticates with Basic has its secret sealed.

CREATE TABLE oauth_client_registrations (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    app_id UUID NOT NULL,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    issuer TEXT NOT NULL CHECK (length(issuer) BETWEEN 1 AND 2048),
    authorization_endpoint TEXT NOT NULL CHECK (length(authorization_endpoint) BETWEEN 1 AND 2048),
    token_endpoint TEXT NOT NULL CHECK (length(token_endpoint) BETWEEN 1 AND 2048),
    registration_endpoint TEXT NOT NULL CHECK (length(registration_endpoint) BETWEEN 1 AND 2048),
    client_id TEXT NOT NULL CHECK (length(client_id) BETWEEN 1 AND 512),
    token_endpoint_auth_method TEXT NOT NULL
        CHECK (token_endpoint_auth_method IN ('none', 'client_secret_basic')),
    key_id TEXT,
    wrapped_key BLOB,
    nonce BLOB CHECK (nonce IS NULL OR length(nonce) = 12),
    ciphertext BLOB,
    created_at INTEGER NOT NULL,
    CHECK (
        (key_id IS NULL) = (ciphertext IS NULL)
        AND (wrapped_key IS NULL) = (ciphertext IS NULL)
        AND (nonce IS NULL) = (ciphertext IS NULL)
        AND (token_endpoint_auth_method = 'none' OR ciphertext IS NOT NULL)
    ),
    FOREIGN KEY (organization_id, app_id) REFERENCES apps (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, app_id, environment, issuer)
);

CREATE TABLE workspace_oauth_client_registrations (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    app_id UUID NOT NULL,
    issuer TEXT NOT NULL CHECK (length(issuer) BETWEEN 1 AND 2048),
    authorization_endpoint TEXT NOT NULL CHECK (length(authorization_endpoint) BETWEEN 1 AND 2048),
    token_endpoint TEXT NOT NULL CHECK (length(token_endpoint) BETWEEN 1 AND 2048),
    registration_endpoint TEXT NOT NULL CHECK (length(registration_endpoint) BETWEEN 1 AND 2048),
    client_id TEXT NOT NULL CHECK (length(client_id) BETWEEN 1 AND 512),
    token_endpoint_auth_method TEXT NOT NULL
        CHECK (token_endpoint_auth_method IN ('none', 'client_secret_basic')),
    key_id TEXT,
    wrapped_key BLOB,
    nonce BLOB CHECK (nonce IS NULL OR length(nonce) = 12),
    ciphertext BLOB,
    created_at INTEGER NOT NULL,
    CHECK (
        (key_id IS NULL) = (ciphertext IS NULL)
        AND (wrapped_key IS NULL) = (ciphertext IS NULL)
        AND (nonce IS NULL) = (ciphertext IS NULL)
        AND (token_endpoint_auth_method = 'none' OR ciphertext IS NOT NULL)
    ),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, app_id)
        REFERENCES workspace_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, app_id, issuer)
);

-- Where an `mcp_oauth` installation signed in, as part of its template
-- snapshot (decision 0036): the issuer and token endpoint discovered, the
-- client it signed in as and the server its token is for. JSON; only beside
-- a snapshot.
ALTER TABLE installations ADD COLUMN oauth_client TEXT
    CHECK (
        oauth_client IS NULL
        OR (json_valid(oauth_client) AND json_type(oauth_client) = 'object' AND auth_template IS NOT NULL)
    );
