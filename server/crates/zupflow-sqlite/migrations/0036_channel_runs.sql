-- no-transaction
-- Channel runs: runs a thread's messages start, continuing their session.
--
-- - `runs` gains the `channel` trigger and, for a channel run, where it came
--   from (its session, connection, conversation, thread, the message that
--   woke it and why), its place in the session (`session_sequence`), the
--   earliest run whose turns its prompt includes (`history_start_run_id`,
--   itself when none), and the tools fixed for its session (`offer`). A run
--   names its session and its messages without referencing them: it
--   outlives both, as it outlives its agent. Instructions may carry the
--   channel preface beside the agent's own. Once a channel run ends, its
--   `transcript` (what it leaves in the session's conversation, as sent)
--   and that transcript's weight in tokens are kept, for the runs after it.
-- - `run_events` takes `channel_messages.received` and `run.stayed_silent`.
-- - `run_steered_turns`: thread messages that joined a run at a turn
--   boundary, one user turn each, before the turn they name. Written once,
--   resent as written.
-- - `run_tool_calls` gains `channel_connection_id`: a call targets an
--   installation or a channel connection, never both.
-- - `channel_sessions` holds what's fixed for the session (model,
--   instructions with the preface, tools and the history budget), the last
--   thread message its conversation took in, and how many runs it started.
-- - `channel_dispatches` takes `steered`, and `read` for a message the
--   session already read in an earlier turn.
-- - `channel_messages` records when the inbound job handled a message.
-- - `channel_connections` keeps the identity's app ID where the provider
--   has one (Slack's `bot_id`), and `channel_people` the team a person
--   belongs to (Slack Connect brings in other teams' people).
--
-- Changing checks means rebuilding tables, and dropping a parent table with
-- foreign keys on would cascade into its children. So, as SQLite documents
-- for this, foreign keys are off while they're rebuilt in one transaction,
-- checked before it commits, and on again after.

PRAGMA foreign_keys = OFF;

BEGIN IMMEDIATE;

-- Runs ---------------------------------------------------------------------

CREATE TABLE runs_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    agent_id UUID NOT NULL CHECK (typeof(agent_id) = 'blob' AND length(agent_id) = 16),
    session_id UUID NOT NULL CHECK (typeof(session_id) = 'blob' AND length(session_id) = 16),
    trigger TEXT NOT NULL CHECK (trigger IN ('api', 'console', 'channel')),
    job_id UUID NOT NULL CHECK (typeof(job_id) = 'blob' AND length(job_id) = 16),
    status TEXT NOT NULL
        CHECK (status IN ('queued', 'running', 'waiting_for_approval', 'completed', 'failed', 'cancelled')),
    input TEXT NOT NULL CHECK (length(input) BETWEEN 1 AND 8000),
    output TEXT,
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 100),
    -- The agent's instructions, after the channel preface for a channel run.
    instructions TEXT NOT NULL CHECK (length(instructions) <= 24000),
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    tool_calls INTEGER NOT NULL CHECK (tool_calls >= 0),
    error_code TEXT CHECK (error_code IS NULL OR length(error_code) BETWEEN 1 AND 100),
    error_message TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    execution_version INTEGER NOT NULL DEFAULT 0 CHECK (execution_version >= 0),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    owner_epoch INTEGER NOT NULL DEFAULT 0 CHECK (owner_epoch >= 0),
    owner_claimant UUID
        CHECK (owner_claimant IS NULL OR (typeof(owner_claimant) = 'blob' AND length(owner_claimant) = 16)),
    lease_until INTEGER,
    cancel_requested_at INTEGER CHECK (cancel_requested_at IS NULL OR status = 'cancelled'),
    continuation TEXT CHECK (
        (execution_version = 0) = (continuation IS NULL)
        AND (continuation IS NULL OR (json_valid(continuation) AND json_type(continuation) = 'object'))
        AND (execution_version > 0 OR (revision = 0 AND owner_epoch = 0 AND owner_claimant IS NULL AND lease_until IS NULL))
        AND (owner_epoch = 0) = (owner_claimant IS NULL)
        AND (lease_until IS NULL OR status IN ('queued', 'running'))
    ),
    model_id UUID CHECK (model_id IS NULL OR (typeof(model_id) = 'blob' AND length(model_id) = 16)),
    model_connection_id UUID
        CHECK (model_connection_id IS NULL OR (typeof(model_connection_id) = 'blob' AND length(model_connection_id) = 16)),
    model_key_version INTEGER CHECK (model_key_version IS NULL OR model_key_version >= 1),
    cache_read_input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_read_input_tokens >= 0),
    cache_write_input_tokens INTEGER NOT NULL DEFAULT 0
        CHECK (cache_write_input_tokens >= 0 AND cache_read_input_tokens + cache_write_input_tokens <= input_tokens),
    -- A channel run's: named without references, as its history.
    channel_session_id UUID
        CHECK (channel_session_id IS NULL OR (typeof(channel_session_id) = 'blob' AND length(channel_session_id) = 16)),
    channel_connection_id UUID
        CHECK (channel_connection_id IS NULL OR (typeof(channel_connection_id) = 'blob' AND length(channel_connection_id) = 16)),
    channel_conversation_id UUID
        CHECK (channel_conversation_id IS NULL OR (typeof(channel_conversation_id) = 'blob' AND length(channel_conversation_id) = 16)),
    channel_thread_key TEXT CHECK (channel_thread_key IS NULL OR length(channel_thread_key) <= 100),
    channel_reason TEXT
        CHECK (channel_reason IS NULL OR channel_reason IN ('direct_message', 'mentioned', 'thread_follow_up')),
    origin_message_id UUID
        CHECK (origin_message_id IS NULL OR (typeof(origin_message_id) = 'blob' AND length(origin_message_id) = 16)),
    silent_reason TEXT CHECK (silent_reason IS NULL OR length(silent_reason) BETWEEN 1 AND 500),
    session_sequence INTEGER CHECK (session_sequence IS NULL OR session_sequence >= 1),
    history_start_run_id UUID
        CHECK (history_start_run_id IS NULL OR (typeof(history_start_run_id) = 'blob' AND length(history_start_run_id) = 16)),
    offer TEXT CHECK (offer IS NULL OR (json_valid(offer) AND json_type(offer) = 'array')),
    -- An ended channel run's, as sent: what the runs after it continue.
    transcript TEXT CHECK (transcript IS NULL OR (json_valid(transcript) AND json_type(transcript) = 'array')),
    transcript_weight INTEGER CHECK (transcript_weight IS NULL OR transcript_weight >= 0),
    CHECK ((transcript IS NULL) = (transcript_weight IS NULL)),
    CHECK (transcript IS NULL OR (channel_session_id IS NOT NULL AND finished_at IS NOT NULL)),
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK ((status IN ('completed', 'failed', 'cancelled')) = (finished_at IS NOT NULL)),
    CHECK ((status = 'failed') = (error_code IS NOT NULL)),
    -- A channel run has all of where it came from; any other run none of it.
    CHECK (
        (trigger = 'channel') = (channel_session_id IS NOT NULL)
        AND (channel_session_id IS NULL) = (channel_connection_id IS NULL)
        AND (channel_session_id IS NULL) = (channel_conversation_id IS NULL)
        AND (channel_session_id IS NULL) = (channel_thread_key IS NULL)
        AND (channel_session_id IS NULL) = (channel_reason IS NULL)
        AND (channel_session_id IS NULL) = (origin_message_id IS NULL)
        AND (channel_session_id IS NULL) = (session_sequence IS NULL)
        AND (channel_session_id IS NULL) = (history_start_run_id IS NULL)
        AND (channel_session_id IS NULL) = (offer IS NULL)
        AND (silent_reason IS NULL OR channel_session_id IS NOT NULL)
    ),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);

