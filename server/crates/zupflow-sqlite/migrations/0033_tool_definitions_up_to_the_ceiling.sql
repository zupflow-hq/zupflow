-- Tool definitions may be larger than 64 KiB: servers list tools whose
-- descriptions and schemas run far past it, and one tool over the limit
-- refuses its whole listing. The limit itself is `MAX_DEFINITION_BYTES` in
-- `zupflow-core::tools`, checked before anything is stored; this table now
-- holds only the far higher backstop, `MAX_DEFINITION_BYTES_CEILING`
-- (16 MiB), so raising the limit up to it needs no migration.
--
-- `installation_tools` names the size in its checks, so it is rebuilt with
-- the backstop, its rows copied as they are. No table references it, so
-- dropping the old one cascades into nothing, and foreign keys stay on.

CREATE TABLE installation_tools_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    installation_id UUID NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 128),
    effect TEXT NOT NULL CHECK (effect IN ('read', 'write', 'destructive')),
    pinned_definition TEXT NOT NULL CHECK (json_valid(pinned_definition) AND length(CAST(pinned_definition AS BLOB)) <= 16777216),
    pinned_hash BLOB NOT NULL CHECK (typeof(pinned_hash) = 'blob' AND length(pinned_hash) = 32),
    latest_definition TEXT CHECK (latest_definition IS NULL OR (json_valid(latest_definition) AND length(CAST(latest_definition AS BLOB)) <= 16777216)),
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

INSERT INTO installation_tools_next (
    id, organization_id, workspace_id, installation_id, name, effect, pinned_definition, pinned_hash,
    latest_definition, latest_hash, first_seen_at, seen_at, changed_at, gone_at
)
SELECT
    id, organization_id, workspace_id, installation_id, name, effect, pinned_definition, pinned_hash,
    latest_definition, latest_hash, first_seen_at, seen_at, changed_at, gone_at
FROM installation_tools;

DROP TABLE installation_tools;
ALTER TABLE installation_tools_next RENAME TO installation_tools;
