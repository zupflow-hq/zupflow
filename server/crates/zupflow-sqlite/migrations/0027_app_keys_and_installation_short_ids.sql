-- no-transaction
-- App keys and installation short IDs (decision 0047).
--
-- An app's kebab-case slug becomes a snake-case key: 1 to 24 characters,
-- starting with a letter, single underscores between letters and digits.
-- Unique across the whole organization: its catalog and every workspace's
-- apps share one key space. Each table holds its own keys unique; that
-- neither repeats the other's spans two tables, so the store checks it in
-- the same transaction as the insert.
--
-- Every installation gets a short ID: six characters of lowercase base32,
-- unique in its workspace. Existing installations get theirs from their order
-- in the workspace, spelled through a bijection on 30 bits, so no two
-- collide. Each one issued is recorded in
-- `installation_short_ids`, which outlives the installation, so a short ID
-- is never issued twice in a workspace; an installation references its row.
--
-- Changing a column's checks means rebuilding the table, and dropping a
-- parent table with foreign keys on would cascade into its children. So,
-- as SQLite documents for this, foreign keys are off while the tables are
-- rebuilt in one transaction, checked before it commits, and on again after.

PRAGMA foreign_keys = OFF;

BEGIN IMMEDIATE;

CREATE TABLE installation_short_ids (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    short_id TEXT NOT NULL CHECK (length(short_id) = 6 AND short_id NOT GLOB '*[^a-z2-7]*'),
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, short_id)
);

-- Catalog apps. An existing slug becomes its key with hyphens as
-- underscores; one that doesn't make a valid key gets `app_` and the last 20
-- hex digits of its ID instead.
CREATE TABLE apps_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('mcp')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    key TEXT NOT NULL CHECK (
        length(key) BETWEEN 1 AND 24
        AND key GLOB '[a-z]*'
        AND key NOT GLOB '*[^a-z0-9_]*'
        AND key NOT GLOB '*__*'
        AND key NOT GLOB '*_'
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
    UNIQUE (organization_id, key)
);

INSERT INTO apps_next
    (id, organization_id, kind, name, key, description, icon_glyph, icon_color, auth, tools, version,
     created_at, updated_at)
SELECT id, organization_id, kind, name,
       CASE
           WHEN length(replace(slug, '-', '_')) <= 24
                AND replace(slug, '-', '_') GLOB '[a-z]*'
                AND replace(slug, '-', '_') NOT GLOB '*__*'
           THEN replace(slug, '-', '_')
           ELSE 'app_' || substr(lower(hex(id)), 13, 20)
       END,
       description, icon_glyph, icon_color, auth, tools, version, created_at, updated_at
FROM apps;

DROP TABLE apps;
ALTER TABLE apps_next RENAME TO apps;
CREATE INDEX apps_by_name ON apps (organization_id, lower(name), id);

-- Workspace apps, converted the same way. Workspaces could hold the same
-- slug before, and the catalog the same as a workspace: the oldest workspace
-- app keeps a key no catalog app of its organization has, and the others get
-- the `app_` form too.
CREATE TABLE workspace_apps_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('mcp')),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 60),
    key TEXT NOT NULL CHECK (
        length(key) BETWEEN 1 AND 24
        AND key GLOB '[a-z]*'
        AND key NOT GLOB '*[^a-z0-9_]*'
        AND key NOT GLOB '*__*'
        AND key NOT GLOB '*_'
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
    -- Unique in the organization, across its workspaces.
    UNIQUE (organization_id, key)
);

INSERT INTO workspace_apps_next
    (id, organization_id, workspace_id, kind, name, key, description, icon_glyph, icon_color, auth, tools,
     version, created_at, updated_at)
SELECT w.id, w.organization_id, w.workspace_id, w.kind, w.name,
       CASE
           WHEN length(w.converted) <= 24
                AND w.converted GLOB '[a-z]*'
                AND w.converted NOT GLOB '*__*'
                AND w.oldest = 1
                AND NOT EXISTS (
                    SELECT 1 FROM apps a WHERE a.organization_id = w.organization_id AND a.key = w.converted
                )
           THEN w.converted
           ELSE 'app_' || substr(lower(hex(w.id)), 13, 20)
       END,
       w.description, w.icon_glyph, w.icon_color, w.auth, w.tools, w.version, w.created_at, w.updated_at
FROM (
    SELECT *, replace(slug, '-', '_') AS converted,
           row_number() OVER (
               PARTITION BY organization_id, replace(slug, '-', '_') ORDER BY created_at, id
           ) AS oldest
    FROM workspace_apps
) w;

DROP TABLE workspace_apps;
ALTER TABLE workspace_apps_next RENAME TO workspace_apps;
CREATE INDEX workspace_apps_by_name ON workspace_apps (organization_id, workspace_id, lower(name), id);