INSERT INTO runs_next (id, organization_id, workspace_id, agent_id, session_id, trigger, job_id, status, input, output,
    model, instructions, input_tokens, output_tokens, tool_calls, error_code, error_message, created_at, started_at,
    finished_at, execution_version, revision, owner_epoch, owner_claimant, lease_until, cancel_requested_at,
    continuation, model_id, model_connection_id, model_key_version, cache_read_input_tokens, cache_write_input_tokens)
SELECT id, organization_id, workspace_id, agent_id, session_id, trigger, job_id, status, input, output, model,
       instructions, input_tokens, output_tokens, tool_calls, error_code, error_message, created_at, started_at,
       finished_at, execution_version, revision, owner_epoch, owner_claimant, lease_until, cancel_requested_at,
       continuation, model_id, model_connection_id, model_key_version, cache_read_input_tokens,
       cache_write_input_tokens
FROM runs;

DROP TABLE runs;
ALTER TABLE runs_next RENAME TO runs;

CREATE INDEX runs_by_workspace ON runs (organization_id, workspace_id, created_at, id);
CREATE INDEX runs_by_organization ON runs (organization_id, created_at, id);
CREATE INDEX runs_by_agent ON runs (organization_id, workspace_id, agent_id, created_at);
CREATE INDEX runs_moving ON runs (status) WHERE status IN ('queued', 'running');
CREATE INDEX runs_durable_moving ON runs (execution_version, status) WHERE status IN ('queued', 'running');
-- A session's runs, in order: the history a run continues.
CREATE UNIQUE INDEX runs_by_session
    ON runs (organization_id, workspace_id, channel_session_id, session_sequence)
    WHERE channel_session_id IS NOT NULL;

