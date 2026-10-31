-- no-transaction
-- Memory in runs: what a run was started with, what it recalled, its calls
-- to memory tools, and what each of those calls changed.
--
-- - `runs.memory`: `agent` (the agent's memory as it's set) or `off` (no
--   recall, and every call to a memory tool refused). A run started before
--   this is `agent`: one that hadn't asked its model yet resolves its
--   agent's memory at its first attempt, as any run does, and one under way
--   keeps the prompt it started with, without memory.
-- - `runs.memory_type_id`, `runs.recall` and `runs.recall_digest`: the
--   memory type a run's first model attempt resolved and the recall it was
--   sent, kept on the run once, so each later attempt sends the same bytes
--   and is held to them by the digest. A recall is never empty, and never
--   without its type or its digest. A finished run never sends again: its
--   recall goes with its last transition, and its digest stays.
-- - `run_tool_calls` gains `memory_type_id`: a call targets an
--   installation, a channel connection or a memory type, at most one. A
--   memory tool's call names no short ID; its `app_key` is the type's kind,
--   up to 32 characters, and it has a call digest as the others do. The
--   calls waiting for approval in a type are found across its
--   organization's workspaces, for the job its deletion queues.
-- - `memory_calls`: what each run's memory call changed, written with the
--   change in its own transaction, one row per call and never a second, so
--   a call whose answer was lost is answered as it was made, and a sender
--   that commits after its call was settled changes nothing. Its answer
--   names a memory by its short ID, never by its text. The rows stay while
--   the run's workspace does, through erasures of the memories they name:
--   a settle after an erasure never makes the change again.
--
-- Changing checks means rebuilding `run_tool_calls`, which
-- `run_call_attempts` references. So, as SQLite documents for this, foreign
-- keys are off while it's rebuilt in one transaction, checked before it
-- commits, and on again after.

PRAGMA foreign_keys = OFF;

BEGIN IMMEDIATE;

-- Runs ---------------------------------------------------------------------

ALTER TABLE runs ADD COLUMN memory TEXT NOT NULL DEFAULT 'agent' CHECK (memory IN ('agent', 'off'));

ALTER TABLE runs ADD COLUMN memory_type_id UUID
    CHECK (memory_type_id IS NULL OR (typeof(memory_type_id) = 'blob' AND length(memory_type_id) = 16));

ALTER TABLE runs ADD COLUMN recall_digest TEXT
    CHECK (recall_digest IS NULL OR (
        memory_type_id IS NOT NULL AND length(recall_digest) = 71 AND recall_digest GLOB 'sha256:*'
    ));

ALTER TABLE runs ADD COLUMN recall TEXT
    CHECK (recall IS NULL OR (memory_type_id IS NOT NULL AND recall_digest IS NOT NULL AND length(recall) >= 1));

-- What each run's memory call changed ------------------------------------------

-- `id` is the run's tool call: one row per call.
CREATE TABLE memory_calls (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL CHECK (typeof(run_id) = 'blob' AND length(run_id) = 16),
    agent_id UUID NOT NULL CHECK (typeof(agent_id) = 'blob' AND length(agent_id) = 16),
    memory_type_id UUID NOT NULL CHECK (typeof(memory_type_id) = 'blob' AND length(memory_type_id) = 16),
    tool TEXT NOT NULL CHECK (length(tool) BETWEEN 1 AND 64),
    memory_id UUID NOT NULL CHECK (typeof(memory_id) = 'blob' AND length(memory_id) = 16),
    answer TEXT NOT NULL CHECK (length(answer) BETWEEN 1 AND 1000),
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX memory_calls_of_run ON memory_calls (organization_id, workspace_id, run_id, id);

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
    app_key TEXT CHECK (app_key IS NULL OR length(app_key) BETWEEN 1 AND 32),
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
    -- A memory tool's call: the run's memory type, whose kind is its
    -- `app_key` and whose name is its label.
    memory_type_id UUID
        CHECK (memory_type_id IS NULL OR (typeof(memory_type_id) = 'blob' AND length(memory_type_id) = 16)),
    CHECK (
        (failure_code IS NULL) = (failure_message IS NULL)
        AND (app_key IS NULL) = (definition_sha256 IS NULL)
        AND (installation_short_id IS NULL OR installation_id IS NOT NULL OR channel_connection_id IS NOT NULL)
        AND (memory_type_id IS NOT NULL OR (installation_short_id IS NULL) = (app_key IS NULL))
        AND (memory_type_id IS NULL OR (installation_short_id IS NULL AND app_key IS NOT NULL))
        AND (memory_type_id IS NOT NULL OR app_key IS NULL OR length(app_key) <= 24)
        AND (call_digest IS NULL OR installation_short_id IS NOT NULL OR memory_type_id IS NOT NULL)
    ),
    -- A call targets one installation, one channel connection or one memory
    -- type, at most.
    CHECK ((installation_id IS NOT NULL) + (channel_connection_id IS NOT NULL) + (memory_type_id IS NOT NULL) <= 1),
    CHECK (
        (installation_id IS NULL AND channel_connection_id IS NULL AND memory_type_id IS NULL) = (tool IS NULL)
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
    definition_sha256, call_digest, approval_call_digest, failure_code, failure_message, channel_connection_id)
SELECT id, organization_id, workspace_id, run_id, turn, position, model_call_id, name, installation_id,
       installation_label, app_name, tool, arguments, state, verdict, policy, mode, rule, reason, approval,
       approval_api_key_id, outcome, outcome_is_error, started_at, finished_at, installation_short_id, app_key,
       definition_sha256, call_digest, approval_call_digest, failure_code, failure_message, channel_connection_id
FROM run_tool_calls;

DROP TABLE run_tool_calls;
ALTER TABLE run_tool_calls_next RENAME TO run_tool_calls;

CREATE UNIQUE INDEX run_tool_calls_in_run ON run_tool_calls (organization_id, workspace_id, run_id, id);
CREATE INDEX run_tool_calls_awaiting_approval
    ON run_tool_calls (organization_id, workspace_id, installation_id)
    WHERE state = 'awaiting_approval';
-- The calls a memory type's deletion refuses, in any of its organization's
-- workspaces.
CREATE INDEX run_tool_calls_awaiting_approval_in_type
    ON run_tool_calls (organization_id, memory_type_id)
    WHERE state = 'awaiting_approval' AND memory_type_id IS NOT NULL;

-- Every reference still holds, or nothing commits: a row here fails its check.
CREATE TEMP TABLE migration_0040_check (violations INTEGER NOT NULL CHECK (violations = 0));
INSERT INTO migration_0040_check SELECT count(*) FROM pragma_foreign_key_check;
DROP TABLE migration_0040_check;

COMMIT;

PRAGMA foreign_keys = ON;
