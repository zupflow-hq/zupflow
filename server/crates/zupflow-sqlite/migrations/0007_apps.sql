-- Apps (decision 0031): the organization's catalog, and each workspace's own,
-- in separate tables so both follow the scoping rules with no exception.
-- Slugs are unique within their owner: the catalog, or one workspace.
-- An app's authentication template and tools are JSON in the contract's
-- shapes; the store writes and reads them, nothing queries inside them.

CREATE TABLE apps (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('mcp')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    slug TEXT NOT NULL CHECK (
        length(slug) BETWEEN 1 AND 40
        AND slug NOT GLOB '*[^a-z0-9-]*'
        AND slug NOT GLOB '-*'
        AND slug NOT GLOB '*-'
    ),
    description TEXT NOT NULL CHECK (length(description) <= 280),
    icon_glyph TEXT NOT NULL CHECK (length(icon_glyph) BETWEEN 1 AND 4),
    icon_color TEXT NOT NULL CHECK (length(icon_color) = 7 AND icon_color GLOB '#[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]'),
    auth TEXT NOT NULL CHECK (json_valid(auth) AND json_type(auth) = 'object'),
    tools TEXT NOT NULL CHECK (json_valid(tools) AND json_type(tools) = 'array'),
    version INTEGER NOT NULL CHECK (version >= 1),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- What installations and configurations reference, with the organization.
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, slug)
);

CREATE INDEX apps_by_name ON apps (organization_id, lower(name), id);

CREATE TABLE workspace_apps (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('mcp')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    slug TEXT NOT NULL CHECK (
        length(slug) BETWEEN 1 AND 40
        AND slug NOT GLOB '*[^a-z0-9-]*'
        AND slug NOT GLOB '-*'
        AND slug NOT GLOB '*-'
    ),
    description TEXT NOT NULL CHECK (length(description) <= 280),
    icon_glyph TEXT NOT NULL CHECK (length(icon_glyph) BETWEEN 1 AND 4),
    icon_color TEXT NOT NULL CHECK (length(icon_color) = 7 AND icon_color GLOB '#[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]'),
    auth TEXT NOT NULL CHECK (json_valid(auth) AND json_type(auth) = 'object'),
    tools TEXT NOT NULL CHECK (json_valid(tools) AND json_type(tools) = 'array'),
    version INTEGER NOT NULL CHECK (version >= 1),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- Goes with its workspace, and can't name a workspace of another organization.
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, slug)
);

CREATE INDEX workspace_apps_by_name ON workspace_apps (organization_id, workspace_id, lower(name), id);
