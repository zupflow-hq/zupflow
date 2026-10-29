-- The channel providers an organization's workspaces may link teams of.
--
-- - `channel_provider_settings`: the organization's, saved by an admin. An
--   organization without a row allows every provider. `allowed_providers`
--   NULL also allows every provider, including ones a later release adds;
--   a JSON array of provider names allows only those, so a provider added
--   later isn't allowed until an admin adds it, and an empty array allows
--   none. It decides only what can be linked: teams already linked keep
--   working.

CREATE TABLE channel_provider_settings (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL UNIQUE REFERENCES organizations (id) ON DELETE CASCADE,
    allowed_providers TEXT
        CHECK (allowed_providers IS NULL
            OR (json_valid(allowed_providers) AND json_type(allowed_providers) = 'array')),
    updated_at INTEGER NOT NULL,
    updated_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL
);
