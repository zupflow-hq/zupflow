-- A call waiting for approval on an installation that is deleted or
-- disconnected is refused, and its run moves on (decision 0047): the job the
-- installation's change queues commits a `refuse_waiting` transition.
--
-- `run_transitions` names the operations it may hold in a check, so it is
-- rebuilt with that one added, its rows copied as they are. No table
-- references it, so dropping the old one cascades into nothing, and foreign
-- keys stay on.
--
-- The job finds a workspace's calls waiting for approval on one
-- installation: a partial index keeps that read to those calls.

CREATE TABLE run_transitions_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    run_id UUID NOT NULL,
    transition_id BLOB NOT NULL CHECK (typeof(transition_id) = 'blob' AND length(transition_id) = 16),
    from_revision INTEGER NOT NULL CHECK (from_revision >= 0),
    to_revision INTEGER NOT NULL CHECK (to_revision = from_revision + 1),
    operation TEXT NOT NULL CHECK (operation IN (
        'admit', 'claim', 'prepare_model', 'commit_turn', 'park', 'resolve_approval', 'refuse_waiting',
        'authorize_call', 'record_outcome', 'settle_unknown', 'cancel', 'yield', 'finish', 'fail_abandoned'
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

INSERT INTO run_transitions_next (
    id, organization_id, workspace_id, run_id, transition_id, from_revision, to_revision, operation,
    request_digest, status, continuation, lease_until, first_sequence, event_count, committed_at
)
SELECT
    id, organization_id, workspace_id, run_id, transition_id, from_revision, to_revision, operation,
    request_digest, status, continuation, lease_until, first_sequence, event_count, committed_at
FROM run_transitions;

DROP TABLE run_transitions;
ALTER TABLE run_transitions_next RENAME TO run_transitions;

CREATE INDEX run_tool_calls_awaiting_approval
    ON run_tool_calls (organization_id, workspace_id, installation_id)
    WHERE state = 'awaiting_approval';
