-- no-transaction
-- Agent memory: memory types, the organization's default, each agent's
-- memory, and the `notes` kind's notes.
--
-- A memory type is one organization's named setup of a memory kind: the
-- kind's key, its settings as the kind checked them (defaults filled in) and
-- whether its agents may write. Types and the default are the
-- organization's, the same in both environments. `default_memory_types`
-- holds at most one row per organization; without one, or with no type in
-- it, agents on the default run without memory. So no organization has
-- memory until it chooses a default, and every agent starts on the default.
--
-- An agent's `memory` is `default`, `type` (with `memory_type_id`, a type of
-- its organization) or `off`. A type the default or an agent names can't be
-- deleted: those references are NO ACTION, so the database refuses what the
-- store's own check would. A type's notes go with it.
--
-- A note belongs to one agent of one workspace, in one type. Its short ID is
-- how the agent names it, unique in that scope. One saved in a direct
-- conversation names it (without referencing it) and is recalled and found
-- only there. `origin` is who wrote its current text: a run's tool call, an
-- API key or a member.
--
-- No search index is kept. A table shared by every scope would rank one
-- scope's notes by every organization's (BM25 weighs a word by how many of
-- its table's rows hold it), and would keep a forgotten note's words until
-- its segments merge. Each search indexes only the notes it may find, in a
-- temporary FTS5 table it rolls back (`src/notes.rs`).
--
-- Changing a column of `agents` means rebuilding the table, as 0031 did:
-- foreign keys are off while it's rebuilt in one transaction, checked before
-- it commits, and on again after. Its grants and channels keep referencing
-- it by name, and its indexes are made again.

PRAGMA foreign_keys = OFF;

BEGIN IMMEDIATE;

CREATE TABLE memory_types (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    kind TEXT NOT NULL CHECK (length(kind) BETWEEN 1 AND 32 AND kind NOT GLOB '*[^a-z0-9_]*'),
    settings TEXT NOT NULL CHECK (json_valid(settings) AND json_type(settings) = 'object'),
    writes TEXT NOT NULL CHECK (writes IN ('allow', 'approve', 'deny')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (organization_id, id),
    UNIQUE (organization_id, name)
);

CREATE INDEX memory_types_by_name ON memory_types (organization_id, lower(name), id);

CREATE TABLE default_memory_types (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL UNIQUE REFERENCES organizations (id) ON DELETE CASCADE,
    memory_type_id UUID
        CHECK (memory_type_id IS NULL OR (typeof(memory_type_id) = 'blob' AND length(memory_type_id) = 16)),
    updated_at INTEGER NOT NULL,
    updated_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL,
    FOREIGN KEY (organization_id, memory_type_id) REFERENCES memory_types (organization_id, id)
);

CREATE TABLE agents_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    description TEXT NOT NULL CHECK (length(description) <= 280),
    model_id UUID CHECK (model_id IS NULL OR (typeof(model_id) = 'blob' AND length(model_id) = 16)),
    memory TEXT NOT NULL DEFAULT 'default' CHECK (memory IN ('default', 'type', 'off')),
    memory_type_id UUID
        CHECK (memory_type_id IS NULL OR (typeof(memory_type_id) = 'blob' AND length(memory_type_id) = 16)),
    instructions TEXT NOT NULL CHECK (length(instructions) <= 20000),
    status TEXT NOT NULL CHECK (status IN ('active', 'paused')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, model_id) REFERENCES models (organization_id, id),
    FOREIGN KEY (organization_id, memory_type_id) REFERENCES memory_types (organization_id, id),
    CHECK ((memory = 'type') = (memory_type_id IS NOT NULL)),
    UNIQUE (organization_id, workspace_id, id)
);

INSERT INTO agents_next (id, organization_id, workspace_id, name, description, model_id, memory, memory_type_id,
                         instructions, status, created_at, updated_at)
SELECT id, organization_id, workspace_id, name, description, model_id, 'default', NULL, instructions, status,
       created_at, updated_at
FROM agents;

DROP TABLE agents;
ALTER TABLE agents_next RENAME TO agents;
CREATE INDEX agents_by_name ON agents (organization_id, workspace_id, name);
CREATE INDEX agents_by_model ON agents (organization_id, model_id) WHERE model_id IS NOT NULL;
CREATE INDEX agents_by_memory_type ON agents (organization_id, memory_type_id) WHERE memory_type_id IS NOT NULL;

CREATE TABLE memory_notes (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    agent_id UUID NOT NULL,
    memory_type_id UUID NOT NULL,
    short_id TEXT NOT NULL CHECK (length(short_id) = 6 AND short_id NOT GLOB '*[^a-z2-7]*'),
    text TEXT NOT NULL CHECK (length(text) BETWEEN 1 AND 500),
    channel_conversation_id UUID CHECK (channel_conversation_id IS NULL
        OR (typeof(channel_conversation_id) = 'blob' AND length(channel_conversation_id) = 16)),
    origin TEXT NOT NULL CHECK (json_valid(origin) AND json_type(origin) = 'object'),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, agent_id)
        REFERENCES agents (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, memory_type_id) REFERENCES memory_types (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, agent_id, memory_type_id, short_id)
);

CREATE INDEX memory_notes_by_scope
    ON memory_notes (organization_id, workspace_id, agent_id, memory_type_id, updated_at, id);
CREATE INDEX memory_notes_by_type ON memory_notes (organization_id, memory_type_id);

-- Every reference still holds, or nothing commits: a row here fails its check.
CREATE TEMP TABLE migration_0039_check (violations INTEGER NOT NULL CHECK (violations = 0));
INSERT INTO migration_0039_check SELECT count(*) FROM pragma_foreign_key_check;
DROP TABLE migration_0039_check;

COMMIT;

PRAGMA foreign_keys = ON;
