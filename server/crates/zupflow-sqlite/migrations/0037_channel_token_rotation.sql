-- A linked team's configuration token is rotated by one job per credential
-- version, an hour before it expires: the version a rotation job was queued
-- for, so the sweep that queues them queues each once, and a refresh token
-- (which Slack takes once) is never spent by two jobs.
ALTER TABLE channel_app_managers ADD COLUMN rotation_version INTEGER
    CHECK (rotation_version IS NULL OR rotation_version >= 1);

-- When a deleted app was removed at its provider (or had nothing there to
-- remove). While it's NULL on a deleted app, its team can't be unlinked:
-- the team's tokens are what removes it.
ALTER TABLE channel_apps ADD COLUMN released_at INTEGER;
UPDATE channel_apps SET released_at = updated_at WHERE status = 'deleted';

-- The digest of the manifest a sync last sent the provider, so a sync of an
-- unchanged agent sends nothing.
ALTER TABLE channel_apps ADD COLUMN manifest_digest TEXT
    CHECK (manifest_digest IS NULL OR (length(manifest_digest) = 64 AND manifest_digest NOT GLOB '*[^0-9a-f]*'));

-- When the provider was last asked about a person: someone it didn't know
-- is asked about again a day after, and so is the email of an author a
-- sender list is checked against. A name, once known, is kept.
ALTER TABLE channel_people ADD COLUMN looked_up_at INTEGER;
