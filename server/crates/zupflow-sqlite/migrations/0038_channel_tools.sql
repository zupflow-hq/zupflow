-- Phase 4B: channel tools, edits and deletes, and retention.
--
-- - `channel_messages` keeps a message's edited text beside what it said
--   when it arrived, which is never rewritten, and when Zupflow learned of
--   its last edit or deletion (the provider's own times may be earlier
--   than a turn that hasn't noted them).
-- - `channel_sessions` records when its turns last noted the thread's edits
--   and deletes, so each change is noted once, in the next turn.
-- - `runs` records when the retention sweep removed a channel run's text,
--   so each run is cleared once.
-- - An index counts a connection's posts for its rate limits, and another
--   finds what a run's call posted.

ALTER TABLE channel_messages ADD COLUMN edited_text TEXT
    CHECK (edited_text IS NULL OR length(CAST(edited_text AS BLOB)) <= 160000);

ALTER TABLE channel_messages ADD COLUMN changed_at INTEGER;

ALTER TABLE channel_sessions ADD COLUMN changes_noted_at INTEGER;

ALTER TABLE runs ADD COLUMN channel_text_removed_at INTEGER
    CHECK (channel_text_removed_at IS NULL OR channel_session_id IS NOT NULL);

CREATE INDEX channel_messages_posts
    ON channel_messages (organization_id, workspace_id, connection_id, direction, sent_at);

CREATE INDEX channel_messages_by_call
    ON channel_messages (organization_id, workspace_id, run_id, tool_call_id)
    WHERE tool_call_id IS NOT NULL;

-- Each change a connection makes at its provider (a post, an edit, a
-- reaction, a deletion) first takes a send slot, inside an immediate
-- transaction: at most one a second and 200 an hour per connection, however
-- many runs send at once. Swept after an hour.
CREATE TABLE channel_send_slots (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    connection_id UUID NOT NULL,
    run_id UUID NOT NULL CHECK (typeof(run_id) = 'blob' AND length(run_id) = 16),
    tool_call_id UUID NOT NULL CHECK (typeof(tool_call_id) = 'blob' AND length(tool_call_id) = 16),
    taken_at INTEGER NOT NULL,
    FOREIGN KEY (organization_id, workspace_id) REFERENCES workspaces (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, workspace_id, connection_id)
        REFERENCES channel_connections (organization_id, workspace_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, workspace_id, id)
);

CREATE INDEX channel_send_slots_taken ON channel_send_slots (organization_id, workspace_id, connection_id, taken_at);