-- Installations, each with a short ID and its record. The nth installation
-- of a workspace (from 0, oldest first) is spelled from n times an odd
-- constant modulo 2^30, five bits a character, lowest first: distinct for
-- every n below 2^30, so none collide.
CREATE TABLE installations_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    catalog_app_id UUID,
    workspace_app_id UUID,
    short_id TEXT NOT NULL,
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 40),
    status TEXT NOT NULL CHECK (status IN ('connected', 'reconnect_required', 'disconnected')),
    configuration_version INTEGER NOT NULL CHECK (configuration_version >= 0),
    account_external_id TEXT CHECK (length(account_external_id) BETWEEN 1 AND 300),
    account_display_name TEXT CHECK (length(account_display_name) BETWEEN 1 AND 300),
    account_email TEXT CHECK (length(account_email) BETWEEN 3 AND 320),
    granted_scopes TEXT NOT NULL CHECK (json_valid(granted_scopes) AND json_type(granted_scopes) = 'array'),
    credential_version INTEGER CHECK (credential_version >= 1),
    connected_at INTEGER,
    expires_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    auth_template TEXT
        CHECK (auth_template IS NULL OR (json_valid(auth_template) AND json_type(auth_template) = 'object')),
    app_version INTEGER
        CHECK ((app_version IS NULL) = (auth_template IS NULL) AND (app_version IS NULL OR app_version >= 0)),
    oauth_client TEXT CHECK (
        oauth_client IS NULL
        OR (json_valid(oauth_client) AND json_type(oauth_client) = 'object' AND auth_template IS NOT NULL)
    ),
    refresh_started_at INTEGER,
    CHECK ((catalog_app_id IS NULL) <> (workspace_app_id IS NULL)),
    CHECK (
        (account_external_id IS NULL) = (account_display_name IS NULL)
        AND (account_external_id IS NULL) = (credential_version IS NULL)
        AND (account_external_id IS NULL) = (connected_at IS NULL)
        AND (account_external_id IS NOT NULL OR (account_email IS NULL AND expires_at IS NULL))
    ),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    -- Catalog apps aren't deleted; were one to be, its installations must be dealt with first.
    FOREIGN KEY (organization_id, catalog_app_id) REFERENCES apps (organization_id, id),
    FOREIGN KEY (organization_id, workspace_id, workspace_app_id)
        REFERENCES workspace_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    -- Its short ID was issued in its own workspace, and is recorded there.
    FOREIGN KEY (organization_id, workspace_id, short_id)
        REFERENCES installation_short_ids (organization_id, workspace_id, short_id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, short_id)
);

INSERT INTO installations_next
    (id, organization_id, workspace_id, catalog_app_id, workspace_app_id, short_id, label, status,
     configuration_version, account_external_id, account_display_name, account_email, granted_scopes,
     credential_version, connected_at, expires_at, created_at, updated_at, auth_template, app_version,
     oauth_client, refresh_started_at)
SELECT id, organization_id, workspace_id, catalog_app_id, workspace_app_id,
       substr('abcdefghijklmnopqrstuvwxyz234567', 1 + (spelled & 31), 1)
           || substr('abcdefghijklmnopqrstuvwxyz234567', 1 + ((spelled >> 5) & 31), 1)
           || substr('abcdefghijklmnopqrstuvwxyz234567', 1 + ((spelled >> 10) & 31), 1)
           || substr('abcdefghijklmnopqrstuvwxyz234567', 1 + ((spelled >> 15) & 31), 1)
           || substr('abcdefghijklmnopqrstuvwxyz234567', 1 + ((spelled >> 20) & 31), 1)
           || substr('abcdefghijklmnopqrstuvwxyz234567', 1 + ((spelled >> 25) & 31), 1),
       label, status, configuration_version, account_external_id, account_display_name, account_email,
       granted_scopes, credential_version, connected_at, expires_at, created_at, updated_at, auth_template,
       app_version, oauth_client, refresh_started_at
FROM (
    SELECT *,
           ((row_number() OVER (PARTITION BY organization_id, workspace_id ORDER BY created_at, id) - 1)
               * 625341585) & 1073741823 AS spelled
    FROM installations
);

INSERT INTO installation_short_ids (id, organization_id, workspace_id, short_id, created_at)
SELECT randomblob(16), organization_id, workspace_id, short_id, created_at FROM installations_next;

DROP TABLE installations;
ALTER TABLE installations_next RENAME TO installations;
CREATE INDEX installations_by_workspace ON installations (organization_id, workspace_id, created_at);
CREATE INDEX installations_by_catalog_app ON installations (organization_id, catalog_app_id, created_at, id);

-- Every reference still holds, or nothing commits: a row here fails its check.
CREATE TEMP TABLE migration_0027_check (violations INTEGER NOT NULL CHECK (violations = 0));
INSERT INTO migration_0027_check SELECT count(*) FROM pragma_foreign_key_check;
DROP TABLE migration_0027_check;

COMMIT;

PRAGMA foreign_keys = ON;
