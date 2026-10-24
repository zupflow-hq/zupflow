-- no-transaction
-- Channels: a workspace's agents reached in a team's chat (Slack first).
--
-- Everything is workspace-scoped except the organization's channel tool
-- levels and settings. Every table starts with its scope, every reference
-- carries it, and every secret is sealed (`keys::seal`) one value per
-- version under its kind's context:
--
-- - `channel_app_managers`: how Zupflow makes apps for a workspace's agents
--   in one team; its configuration token and refresh token are sealed
--   together in `channel_app_manager_credentials` (the
--   `channel_app_manager_credential` context). A rotation stores the next
--   version only while the stored one is still the version it read.
-- - `channel_apps`: one agent's generated app in one team, with its own
--   inbound reference (where the provider sends its events: the one lookup
--   that starts without a tenant) and, once made, its client and signing
--   secrets sealed in `channel_app_secrets` (`channel_app_secret`). One live
--   app per agent per manager. Making one is at least once: an attempt
--   marks the app before it asks the provider, and records the provider's
--   ID as soon as it answers, so a later attempt deletes what an earlier
--   one made before making another. An app names its agent and its manager
--   without referencing them: a deleted app is kept as history (its
--   connection's history stays readable), so it outlives both, like a run
--   outlives its agent. The store checks both, in the workspace, in the
--   transaction that makes the app, and refuses to unlink a team while it
--   has apps that aren't deleted.
-- - `channel_inbound_events`: deliveries to an app, once per provider event ID.
-- - `channel_connections`: an installed app: one team, one identity, one
--   agent; its token in `channel_connection_credentials`
--   (`channel_connection_credential`). One live connection per app. Its
--   short ID comes from the same pool as installations'.
-- - `channel_conversations`, `channel_people`, `channel_messages`: what a
--   connection has seen, gone with it.
-- - `agent_channels`: an agent's settings on one of its own app's
--   connections, one per connection.
-- - `channel_sessions`: an agent's session in one thread, with at most one
--   active run; `channel_dispatches`: what the handler decided for each
--   agent and what became of it.
-- - `channel_tool_levels`, `channel_settings`: the organization's.
--
-- A Connect session now either installs an app or sets up a channel: it
-- names an app (`catalog_app_id` or `workspace_app_id`) or a target
-- (`target_kind`), never both. A `channel_app` target references its app;
-- the manager or connection a completed session made is named without a
-- reference, as the session's history, since unlinking a team or removing a
-- connection keeps the session.
--
-- Changing `connect_sessions`' checks means rebuilding it, and dropping a
-- parent table with foreign keys on would cascade into its children. So, as
-- SQLite documents for this, foreign keys are off while it's rebuilt in one
-- transaction, checked before it commits, and on again after.

PRAGMA foreign_keys = OFF;

BEGIN IMMEDIATE;

-- Organization scope -----------------------------------------------------

-- A level someone set; a tool without a row has Zupflow's default.
CREATE TABLE channel_tool_levels (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    provider TEXT NOT NULL CHECK (provider IN ('slack')),
    tool TEXT NOT NULL CHECK (length(tool) BETWEEN 1 AND 64),
    level TEXT NOT NULL CHECK (level IN ('allow', 'approve', 'deny')),
    updated_at INTEGER NOT NULL,
    updated_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL,
    UNIQUE (organization_id, provider, tool)
);

-- Saved settings; an organization without a row has the defaults.
CREATE TABLE channel_settings (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL UNIQUE REFERENCES organizations (id) ON DELETE CASCADE,
    breaker_runs INTEGER NOT NULL CHECK (breaker_runs BETWEEN 1 AND 200),
    breaker_window_minutes INTEGER NOT NULL CHECK (breaker_window_minutes BETWEEN 1 AND 1440),
    breaker_pause_minutes INTEGER NOT NULL CHECK (breaker_pause_minutes BETWEEN 1 AND 1440),
    message_retention_days INTEGER NOT NULL CHECK (message_retention_days BETWEEN 1 AND 365),
    updated_at INTEGER NOT NULL,
    updated_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL
);

-- Workspace scope --------------------------------------------------------

CREATE TABLE channel_app_managers (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('slack')),
    method TEXT NOT NULL CHECK (method IN ('configuration_token')),
    -- Learned from the first install; every later install must match it.
    team_id TEXT CHECK (length(team_id) BETWEEN 1 AND 100),
    team_name TEXT CHECK (length(team_name) BETWEEN 1 AND 300),
    status TEXT NOT NULL CHECK (status IN ('active', 'token_expired', 'disconnected')),
    token_last4 TEXT NOT NULL CHECK (length(token_last4) BETWEEN 1 AND 4),
    token_updated_at INTEGER NOT NULL,
    credential_version INTEGER NOT NULL CHECK (credential_version >= 1),
    token_expires_at INTEGER,
    created_by_user_id UUID REFERENCES users (id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);
CREATE INDEX channel_app_managers_by_workspace ON channel_app_managers (organization_id, workspace_id, created_at, id);

CREATE TABLE channel_app_manager_credentials (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    manager_id UUID NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    key_id TEXT NOT NULL,
    wrapped_key BLOB NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce) = 12),
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, manager_id)
        REFERENCES channel_app_managers (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, manager_id, version)
);

