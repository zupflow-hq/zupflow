-- Agents and their grants (decision 0040). An agent belongs to one
-- workspace. A grant gives it tools of one installation of that same
-- workspace: it references both by (organization_id, workspace_id, id), so
-- the database itself refuses another workspace's agent or installation, and
-- it goes with either. One grant per agent and installation.
--
-- A grant's tools and approvals are JSON arrays of tool names, kept as
-- given. Each named tool was in the installation's registry, and not gone,
-- when the grant was written; a tool that goes later stays in the grant, and
-- the run engine refuses the call.

CREATE TABLE agents (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    description TEXT NOT NULL CHECK (length(description) <= 280),
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 100),
    instructions TEXT NOT NULL CHECK (length(instructions) <= 20000),
    status TEXT NOT NULL CHECK (status IN ('active', 'paused')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);

CREATE INDEX agents_by_name ON agents (organization_id, workspace_id, name);

CREATE TABLE agent_grants (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    agent_id UUID NOT NULL,
    installation_id UUID NOT NULL,
    tools TEXT NOT NULL CHECK (json_valid(tools) AND json_type(tools) = 'array' AND json_array_length(tools) >= 1),
    approval_required TEXT NOT NULL
        CHECK (json_valid(approval_required) AND json_type(approval_required) = 'array'),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, agent_id)
        REFERENCES agents (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, installation_id)
        REFERENCES installations (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, agent_id, installation_id)
);

CREATE INDEX agent_grants_by_installation ON agent_grants (organization_id, workspace_id, installation_id);
