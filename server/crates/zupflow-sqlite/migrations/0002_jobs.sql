-- The database-backed job queue. A job's scope is its organization plus at
-- most one of a workspace or an environment, matching `Scope` exactly.

CREATE TABLE jobs (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    workspace_id UUID,
    environment TEXT CHECK (environment IN ('sandbox', 'production')),
    kind TEXT NOT NULL CHECK (length(kind) BETWEEN 1 AND 100),
    payload BLOB NOT NULL CHECK (length(payload) <= 65536),
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'dead')),
    attempts INTEGER NOT NULL CHECK (attempts >= 0),
    max_attempts INTEGER NOT NULL CHECK (max_attempts BETWEEN 1 AND 100),
    -- When a queued job is due.
    run_at INTEGER NOT NULL,
    -- The holder's token and when its lease runs out, set exactly while running.
    lease_token BLOB CHECK (lease_token IS NULL OR length(lease_token) = 16),
    lease_expires_at INTEGER,
    -- Why a dead job died: a short code, never a message.
    reason TEXT CHECK (reason IS NULL OR length(reason) BETWEEN 1 AND 100),
    created_at INTEGER NOT NULL,
    CHECK (workspace_id IS NULL OR environment IS NULL),
    CHECK ((state = 'running') = (lease_token IS NOT NULL AND lease_expires_at IS NOT NULL)),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE
);

-- What `claim_next` reads: due queued jobs, and running jobs by lease expiry.
CREATE INDEX jobs_queued ON jobs (kind, run_at) WHERE state = 'queued';
CREATE INDEX jobs_running ON jobs (lease_expires_at) WHERE state = 'running';