CREATE TABLE channel_apps (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    -- Named, not referenced: a deleted app is history that outlives both.
    manager_id UUID NOT NULL CHECK (typeof(manager_id) = 'blob' AND length(manager_id) = 16),
    agent_id UUID NOT NULL CHECK (typeof(agent_id) = 'blob' AND length(agent_id) = 16),
    provider TEXT NOT NULL CHECK (provider IN ('slack')),
    external_app_id TEXT CHECK (length(external_app_id) BETWEEN 1 AND 100),
    client_id TEXT CHECK (length(client_id) BETWEEN 1 AND 100),
    -- 32 random base32 characters: this app's events go to /channels/inbound/{inbound_ref}.
    inbound_ref TEXT NOT NULL UNIQUE CHECK (length(inbound_ref) = 32 AND inbound_ref NOT GLOB '*[^a-z2-7]*'),
    status TEXT NOT NULL CHECK (status IN
        ('creating', 'awaiting_install', 'installed', 'updating', 'deleting', 'deleted', 'failed')),
    -- A stable code, never the provider's text.
    last_error TEXT CHECK (length(last_error) BETWEEN 1 AND 100),
    -- The job making it, named without a reference (a finished job's row
    -- goes): an app whose job is gone or buried while it's still being made
    -- is failed, and one still in flight gets a job to clean up after it.
    provision_job_id UUID NOT NULL CHECK (typeof(provision_job_id) = 'blob' AND length(provision_job_id) = 16),
    -- Set before an attempt asks the provider to make it, and cleared once
    -- what that made is recorded as made or deleted at the provider: while
    -- it's set, the provider may hold an app no row records.
    creation_started_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (status IN ('creating', 'failed', 'deleted') OR (external_app_id IS NOT NULL AND client_id IS NOT NULL)),
    CHECK (creation_started_at IS NULL OR status IN ('creating', 'failed', 'deleted')),
    CHECK ((status = 'failed') = (last_error IS NOT NULL)),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);
-- One live app per agent per manager.
CREATE UNIQUE INDEX channel_apps_one_live_per_agent
    ON channel_apps (organization_id, workspace_id, manager_id, agent_id)
    WHERE status NOT IN ('deleted', 'failed');
CREATE INDEX channel_apps_by_workspace ON channel_apps (organization_id, workspace_id, created_at, id);
-- What the sweep for apps whose making job is gone reads.
CREATE INDEX channel_apps_unsettled ON channel_apps (provision_job_id)
    WHERE status = 'creating' OR creation_started_at IS NOT NULL;