-- Run events ---------------------------------------------------------------

CREATE TABLE run_events_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence >= 1),
    type TEXT NOT NULL CHECK (type IN (
        'run.queued', 'run.started', 'message.assistant', 'channel_messages.received', 'tool_call.started',
        'tool_call.completed', 'tool_call.failed', 'approval.requested', 'approval.resolved', 'run.stayed_silent',
        'run.completed', 'run.failed', 'run.cancelled'
    )),
    data TEXT NOT NULL CHECK (json_valid(data) AND json_type(data) = 'object'),
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id)
        REFERENCES runs (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, run_id, sequence)
);

INSERT INTO run_events_next (id, organization_id, workspace_id, run_id, sequence, type, data, created_at)
SELECT id, organization_id, workspace_id, run_id, sequence, type, data, created_at FROM run_events;

DROP TABLE run_events;
ALTER TABLE run_events_next RENAME TO run_events;

-- Steered turns ------------------------------------------------------------

CREATE TABLE run_steered_turns (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    -- The turn it comes before: taken in when that turn was first prepared.
    turn INTEGER NOT NULL CHECK (turn >= 0),
    text TEXT NOT NULL CHECK (length(text) BETWEEN 1 AND 8000),
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id)
        REFERENCES runs (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, run_id, turn)
);

-- Tool calls ---------------------------------------------------------------

CREATE TABLE run_tool_calls_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    turn INTEGER NOT NULL CHECK (turn >= 0),
    position INTEGER NOT NULL CHECK (position >= 0),
    model_call_id TEXT NOT NULL CHECK (length(model_call_id) BETWEEN 1 AND 256),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 256),
    installation_id UUID
        CHECK (installation_id IS NULL OR (typeof(installation_id) = 'blob' AND length(installation_id) = 16)),
    installation_label TEXT,
    app_name TEXT,
    tool TEXT,
    arguments TEXT NOT NULL CHECK (json_valid(arguments) AND json_type(arguments) = 'object'),
    state TEXT NOT NULL CHECK (state IN (
        'allowed', 'awaiting_approval', 'approved', 'denied', 'refused', 'started', 'completed', 'failed', 'unknown'
    )),
    verdict TEXT CHECK (verdict IS NULL OR verdict IN ('allow', 'require_approval', 'deny')),
    policy TEXT,
    mode TEXT,
    rule TEXT,
    reason TEXT,
    approval TEXT CHECK (approval IS NULL OR approval IN ('approved', 'denied')),
    approval_api_key_id UUID,
    outcome TEXT,
    outcome_is_error INTEGER CHECK (outcome_is_error IS NULL OR outcome_is_error IN (0, 1)),
    started_at INTEGER,
    finished_at INTEGER,
    installation_short_id TEXT
        CHECK (installation_short_id IS NULL OR (length(installation_short_id) = 6 AND installation_short_id NOT GLOB '*[^a-z2-7]*')),
    app_key TEXT CHECK (app_key IS NULL OR length(app_key) BETWEEN 1 AND 24),
    definition_sha256 TEXT
        CHECK (definition_sha256 IS NULL OR (length(definition_sha256) = 71 AND definition_sha256 GLOB 'sha256:*')),
    call_digest TEXT CHECK (call_digest IS NULL OR (length(call_digest) = 71 AND call_digest GLOB 'sha256:*')),
    approval_call_digest TEXT
        CHECK (approval_call_digest IS NULL OR (approval IS NOT NULL AND approval_call_digest = call_digest)),
    failure_code TEXT CHECK (failure_code IS NULL OR length(failure_code) BETWEEN 1 AND 100),
    failure_message TEXT,
    -- A channel tool's call: its connection, whose short ID and provider
    -- key name it as an installation's name its app's tool.
    channel_connection_id UUID
        CHECK (channel_connection_id IS NULL OR (typeof(channel_connection_id) = 'blob' AND length(channel_connection_id) = 16)),
    CHECK (
        (failure_code IS NULL) = (failure_message IS NULL)
        AND (installation_short_id IS NULL) = (app_key IS NULL)
        AND (app_key IS NULL) = (definition_sha256 IS NULL)
        AND (installation_short_id IS NULL OR installation_id IS NOT NULL OR channel_connection_id IS NOT NULL)
        AND (call_digest IS NULL OR installation_short_id IS NOT NULL)
    ),
    -- A call targets one installation or one channel connection, never both.
    CHECK (installation_id IS NULL OR channel_connection_id IS NULL),
    CHECK (
        (installation_id IS NULL AND channel_connection_id IS NULL) = (tool IS NULL)
        AND (tool IS NULL) = (installation_label IS NULL)
        AND (tool IS NULL) = (app_name IS NULL)
    ),
    CHECK ((verdict IS NULL) = (policy IS NULL) AND (policy IS NULL) = (rule IS NULL)),
    CHECK ((approval IS NULL) = (approval_api_key_id IS NULL)),
    CHECK ((outcome IS NULL) = (outcome_is_error IS NULL)),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id)
        REFERENCES runs (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, run_id, turn, position),
    UNIQUE (organization_id, workspace_id, run_id, model_call_id)
);

