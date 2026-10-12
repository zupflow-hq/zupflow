-- The run engine before decision 0046 is gone, and what only it used goes
-- with it: `runs.turns` (the durable engine keeps its turns in `run_turns`)
-- and `run_tool_calls.approval_requested` (a call waits for approval when
-- the run's continuation says so). Neither column is named by an index, a
-- reference or another column's check, so SQLite drops each in place; no
-- table is rebuilt, and no reference to one is touched.
--
-- A run that engine made keeps `execution_version` 0 and no continuation,
-- and no worker takes it forward any more. One that hadn't finished fails
-- here as `run_unavailable`, as that engine's sweep would have failed it,
-- so none is left queued, running or waiting forever, and its calls are
-- closed as a failed run's are: one recorded as sent and never settled is
-- `unknown`, never sent again; one not sent is refused.
--
-- Nothing is deployed: such runs exist only in development databases. So
-- this writes no `run.failed` event and queues no webhook for them: an
-- event's ID is a UUIDv7 the server issues, and its announcement a job the
-- store queues, neither of which SQL here can make as the server does. The
-- run's status, error and finish time say what happened.

UPDATE run_tool_calls
SET state = 'unknown',
    outcome = 'The call was sent, but no answer came back, so whether it acted is unknown. It wasn''t repeated.',
    outcome_is_error = 1,
    failure_code = 'outcome_unknown',
    failure_message = 'The call was sent, but its outcome is unknown. It wasn''t repeated.',
    finished_at = CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)
WHERE state = 'started'
  AND EXISTS (
    SELECT 1 FROM runs r
    WHERE r.organization_id = run_tool_calls.organization_id
      AND r.workspace_id = run_tool_calls.workspace_id
      AND r.id = run_tool_calls.run_id
      AND r.execution_version = 0
      AND r.status IN ('queued', 'running', 'waiting_for_approval')
  );

UPDATE run_tool_calls
SET state = 'refused',
    outcome = 'The run ended before this call was made.',
    outcome_is_error = 1,
    failure_code = 'run_failed',
    failure_message = 'The run ended before this call was made.',
    finished_at = CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)
WHERE state IN ('allowed', 'awaiting_approval', 'approved')
  AND EXISTS (
    SELECT 1 FROM runs r
    WHERE r.organization_id = run_tool_calls.organization_id
      AND r.workspace_id = run_tool_calls.workspace_id
      AND r.id = run_tool_calls.run_id
      AND r.execution_version = 0
      AND r.status IN ('queued', 'running', 'waiting_for_approval')
  );

UPDATE runs
SET status = 'failed',
    error_code = 'run_unavailable',
    error_message = 'The run couldn''t continue. Start it again.',
    finished_at = CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)
WHERE execution_version = 0 AND status IN ('queued', 'running', 'waiting_for_approval');

ALTER TABLE runs DROP COLUMN turns;
ALTER TABLE run_tool_calls DROP COLUMN approval_requested;