CREATE TABLE channel_app_secrets (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    app_id UUID NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    key_id TEXT NOT NULL,
    wrapped_key BLOB NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce) = 12),
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, app_id)
        REFERENCES channel_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, app_id, version)
);

CREATE TABLE channel_inbound_events (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    app_id UUID NOT NULL,
    provider_event_id TEXT NOT NULL CHECK (length(provider_event_id) BETWEEN 1 AND 200),
    kind TEXT NOT NULL CHECK (length(kind) BETWEEN 1 AND 100),
    received_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, app_id)
        REFERENCES channel_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, app_id, provider_event_id)
);

CREATE TABLE channel_connections (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('slack')),
    app_id UUID,
    -- Drawn from installation_short_ids, so a channel tool never shares a name with an app tool.
    short_id TEXT NOT NULL CHECK (length(short_id) = 6 AND short_id NOT GLOB '*[^a-z2-7]*'),
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 40),
    handler TEXT NOT NULL CHECK (handler IN ('mention')),
    status TEXT NOT NULL CHECK (status IN ('connected', 'reconnect_required', 'disconnected')),
    team_id TEXT NOT NULL CHECK (length(team_id) BETWEEN 1 AND 100),
    team_name TEXT CHECK (length(team_name) BETWEEN 1 AND 300),
    enterprise_id TEXT CHECK (length(enterprise_id) BETWEEN 1 AND 100),
    identity_external_id TEXT NOT NULL CHECK (length(identity_external_id) BETWEEN 1 AND 100),
    handle TEXT NOT NULL CHECK (length(handle) BETWEEN 1 AND 100),
    granted_scopes TEXT NOT NULL CHECK (json_valid(granted_scopes) AND json_type(granted_scopes) = 'array'),
    credential_version INTEGER CHECK (credential_version >= 1),
    connected_at INTEGER,
    last_event_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (provider <> 'slack' OR app_id IS NOT NULL),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, app_id) REFERENCES channel_apps (organization_id, workspace_id, id),
    FOREIGN KEY (organization_id, workspace_id, short_id)
        REFERENCES installation_short_ids (organization_id, workspace_id, short_id),
    UNIQUE (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, short_id)
);
-- An app is installed once at a time; a reinstall updates its live row.
CREATE UNIQUE INDEX channel_connections_one_live_per_app
    ON channel_connections (organization_id, workspace_id, app_id)
    WHERE status <> 'disconnected' AND app_id IS NOT NULL;
CREATE INDEX channel_connections_by_workspace ON channel_connections (organization_id, workspace_id, created_at, id);

CREATE TABLE channel_connection_credentials (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    connection_id UUID NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    key_id TEXT NOT NULL,
    wrapped_key BLOB NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce) = 12),
    ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, connection_id)
        REFERENCES channel_connections (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, connection_id, version)
);

CREATE TABLE channel_conversations (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    connection_id UUID NOT NULL,
    external_id TEXT NOT NULL CHECK (length(external_id) BETWEEN 1 AND 100),
    kind TEXT NOT NULL CHECK (kind IN ('channel', 'private_channel', 'dm', 'group_dm')),
    name TEXT CHECK (length(name) BETWEEN 1 AND 300),
    is_external INTEGER NOT NULL CHECK (is_external IN (0, 1)),
    identity_is_member INTEGER NOT NULL CHECK (identity_is_member IN (0, 1)),
    last_message_at INTEGER,
    -- The conversation breaker: runs counted since the window started, and the pause it set.
    breaker_window_start INTEGER,
    breaker_count INTEGER NOT NULL DEFAULT 0 CHECK (breaker_count >= 0),
    paused_until INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, connection_id)
        REFERENCES channel_connections (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, connection_id, external_id)
);
CREATE INDEX channel_conversations_by_recency
    ON channel_conversations (organization_id, workspace_id, connection_id, last_message_at, id);

