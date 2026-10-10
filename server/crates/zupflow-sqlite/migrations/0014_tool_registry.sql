-- The tool registry (decision 0038): the tools an installation's MCP server
-- lists to it, one row per tool name. Each installation keeps its own: a
-- server can list different tools to different accounts, and no workspace
-- reaches another. The first definition listed is pinned; a later one that
-- hashes differently is kept as the latest, unreviewed, beside it; a tool
-- missing from a complete listing is gone. Definitions are canonical JSON,
-- hashes their SHA-256.

CREATE TABLE installation_tools (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    installation_id UUID NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 128),
    effect TEXT NOT NULL CHECK (effect IN ('read', 'write', 'destructive')),
    pinned_definition TEXT NOT NULL CHECK (json_valid(pinned_definition) AND length(pinned_definition) <= 65536),
    pinned_hash BLOB NOT NULL CHECK (typeof(pinned_hash) = 'blob' AND length(pinned_hash) = 32),
    latest_definition TEXT CHECK (latest_definition IS NULL OR (json_valid(latest_definition) AND length(latest_definition) <= 65536)),
    latest_hash BLOB CHECK (latest_hash IS NULL OR (typeof(latest_hash) = 'blob' AND length(latest_hash) = 32)),
    first_seen_at INTEGER NOT NULL,
    seen_at INTEGER NOT NULL,
    changed_at INTEGER,
    gone_at INTEGER,
    CHECK (
        (latest_definition IS NULL) = (latest_hash IS NULL)
        AND (latest_hash IS NULL) = (changed_at IS NULL)
        AND (latest_hash IS NULL OR latest_hash != pinned_hash)
    ),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, installation_id)
        REFERENCES installations (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, installation_id, name)
);
