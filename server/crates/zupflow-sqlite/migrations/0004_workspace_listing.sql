-- Listing an organization's workspaces by name (case-insensitively), then
-- environment and ID: the order the list's cursor follows.
CREATE INDEX workspaces_by_name ON workspaces (organization_id, lower(name), environment, id);