CREATE TABLE channel_people (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    connection_id UUID NOT NULL,
    external_id TEXT NOT NULL CHECK (length(external_id) BETWEEN 1 AND 100),
    handle TEXT CHECK (length(handle) BETWEEN 1 AND 100),
    display_name TEXT CHECK (length(display_name) BETWEEN 1 AND 300),
    email TEXT CHECK (length(email) BETWEEN 3 AND 320),
    is_automated INTEGER NOT NULL CHECK (is_automated IN (0, 1)),
    first_seen_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, connection_id)
        REFERENCES channel_connections (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, connection_id, external_id)
);

CREATE TABLE channel_messages (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    connection_id UUID NOT NULL,
    conversation_id UUID NOT NULL,
    external_id TEXT NOT NULL CHECK (length(external_id) BETWEEN 1 AND 100),
    -- The thread's root external ID; '' for a DM's top level.
    thread_key TEXT NOT NULL CHECK (length(thread_key) <= 100),
    direction TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
    author_person_id UUID,
    -- Rendered once, when it arrived, and never rewritten.
    text TEXT NOT NULL CHECK (length(CAST(text AS BLOB)) <= 160000),
    mentions TEXT NOT NULL CHECK (json_valid(mentions) AND json_type(mentions) = 'array'),
    -- Outbound: the run and call that sent it, named without a reference.
    run_id UUID CHECK (run_id IS NULL OR (typeof(run_id) = 'blob' AND length(run_id) = 16)),
    tool_call_id UUID CHECK (tool_call_id IS NULL OR (typeof(tool_call_id) = 'blob' AND length(tool_call_id) = 16)),
    agent_hop INTEGER NOT NULL DEFAULT 0 CHECK (agent_hop >= 0),
    sent_at INTEGER NOT NULL,
    edited_at INTEGER,
    deleted_at INTEGER,
    created_at INTEGER NOT NULL,
    -- Kept for the organization's retention period.
    expires_at INTEGER NOT NULL,
    CHECK ((direction = 'outbound') = (author_person_id IS NULL)),
    CHECK (direction = 'outbound' OR (run_id IS NULL AND tool_call_id IS NULL)),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, connection_id)
        REFERENCES channel_connections (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, conversation_id)
        REFERENCES channel_conversations (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, author_person_id)
        REFERENCES channel_people (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, conversation_id, external_id)
);
CREATE INDEX channel_messages_by_conversation
    ON channel_messages (organization_id, workspace_id, conversation_id, sent_at, id);
CREATE INDEX channel_messages_by_thread
    ON channel_messages (organization_id, workspace_id, conversation_id, thread_key, sent_at);
CREATE INDEX channel_messages_expiry ON channel_messages (organization_id, workspace_id, expires_at);

