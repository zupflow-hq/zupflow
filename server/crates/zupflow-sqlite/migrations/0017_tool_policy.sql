-- Tool permissions (decision 0040): the organization's tool policy and its
-- level for each tool of its catalog apps.
--
-- One policy per organization; an organization without a row reads the
-- defaults (`manual`, new tools at `approve`). A level is kept per catalog
-- app and tool name, referencing the app with its organization, so it can't
-- name another organization's app or a workspace's own, and goes with it. A
-- level is suggested from a tool's hints (`confirmed` 0) until a person sets
-- it; only a confirmed level is in force. A name nobody listed yet can be
-- set ahead of it.

CREATE TABLE tool_policies (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    mode TEXT NOT NULL CHECK (mode IN ('manual')),
    new_tool_level TEXT NOT NULL CHECK (new_tool_level IN ('approve', 'deny')),
    updated_at INTEGER NOT NULL,
    -- Nobody, once that user is deleted, or when an API key wrote it.
    updated_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL,
    UNIQUE (organization_id)
);

CREATE TABLE app_tool_levels (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    app_id UUID NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 128),
    level TEXT NOT NULL CHECK (level IN ('allow', 'approve', 'deny')),
    confirmed INTEGER NOT NULL CHECK (confirmed IN (0, 1)),
    destructive INTEGER NOT NULL CHECK (destructive IN (0, 1)),
    updated_at INTEGER NOT NULL,
    -- Who confirmed it: nobody while suggested, once that user is deleted, or when an API key set it.
    updated_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL,
    CHECK (confirmed = 1 OR updated_by_user_id IS NULL),
    FOREIGN KEY (organization_id, app_id) REFERENCES apps (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, app_id, name)
);
