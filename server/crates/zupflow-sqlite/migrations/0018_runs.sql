-- Runs (decision 0040; the backend standard's "Runs"): an agent working on
-- one input, its timeline and the tool calls it asked for. All three are
-- workspace-scoped and go with the workspace.
--
-- A run outlives its agent and the installations its calls reached: it names
-- them by ID, with no reference, and copies what its timeline shows (the
-- installation's label, the app's name) when a call is decided.
--
-- A run keeps the agent's model and instructions as it started, and its
-- turns (what the model answered, with its calls' IDs) as a JSON array, so
-- the worker can rebuild the conversation after a restart.
--
-- `job_id` names the last job queued to take the run forward (jobs are
-- internal and deleted when done, so it's no reference): a queued or running
-- run whose job is gone or dead is abandoned, and the worker's sweep fails it.

CREATE TABLE runs (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    agent_id UUID NOT NULL CHECK (typeof(agent_id) = 'blob' AND length(agent_id) = 16),
    session_id UUID NOT NULL CHECK (typeof(session_id) = 'blob' AND length(session_id) = 16),
    trigger TEXT NOT NULL CHECK (trigger IN ('api', 'console')),
    job_id UUID NOT NULL CHECK (typeof(job_id) = 'blob' AND length(job_id) = 16),
    status TEXT NOT NULL
        CHECK (status IN ('queued', 'running', 'waiting_for_approval', 'completed', 'failed', 'cancelled')),
    input TEXT NOT NULL CHECK (length(input) BETWEEN 1 AND 8000),
    output TEXT,
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 100),
    instructions TEXT NOT NULL CHECK (length(instructions) <= 20000),
    turns TEXT NOT NULL CHECK (json_valid(turns) AND json_type(turns) = 'array'),
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    tool_calls INTEGER NOT NULL CHECK (tool_calls >= 0),
    error_code TEXT CHECK (error_code IS NULL OR length(error_code) BETWEEN 1 AND 100),
    error_message TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK ((status IN ('completed', 'failed', 'cancelled')) = (finished_at IS NOT NULL)),
    CHECK ((status = 'failed') = (error_code IS NOT NULL)),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);

CREATE INDEX runs_by_workspace ON runs (organization_id, workspace_id, created_at, id);
CREATE INDEX runs_by_organization ON runs (organization_id, created_at, id);
CREATE INDEX runs_by_agent ON runs (organization_id, workspace_id, agent_id, created_at);
-- What the sweep reads, across tenants.
CREATE INDEX runs_moving ON runs (status) WHERE status IN ('queued', 'running');

CREATE TABLE run_events (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence >= 1),
    type TEXT NOT NULL CHECK (type IN (
        'run.queued', 'run.started', 'message.assistant', 'tool_call.started', 'tool_call.completed',
        'tool_call.failed', 'approval.requested', 'approval.resolved', 'run.completed', 'run.failed',
        'run.cancelled'
    )),
    data TEXT NOT NULL CHECK (json_valid(data) AND json_type(data) = 'object'),
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id)
        REFERENCES runs (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, run_id, sequence)
);

-- A call's target is NULL when the model named a tool it wasn't given. A
-- decision is recorded with the policy, the mode and the rule that made it.
CREATE TABLE run_tool_calls (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    turn INTEGER NOT NULL CHECK (turn >= 0),
    position INTEGER NOT NULL CHECK (position >= 0),
    model_call_id TEXT NOT NULL CHECK (length(model_call_id) BETWEEN 1 AND 256),
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 256),
    installation_id UUID CHECK (installation_id IS NULL OR (typeof(installation_id) = 'blob' AND length(installation_id) = 16)),
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
    approval_requested INTEGER NOT NULL CHECK (approval_requested IN (0, 1)),
    approval TEXT CHECK (approval IS NULL OR approval IN ('approved', 'denied')),
    approval_api_key_id UUID,
    outcome TEXT,
    outcome_is_error INTEGER CHECK (outcome_is_error IS NULL OR outcome_is_error IN (0, 1)),
    started_at INTEGER,
    finished_at INTEGER,
    CHECK (
        (installation_id IS NULL) = (tool IS NULL)
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
