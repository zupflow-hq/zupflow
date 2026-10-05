-- The two roots every scoped row hangs from. Conventions for every table:
-- UUIDs are 16-byte blobs in columns typed UUID, instants are INTEGER
-- milliseconds since the Unix epoch, UTC. `id` is declared NOT NULL because
-- SQLite, unlike the standard, lets a PRIMARY KEY column hold NULL. See
-- src/schema.rs for the rules the schema test holds every table to.

CREATE TABLE organizations (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    -- Unique across the deployment: it's in the Console's URLs.
    slug TEXT NOT NULL UNIQUE CHECK (
        length(slug) BETWEEN 1 AND 40
        AND slug NOT GLOB '*[^a-z0-9-]*'
        AND slug NOT GLOB '-*'
        AND slug NOT GLOB '*-'
    ),
    created_at INTEGER NOT NULL
);

CREATE TABLE workspaces (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    -- Fixed at creation.
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    slug TEXT NOT NULL CHECK (
        length(slug) BETWEEN 1 AND 40
        AND slug NOT GLOB '*[^a-z0-9-]*'
        AND slug NOT GLOB '-*'
        AND slug NOT GLOB '*-'
    ),
    external_id TEXT CHECK (length(external_id) BETWEEN 1 AND 120),
    created_at INTEGER NOT NULL,
    -- What workspace-scoped tables reference, so a row can't point at a
    -- workspace of another organization.
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, slug)
);
