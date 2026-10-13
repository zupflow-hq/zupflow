-- no-transaction
-- Model providers: bring your own key (decisions 0049, 0050).
--
-- A model connection is one organization's account with one provider, in
-- one environment. Its API key is sealed (`keys::seal`) under the
-- connection's `model_provider_key` context, one row per key version in
-- `model_connection_credentials`; replacing the key stores the next version
-- and forgets the earlier ones. The connection keeps the current version and
-- the key's last four characters, for display.
--
-- A model is one the organization added from a connection, in the
-- connection's environment: it references the connection by (organization,
-- environment, connection), so a model can't be in another environment than
-- its route, and it goes with its connection. One per connection and
-- provider model.
--
-- `default_models` holds each environment's default, a model of that same
-- organization and environment (or none). An agent's `model_id` overrides
-- the default with a model of its organization; that the model is of the
-- agent's workspace's environment is checked by the store, in the
-- transaction that writes the agent. A model the default or an agent names
-- can't be deleted: the references are NO ACTION, so the database refuses
-- what the store's own check would.
--
-- Agents named a deployment's model by its ID until now; no organization
-- model matches one, so every agent runs on its environment's default until
-- it's given a model. Runs record the model they were admitted with (the
-- provider's ID, the organization's model and the connection, without
-- references, so a run outlives both) and the key version they first used;
-- each model attempt records the key version it used.
--
-- Changing a column of `agents` means rebuilding the table, and dropping a
-- parent table with foreign keys on would cascade into its children. So, as
-- SQLite documents for this, foreign keys are off while the table is rebuilt
-- in one transaction, checked before it commits, and on again after.

PRAGMA foreign_keys = OFF;

BEGIN IMMEDIATE;

CREATE TABLE model_connections (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    kind TEXT NOT NULL CHECK (kind IN ('openai', 'anthropic', 'openrouter')),
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 60),
    key_version INTEGER NOT NULL CHECK (key_version >= 1),
    key_last4 TEXT NOT NULL CHECK (length(key_last4) BETWEEN 1 AND 4),
    key_updated_at INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'invalid_key')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, environment, id)
);

CREATE INDEX model_connections_by_organization ON model_connections (organization_id, environment, created_at, id);

CREATE TABLE model_connection_credentials (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    connection_id UUID NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    key_id TEXT NOT NULL,
    wrapped_key BLOB NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce) = 12),
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, environment, connection_id)
        REFERENCES model_connections (organization_id, environment, id) ON DELETE CASCADE,
    UNIQUE (organization_id, environment, connection_id, version)
);

CREATE TABLE models (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    connection_id UUID NOT NULL,
    provider_model_id TEXT NOT NULL CHECK (length(provider_model_id) BETWEEN 1 AND 200),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    author TEXT NOT NULL CHECK (length(author) <= 100),
    context_window INTEGER CHECK (context_window IS NULL OR context_window >= 0),
    supports_tools INTEGER NOT NULL CHECK (supports_tools IN (0, 1)),
    status TEXT NOT NULL CHECK (status IN ('available', 'unavailable')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, environment, connection_id)
        REFERENCES model_connections (organization_id, environment, id) ON DELETE CASCADE,
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, environment, id),
    UNIQUE (organization_id, connection_id, provider_model_id)
);

CREATE INDEX models_by_name ON models (organization_id, environment, name, id);

CREATE TABLE default_models (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    model_id UUID CHECK (model_id IS NULL OR (typeof(model_id) = 'blob' AND length(model_id) = 16)),
    updated_at INTEGER NOT NULL,
    updated_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL,
    FOREIGN KEY (organization_id, environment, model_id) REFERENCES models (organization_id, environment, id),
    UNIQUE (organization_id, environment)
);

CREATE TABLE agents_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    description TEXT NOT NULL CHECK (length(description) <= 280),
    model_id UUID CHECK (model_id IS NULL OR (typeof(model_id) = 'blob' AND length(model_id) = 16)),
    instructions TEXT NOT NULL CHECK (length(instructions) <= 20000),
    status TEXT NOT NULL CHECK (status IN ('active', 'paused')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, model_id) REFERENCES models (organization_id, id),
    UNIQUE (organization_id, workspace_id, id)
);

INSERT INTO agents_next (id, organization_id, workspace_id, name, description, model_id, instructions, status,
                         created_at, updated_at)
SELECT id, organization_id, workspace_id, name, description, NULL, instructions, status, created_at, updated_at
FROM agents;

DROP TABLE agents;
ALTER TABLE agents_next RENAME TO agents;
CREATE INDEX agents_by_name ON agents (organization_id, workspace_id, name);
CREATE INDEX agents_by_model ON agents (organization_id, model_id) WHERE model_id IS NOT NULL;

ALTER TABLE runs ADD COLUMN model_id UUID
    CHECK (model_id IS NULL OR (typeof(model_id) = 'blob' AND length(model_id) = 16));
ALTER TABLE runs ADD COLUMN model_connection_id UUID
    CHECK (model_connection_id IS NULL OR (typeof(model_connection_id) = 'blob' AND length(model_connection_id) = 16));
ALTER TABLE runs ADD COLUMN model_key_version INTEGER CHECK (model_key_version IS NULL OR model_key_version >= 1);

ALTER TABLE run_model_attempts ADD COLUMN key_version INTEGER CHECK (key_version IS NULL OR key_version >= 1);

-- Every reference still holds, or nothing commits: a row here fails its check.
CREATE TEMP TABLE migration_0031_check (violations INTEGER NOT NULL CHECK (violations = 0));
INSERT INTO migration_0031_check SELECT count(*) FROM pragma_foreign_key_check;
DROP TABLE migration_0031_check;

COMMIT;

PRAGMA foreign_keys = ON;
