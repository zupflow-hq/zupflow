-- The durable run engine's storage (decision 0046), beside the run engine
-- that serves runs today, in the same tables, so a transition, its events
-- and its jobs commit together and the API reads both kinds of run alike.
--
-- A run the durable engine admits has an `execution_version` of 1 or more
-- and a `continuation`, its exact next step as versioned JSON. The run
-- engine before it writes neither: its runs keep version 0 and no
-- continuation, and the durable engine never advances them. Every
-- transition raises `revision`. A worker's claim raises `owner_epoch`, names
-- its `owner_claimant` and holds until `lease_until`; a cancel sets
-- `cancel_requested_at`. Instants are the store's own clock.
--
-- `runs.turns` and `run_tool_calls.approval_requested` are the older
-- engine's: the durable engine keeps its turns in `run_turns`, and writes
-- `[]` and whether the run paused for a call there. They go with that
-- engine.

ALTER TABLE runs ADD COLUMN execution_version INTEGER NOT NULL DEFAULT 0 CHECK (execution_version >= 0);
ALTER TABLE runs ADD COLUMN revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0);
ALTER TABLE runs ADD COLUMN owner_epoch INTEGER NOT NULL DEFAULT 0 CHECK (owner_epoch >= 0);
ALTER TABLE runs ADD COLUMN owner_claimant UUID
    CHECK (owner_claimant IS NULL OR (typeof(owner_claimant) = 'blob' AND length(owner_claimant) = 16));
ALTER TABLE runs ADD COLUMN lease_until INTEGER;
ALTER TABLE runs ADD COLUMN cancel_requested_at INTEGER CHECK (cancel_requested_at IS NULL OR status = 'cancelled');
ALTER TABLE runs ADD COLUMN continuation TEXT CHECK (
    (execution_version = 0) = (continuation IS NULL)
    AND (continuation IS NULL OR (json_valid(continuation) AND json_type(continuation) = 'object'))
    AND (execution_version > 0 OR (revision = 0 AND owner_epoch = 0 AND owner_claimant IS NULL AND lease_until IS NULL))
    AND (owner_epoch = 0) = (owner_claimant IS NULL)
    AND (lease_until IS NULL OR status IN ('queued', 'running'))
);

-- What the sweep reads, across tenants: the durable engine's moving runs.
CREATE INDEX runs_durable_moving ON runs (execution_version, status) WHERE status IN ('queued', 'running');

-- A call the durable engine records names the installation's short ID, the
-- app's key and the SHA-256 of the pinned definition it was decided against
-- with its target (decision 0047), and its digest: what an approval binds
-- and a send checks again. An approval keeps the digest it was bound to. A
-- call that failed or was refused keeps the code and message its timeline
-- showed.
ALTER TABLE run_tool_calls ADD COLUMN installation_short_id TEXT
    CHECK (installation_short_id IS NULL OR (length(installation_short_id) = 6 AND installation_short_id NOT GLOB '*[^a-z2-7]*'));
ALTER TABLE run_tool_calls ADD COLUMN app_key TEXT CHECK (app_key IS NULL OR length(app_key) BETWEEN 1 AND 24);
ALTER TABLE run_tool_calls ADD COLUMN definition_sha256 TEXT
    CHECK (definition_sha256 IS NULL OR (length(definition_sha256) = 71 AND definition_sha256 GLOB 'sha256:*'));
ALTER TABLE run_tool_calls ADD COLUMN call_digest TEXT
    CHECK (call_digest IS NULL OR (length(call_digest) = 71 AND call_digest GLOB 'sha256:*'));
ALTER TABLE run_tool_calls ADD COLUMN approval_call_digest TEXT
    CHECK (approval_call_digest IS NULL OR (approval IS NOT NULL AND approval_call_digest = call_digest));
ALTER TABLE run_tool_calls ADD COLUMN failure_code TEXT
    CHECK (failure_code IS NULL OR length(failure_code) BETWEEN 1 AND 100);
ALTER TABLE run_tool_calls ADD COLUMN failure_message TEXT CHECK (
    (failure_code IS NULL) = (failure_message IS NULL)
    AND (installation_short_id IS NULL) = (app_key IS NULL)
    AND (app_key IS NULL) = (definition_sha256 IS NULL)
    AND (installation_short_id IS NULL OR installation_id IS NOT NULL)
    AND (call_digest IS NULL OR installation_short_id IS NOT NULL)
);

-- What a call's attempts reference: a call, with its run and both scopes.
CREATE UNIQUE INDEX run_tool_calls_in_run ON run_tool_calls (organization_id, workspace_id, run_id, id);