INSERT INTO run_tool_calls_next (id, organization_id, workspace_id, run_id, turn, position, model_call_id, name,
    installation_id, installation_label, app_name, tool, arguments, state, verdict, policy, mode, rule, reason,
    approval, approval_api_key_id, outcome, outcome_is_error, started_at, finished_at, installation_short_id, app_key,
    definition_sha256, call_digest, approval_call_digest, failure_code, failure_message)
SELECT id, organization_id, workspace_id, run_id, turn, position, model_call_id, name, installation_id,
       installation_label, app_name, tool, arguments, state, verdict, policy, mode, rule, reason, approval,
       approval_api_key_id, outcome, outcome_is_error, started_at, finished_at, installation_short_id, app_key,
       definition_sha256, call_digest, approval_call_digest, failure_code, failure_message
FROM run_tool_calls;

DROP TABLE run_tool_calls;
ALTER TABLE run_tool_calls_next RENAME TO run_tool_calls;

CREATE UNIQUE INDEX run_tool_calls_in_run ON run_tool_calls (organization_id, workspace_id, run_id, id);
CREATE INDEX run_tool_calls_awaiting_approval
    ON run_tool_calls (organization_id, workspace_id, installation_id)
    WHERE state = 'awaiting_approval';

-- Sessions -----------------------------------------------------------------

