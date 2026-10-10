-- Webhooks (decision 0017; the webhooks standard): an organization's
-- endpoints, the events recorded for them and each delivery of an event to
-- an endpoint. All three are organization-scoped: an endpoint belongs to one
-- organization and one environment, and an event stores the workspace it
-- happened in (none for the test button's) and its environment.
--
-- An endpoint's signing secret is sealed (`keys::seal`) under its
-- `webhook_signing_secret` context at `secret_version`: a data key wrapped by
-- the key service, a nonce and the ciphertext. Its last four characters are
-- kept apart, for display. `failing_since` is the first failed attempt since
-- the last success; an endpoint failing for three days is disabled.
-- `delivery_holder` is the one delivery in flight to the endpoint, held
-- until `delivery_held_until` so a worker that stops doesn't hold it forever.
--
-- An event's `body` is the exact JSON every attempt sends: lean, never
-- content or secrets. Recording an event twice records it once (its ID is
-- fixed by the change that announced it), and an endpoint gets at most one
-- delivery of each event.

CREATE TABLE webhook_endpoints (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    url TEXT NOT NULL CHECK (length(url) BETWEEN 1 AND 2048),
    description TEXT NOT NULL CHECK (length(description) <= 200),
    events TEXT NOT NULL CHECK (json_valid(events) AND json_type(events) = 'array' AND json_array_length(events) >= 1),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    secret_version INTEGER NOT NULL CHECK (secret_version >= 1),
    secret_key_id TEXT NOT NULL,
    secret_wrapped_key BLOB NOT NULL,
    secret_nonce BLOB NOT NULL CHECK (length(secret_nonce) = 12),
    secret_ciphertext BLOB NOT NULL,
    secret_last4 TEXT NOT NULL CHECK (length(secret_last4) = 4),
    created_at INTEGER NOT NULL,
    last_status_code INTEGER CHECK (last_status_code IS NULL OR last_status_code BETWEEN 0 AND 999),
    last_delivered_at INTEGER,
    failing_since INTEGER,
    delivery_holder BLOB CHECK (delivery_holder IS NULL OR length(delivery_holder) = 16),
    delivery_held_until INTEGER,
    CHECK ((last_status_code IS NULL) = (last_delivered_at IS NULL)),
    CHECK ((delivery_holder IS NULL) = (delivery_held_until IS NULL)),
    UNIQUE (organization_id, id)
);

CREATE INDEX webhook_endpoints_by_organization ON webhook_endpoints (organization_id, environment, created_at, id);

CREATE TABLE webhook_events (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    workspace_id UUID CHECK (workspace_id IS NULL OR (typeof(workspace_id) = 'blob' AND length(workspace_id) = 16)),
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    type TEXT NOT NULL CHECK (length(type) BETWEEN 1 AND 100),
    body TEXT NOT NULL CHECK (json_valid(body) AND json_type(body) = 'object'),
    created_at INTEGER NOT NULL,
    -- Only the test button's event belongs to no workspace.
    CHECK ((workspace_id IS NULL) = (type = 'webhook.test')),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, id)
);

CREATE TABLE webhook_deliveries (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    endpoint_id UUID NOT NULL,
    event_id UUID NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'succeeded', 'failed', 'skipped')),
    attempts INTEGER NOT NULL CHECK (attempts >= 0),
    last_status_code INTEGER CHECK (last_status_code IS NULL OR last_status_code BETWEEN 100 AND 999),
    last_error TEXT CHECK (last_error IS NULL OR length(last_error) BETWEEN 1 AND 100),
    last_attempt_at INTEGER,
    next_attempt_at INTEGER,
    created_at INTEGER NOT NULL,
    CHECK ((state = 'pending') OR next_attempt_at IS NULL),
    FOREIGN KEY (organization_id, endpoint_id) REFERENCES webhook_endpoints (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, event_id) REFERENCES webhook_events (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, event_id, endpoint_id)
);

CREATE INDEX webhook_deliveries_by_endpoint ON webhook_deliveries (organization_id, endpoint_id, created_at);