-- One row per committed transition, unique per run by its ID and by the
-- revision it started from: a retry of the same request finds its receipt
-- here, and a second request from the same revision can't commit. The
-- receipt is the row and the events it appended, `event_count` of them from
-- `first_sequence`.
CREATE TABLE run_transitions (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    transition_id BLOB NOT NULL CHECK (typeof(transition_id) = 'blob' AND length(transition_id) = 16),
    from_revision INTEGER NOT NULL CHECK (from_revision >= 0),
    to_revision INTEGER NOT NULL CHECK (to_revision = from_revision + 1),
    operation TEXT NOT NULL CHECK (operation IN (
        'admit', 'claim', 'prepare_model', 'commit_turn', 'park', 'resolve_approval', 'authorize_call',
        'record_outcome', 'settle_unknown', 'cancel', 'yield', 'finish', 'fail_abandoned'
    )),
    request_digest TEXT NOT NULL CHECK (length(request_digest) = 71 AND request_digest GLOB 'sha256:*'),
    status TEXT NOT NULL
        CHECK (status IN ('queued', 'running', 'waiting_for_approval', 'completed', 'failed', 'cancelled')),
    continuation TEXT NOT NULL CHECK (json_valid(continuation) AND json_type(continuation) = 'object'),
    lease_until INTEGER,
    first_sequence INTEGER NOT NULL CHECK (first_sequence >= 1),
    event_count INTEGER NOT NULL CHECK (event_count >= 0),
    committed_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id)
        REFERENCES runs (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, run_id, transition_id),
    UNIQUE (organization_id, workspace_id, run_id, from_revision)
);

-- One row per committed model answer: its text and the tokens it took.
CREATE TABLE run_turns (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    turn INTEGER NOT NULL CHECK (turn >= 0),
    model_attempt INTEGER NOT NULL CHECK (model_attempt >= 1),
    text TEXT,
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    created_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id)
        REFERENCES runs (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, run_id, turn)
);

-- Each request to the model, committed before it goes out, with the tools
-- it offered frozen: prepared, then answered, abandoned (its worker stopped
-- first: tokens may have been spent and not counted) or unanswered.
CREATE TABLE run_model_attempts (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    turn INTEGER NOT NULL CHECK (turn >= 0),
    attempt INTEGER NOT NULL CHECK (attempt >= 1),
    state TEXT NOT NULL CHECK (state IN ('prepared', 'answered', 'abandoned', 'unanswered')),
    request_digest TEXT NOT NULL CHECK (length(request_digest) = 71 AND request_digest GLOB 'sha256:*'),
    offer TEXT NOT NULL CHECK (json_valid(offer) AND json_type(offer) = 'array'),
    input_tokens INTEGER CHECK (input_tokens IS NULL OR input_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens IS NULL OR output_tokens >= 0),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK ((state = 'answered') = (input_tokens IS NOT NULL AND output_tokens IS NOT NULL)),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id)
        REFERENCES runs (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, run_id, turn, attempt)
);

-- At most one model attempt of a turn is prepared at a time.
CREATE UNIQUE INDEX run_model_attempts_one_prepared ON run_model_attempts (organization_id, workspace_id, run_id, turn)
    WHERE state = 'prepared';

-- Each authorization to send a call: its send claim, with the owner epoch
-- that authorized it, its own invocation ID and lease, and what its sender
-- reported (`outcome`, the sanitized outcome as JSON). A report that came
-- after the call was settled unknown is kept as late evidence, for
-- operators only.
CREATE TABLE run_call_attempts (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    call_id UUID NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt >= 1),
    dispatch_epoch INTEGER NOT NULL CHECK (dispatch_epoch >= 1),
    invocation_id UUID NOT NULL CHECK (typeof(invocation_id) = 'blob' AND length(invocation_id) = 16),
    authorized_at INTEGER NOT NULL,
    send_lease_until INTEGER NOT NULL,
    outcome TEXT CHECK (outcome IS NULL OR (json_valid(outcome) AND json_type(outcome) = 'object')),
    outcome_recorded_at INTEGER,
    late INTEGER NOT NULL CHECK (late IN (0, 1)),
    CHECK ((outcome IS NULL) = (outcome_recorded_at IS NULL)),
    CHECK (late = 0 OR outcome IS NOT NULL),
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id)
        REFERENCES runs (organization_id, workspace_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, run_id, call_id)
        REFERENCES run_tool_calls (organization_id, workspace_id, run_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, call_id, attempt),
    UNIQUE (organization_id, workspace_id, invocation_id)
);

-- At most one send per call whose outcome isn't recorded.
CREATE UNIQUE INDEX run_call_attempts_one_live ON run_call_attempts (organization_id, workspace_id, call_id)
    WHERE outcome IS NULL;
