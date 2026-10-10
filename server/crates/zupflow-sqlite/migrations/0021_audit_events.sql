-- The audit log: who changed what in an organization. Each store writes the
-- event in the transaction of the change it records.
--
-- Organization-scoped, and it keeps history: an event of a workspace keeps
-- the workspace's ID after the workspace is deleted, so `workspace_id` has
-- no reference to `workspaces` (the one exception to that rule; see
-- `schema.rs`). A store writes it only from a workspace the organization
-- holds at the time. Events go with their organization.
--
-- Actor and target are kept as they were: a type, the UUID (none for
-- Zupflow itself, or a target with no ID) and the name then. Neither
-- references its row, which may go. Never a secret or content.

CREATE TABLE audit_events (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    workspace_id UUID CHECK (workspace_id IS NULL OR (typeof(workspace_id) = 'blob' AND length(workspace_id) = 16)),
    environment TEXT CHECK (environment IS NULL OR environment IN ('sandbox', 'production')),
    actor_type TEXT NOT NULL CHECK (actor_type IN ('user', 'api_key', 'system')),
    actor_id UUID CHECK (actor_id IS NULL OR (typeof(actor_id) = 'blob' AND length(actor_id) = 16)),
    actor_name TEXT NOT NULL CHECK (length(actor_name) <= 4096),
    action TEXT NOT NULL CHECK (length(action) BETWEEN 1 AND 100),
    target_type TEXT NOT NULL CHECK (length(target_type) BETWEEN 1 AND 100),
    target_id UUID CHECK (target_id IS NULL OR (typeof(target_id) = 'blob' AND length(target_id) = 16)),
    target_name TEXT NOT NULL CHECK (length(target_name) <= 4096),
    created_at INTEGER NOT NULL,
    CHECK ((actor_id IS NULL) = (actor_type = 'system')),
    -- An event in a workspace is in the workspace's environment.
    CHECK (workspace_id IS NULL OR environment IS NOT NULL),
    UNIQUE (organization_id, id)
);

-- Newest first, for the organization and for one workspace.
CREATE INDEX audit_events_by_organization ON audit_events (organization_id, created_at, id);
CREATE INDEX audit_events_by_workspace ON audit_events (organization_id, workspace_id, created_at, id);