CREATE TABLE agent_channels (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    agent_id UUID NOT NULL,
    connection_id UUID NOT NULL,
    tools TEXT NOT NULL CHECK (json_valid(tools) AND json_type(tools) = 'array'),
    approval_required TEXT NOT NULL CHECK (json_valid(approval_required) AND json_type(approval_required) = 'array'),
    audience TEXT NOT NULL CHECK (audience IN ('origin', 'listed', 'anywhere')),
    audience_conversations TEXT NOT NULL
        CHECK (json_valid(audience_conversations) AND json_type(audience_conversations) = 'array'),
    persona TEXT CHECK (persona IS NULL OR (json_valid(persona) AND json_type(persona) = 'object')),
    answers_dms INTEGER NOT NULL CHECK (answers_dms IN (0, 1)),
    answers_mentions INTEGER NOT NULL CHECK (answers_mentions IN (0, 1)),
    thread_follow_up TEXT NOT NULL CHECK (thread_follow_up IN ('off', 'when_last_addressed', 'always')),
    agent_messages TEXT NOT NULL CHECK (agent_messages IN ('ignore', 'mentions')),
    max_agent_hops INTEGER NOT NULL CHECK (max_agent_hops BETWEEN 1 AND 5),
    mid_run TEXT NOT NULL CHECK (mid_run IN ('steer', 'collect')),
    senders TEXT NOT NULL CHECK (senders IN ('anyone', 'listed')),
    sender_list TEXT NOT NULL CHECK (json_valid(sender_list) AND json_type(sender_list) = 'array'),
    allow_automated INTEGER NOT NULL CHECK (allow_automated IN (0, 1)),
    allow_external INTEGER NOT NULL CHECK (allow_external IN (0, 1)),
    reply_with_output INTEGER NOT NULL CHECK (reply_with_output IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, agent_id)
        REFERENCES agents (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, connection_id)
        REFERENCES channel_connections (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);
-- A connection is one agent's app: one agent channel on it, so one identity never wakes two agents.
CREATE UNIQUE INDEX agent_channels_one_per_connection ON agent_channels (organization_id, workspace_id, connection_id);
CREATE INDEX agent_channels_by_agent ON agent_channels (organization_id, workspace_id, agent_id, created_at, id);

CREATE TABLE channel_sessions (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    agent_channel_id UUID NOT NULL,
    conversation_id UUID NOT NULL,
    thread_key TEXT NOT NULL CHECK (length(thread_key) <= 100),
    -- The runs' session_id.
    session_id UUID NOT NULL CHECK (typeof(session_id) = 'blob' AND length(session_id) = 16),
    -- Set while a run is queued or running: one at a time.
    active_run_id UUID,
    last_run_id UUID,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, agent_channel_id)
        REFERENCES agent_channels (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, conversation_id)
        REFERENCES channel_conversations (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, active_run_id) REFERENCES runs (organization_id, workspace_id, id),
    FOREIGN KEY (organization_id, workspace_id, last_run_id) REFERENCES runs (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, agent_channel_id, conversation_id, thread_key)
);

CREATE TABLE channel_dispatches (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    message_id UUID NOT NULL,
    agent_channel_id UUID NOT NULL,
    -- NULL when stopped before a session (the breaker).
    channel_session_id UUID,
    -- The provider's key for the message across connections: one agent is never dispatched twice for it.
    origin_key TEXT NOT NULL CHECK (length(origin_key) BETWEEN 1 AND 300),
    thread_origin_key TEXT NOT NULL CHECK (length(thread_origin_key) BETWEEN 1 AND 300),
    agent_hop INTEGER NOT NULL CHECK (agent_hop >= 0),
    handler TEXT NOT NULL CHECK (handler IN ('mention')),
    reason TEXT NOT NULL CHECK (reason IN ('direct_message', 'mentioned', 'thread_follow_up')),
    outcome TEXT NOT NULL CHECK (outcome IN ('started', 'waiting', 'collected', 'paused')),
    run_id UUID,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK ((outcome IN ('started', 'collected')) = (run_id IS NOT NULL)),
    CHECK (outcome = 'paused' OR channel_session_id IS NOT NULL),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, message_id)
        REFERENCES channel_messages (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, agent_channel_id)
        REFERENCES agent_channels (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, channel_session_id)
        REFERENCES channel_sessions (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id) REFERENCES runs (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, id),
    UNIQUE (organization_id, workspace_id, agent_channel_id, origin_key)
);
CREATE INDEX channel_dispatches_waiting
    ON channel_dispatches (organization_id, workspace_id, channel_session_id, created_at)
    WHERE outcome = 'waiting';
CREATE INDEX channel_dispatches_by_thread
    ON channel_dispatches (organization_id, workspace_id, thread_origin_key, created_at);

-- Connect sessions: an app's, or a channel's ------------------------------