CREATE TABLE channel_sessions_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    agent_channel_id UUID NOT NULL,
    conversation_id UUID NOT NULL,
    thread_key TEXT NOT NULL CHECK (length(thread_key) <= 100),
    -- The runs' session_id; a new one when what's fixed for the session changes.
    session_id UUID NOT NULL CHECK (typeof(session_id) = 'blob' AND length(session_id) = 16),
    -- Set while a run is queued or running: one at a time.
    active_run_id UUID,
    last_run_id UUID,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- Fixed for the session once its first run starts: the model, the
    -- instructions with the channel preface, the tools, and the history
    -- budget in tokens.
    model TEXT CHECK (model IS NULL OR length(model) BETWEEN 1 AND 100),
    model_id UUID CHECK (model_id IS NULL OR (typeof(model_id) = 'blob' AND length(model_id) = 16)),
    model_connection_id UUID
        CHECK (model_connection_id IS NULL OR (typeof(model_connection_id) = 'blob' AND length(model_connection_id) = 16)),
    instructions TEXT CHECK (instructions IS NULL OR length(instructions) <= 24000),
    offer TEXT CHECK (offer IS NULL OR (json_valid(offer) AND json_type(offer) = 'array')),
    history_budget INTEGER CHECK (history_budget IS NULL OR history_budget >= 1),
    -- The last thread message the session's conversation took in.
    seen_message_id UUID
        CHECK (seen_message_id IS NULL OR (typeof(seen_message_id) = 'blob' AND length(seen_message_id) = 16)),
    seen_at INTEGER,
    -- The runs it started, across its session IDs: the next one's sequence.
    run_count INTEGER NOT NULL DEFAULT 0 CHECK (run_count >= 0),
    CHECK (
        (model IS NULL) = (model_id IS NULL)
        AND (model IS NULL) = (model_connection_id IS NULL)
        AND (model IS NULL) = (instructions IS NULL)
        AND (model IS NULL) = (offer IS NULL)
        AND (model IS NULL) = (history_budget IS NULL)
        AND (seen_message_id IS NULL) = (seen_at IS NULL)
        AND (active_run_id IS NULL OR model IS NOT NULL)
    ),
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

-- Phase 3 started no runs: no session has an active one to keep.
INSERT INTO channel_sessions_next (id, organization_id, workspace_id, agent_channel_id, conversation_id, thread_key,
    session_id, active_run_id, last_run_id, created_at, updated_at)
SELECT id, organization_id, workspace_id, agent_channel_id, conversation_id, thread_key, session_id, NULL,
       last_run_id, created_at, updated_at
FROM channel_sessions;

DROP TABLE channel_sessions;
ALTER TABLE channel_sessions_next RENAME TO channel_sessions;

-- Dispatches ---------------------------------------------------------------

CREATE TABLE channel_dispatches_next (
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
    -- `read`: the session read it in an earlier turn, which answered it.
    outcome TEXT NOT NULL CHECK (outcome IN ('started', 'waiting', 'steered', 'collected', 'paused', 'read')),
    -- The run that took it in.
    run_id UUID,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK ((outcome IN ('started', 'steered', 'collected')) = (run_id IS NOT NULL)),
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

INSERT INTO channel_dispatches_next (id, organization_id, workspace_id, message_id, agent_channel_id,
    channel_session_id, origin_key, thread_origin_key, agent_hop, handler, reason, outcome, run_id, created_at,
    updated_at)
SELECT id, organization_id, workspace_id, message_id, agent_channel_id, channel_session_id, origin_key,
       thread_origin_key, agent_hop, handler, reason, outcome, run_id, created_at, updated_at
FROM channel_dispatches;

DROP TABLE channel_dispatches;
ALTER TABLE channel_dispatches_next RENAME TO channel_dispatches;

CREATE INDEX channel_dispatches_waiting
    ON channel_dispatches (organization_id, workspace_id, channel_session_id, created_at)
    WHERE outcome = 'waiting';
CREATE INDEX channel_dispatches_by_thread
    ON channel_dispatches (organization_id, workspace_id, thread_origin_key, created_at);
CREATE INDEX channel_dispatches_by_message
    ON channel_dispatches (organization_id, workspace_id, agent_channel_id, message_id);

-- Messages -----------------------------------------------------------------

-- Inbound: when the inbound job decided who answers it.
ALTER TABLE channel_messages ADD COLUMN handled_at INTEGER;

-- Connections and people -------------------------------------------------------

-- The identity's app ID, where the provider has one beside its user (Slack's
-- `bot_id`): its own posts carry one or the other.
ALTER TABLE channel_connections ADD COLUMN identity_bot_id TEXT
    CHECK (identity_bot_id IS NULL OR length(identity_bot_id) BETWEEN 1 AND 100);
-- The team the person belongs to, when the provider says.
ALTER TABLE channel_people ADD COLUMN team_id TEXT CHECK (team_id IS NULL OR length(team_id) BETWEEN 1 AND 100);

-- Every reference still holds, or nothing commits: a row here fails its check.
CREATE TEMP TABLE migration_0036_check (violations INTEGER NOT NULL CHECK (violations = 0));
INSERT INTO migration_0036_check SELECT count(*) FROM pragma_foreign_key_check;
DROP TABLE migration_0036_check;

COMMIT;

PRAGMA foreign_keys = ON;