CREATE TABLE connect_sessions_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    environment TEXT NOT NULL CHECK (environment IN ('sandbox', 'production')),
    catalog_app_id UUID,
    workspace_app_id UUID,
    configuration_version INTEGER NOT NULL CHECK (configuration_version >= 0),
    intent TEXT NOT NULL CHECK (intent IN ('create', 'reconnect')),
    installation_id UUID,
    label TEXT NOT NULL CHECK (length(label) BETWEEN 1 AND 40),
    status TEXT NOT NULL CHECK (status IN ('pending', 'completed', 'expired', 'cancelled', 'failed')),
    return_url TEXT CHECK (length(return_url) BETWEEN 1 AND 2000),
    requested_scopes TEXT NOT NULL CHECK (json_valid(requested_scopes) AND json_type(requested_scopes) = 'array'),
    reference_hash BLOB NOT NULL UNIQUE CHECK (typeof(reference_hash) = 'blob' AND length(reference_hash) = 32),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    completed_at INTEGER,
    failure_code TEXT CHECK (length(failure_code) BETWEEN 1 AND 60),
    authorization_attempt INTEGER NOT NULL DEFAULT 0 CHECK (authorization_attempt >= 0),
    -- A channel session's target, and what it made once completed.
    target_kind TEXT CHECK (target_kind IN ('channel_app_manager', 'channel_app')),
    target_provider TEXT CHECK (target_provider IN ('slack')),
    target_app_id UUID,
    target_manager_id UUID CHECK (target_manager_id IS NULL
        OR (typeof(target_manager_id) = 'blob' AND length(target_manager_id) = 16)),
    target_connection_id UUID CHECK (target_connection_id IS NULL
        OR (typeof(target_connection_id) = 'blob' AND length(target_connection_id) = 16)),
    -- An app's session names exactly one app; a channel's names none, and its target.
    CHECK (
        (target_kind IS NULL AND (catalog_app_id IS NULL) <> (workspace_app_id IS NULL))
        OR (target_kind IS NOT NULL AND catalog_app_id IS NULL AND workspace_app_id IS NULL
            AND target_provider IS NOT NULL AND intent = 'create' AND installation_id IS NULL)
    ),
    CHECK (target_kind IS NOT NULL OR (target_provider IS NULL AND target_app_id IS NULL
        AND target_manager_id IS NULL AND target_connection_id IS NULL)),
    CHECK (target_kind <> 'channel_app_manager' OR (target_app_id IS NULL AND target_connection_id IS NULL)),
    CHECK (target_kind <> 'channel_app' OR (target_app_id IS NOT NULL AND target_manager_id IS NULL)),
    CHECK ((target_manager_id IS NULL AND target_connection_id IS NULL) OR status = 'completed'),
    -- A reconnect always names its installation; a create names one only once it has made it.
    CHECK (intent = 'create' OR installation_id IS NOT NULL),
    CHECK (intent = 'reconnect' OR installation_id IS NULL OR status = 'completed'),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, catalog_app_id) REFERENCES apps (organization_id, id),
    FOREIGN KEY (organization_id, workspace_id, workspace_app_id)
        REFERENCES workspace_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, installation_id)
        REFERENCES installations (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, target_app_id)
        REFERENCES channel_apps (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);

INSERT INTO connect_sessions_next (id, organization_id, workspace_id, environment, catalog_app_id, workspace_app_id,
    configuration_version, intent, installation_id, label, status, return_url, requested_scopes, reference_hash,
    created_at, expires_at, completed_at, failure_code, authorization_attempt)
SELECT id, organization_id, workspace_id, environment, catalog_app_id, workspace_app_id, configuration_version,
       intent, installation_id, label, status, return_url, requested_scopes, reference_hash, created_at,
       expires_at, completed_at, failure_code, authorization_attempt
FROM connect_sessions;

DROP TABLE connect_sessions;
ALTER TABLE connect_sessions_next RENAME TO connect_sessions;

CREATE INDEX connect_sessions_by_workspace ON connect_sessions (organization_id, workspace_id, created_at);
CREATE INDEX connect_sessions_pending_expiry ON connect_sessions (expires_at, id) WHERE status = 'pending';

-- Every reference still holds, or nothing commits: a row here fails its check.
CREATE TEMP TABLE migration_0035_check (violations INTEGER NOT NULL CHECK (violations = 0));
INSERT INTO migration_0035_check SELECT count(*) FROM pragma_foreign_key_check;
DROP TABLE migration_0035_check;

COMMIT;

PRAGMA foreign_keys = ON;
