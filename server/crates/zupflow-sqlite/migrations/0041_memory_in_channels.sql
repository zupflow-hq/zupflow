-- Memory in channel runs: a session's memory, fixed with its tools, and the
-- recall its runs share.
--
-- - `channel_sessions.memory_type_id` and `memory_audience`: the memory type
--   the session's memory tools are of, and who its runs recall and save for
--   (`direct`: the workspace's memories and those of the session's own
--   conversation, a `dm` or a `group_dm`; `workspace`: the workspace's
--   only), fixed with the tools when the session began. Neither for a
--   session without memory, and neither before its first run.
-- - `recall` and `recall_start_run_id`: what the session's history begins
--   with, framed, and the run that history starts at. The first run from a
--   start reads it, from the session's type for its audience, and every run
--   from the same start sends it again: a run keeps only the type and the
--   recall's digest. A new start (a trim, or a new run session) reads it
--   anew. A recall is never empty, and never without its start or the
--   session's memory; a start is never without the session's memory.
-- - `refresh_recall`: set when what the session recalls may hold a memory
--   forgotten since: a memory of its agent, in its workspace and type,
--   forgotten through the API or by a run (one of the session's own
--   included), in the forget's own transaction; and every session kept on a
--   type when the type is deleted. The session's next run clears it and
--   reads the recall anew, from the same history start: a run under way
--   keeps the recall it started with. Saves and changes still reach a
--   session only at a trim or a new run session. Never set on a session
--   without memory.
-- - `app_content_read`: whether a run of the session read an app's tool
--   result, set as such a run ends and kept from then on, so a save in a
--   later run says it may come from that content.
--
-- Sessions from before this have no memory: the next run that starts one
-- with the agent's memory starts a new run session.

ALTER TABLE channel_sessions ADD COLUMN memory_type_id UUID
    CHECK (memory_type_id IS NULL OR (
        model IS NOT NULL AND typeof(memory_type_id) = 'blob' AND length(memory_type_id) = 16
    ));

ALTER TABLE channel_sessions ADD COLUMN memory_audience TEXT
    CHECK ((memory_audience IS NULL) = (memory_type_id IS NULL)
        AND (memory_audience IS NULL OR memory_audience IN ('workspace', 'direct')));

ALTER TABLE channel_sessions ADD COLUMN recall_start_run_id UUID
    CHECK (recall_start_run_id IS NULL OR (
        memory_type_id IS NOT NULL AND typeof(recall_start_run_id) = 'blob' AND length(recall_start_run_id) = 16
    ));

ALTER TABLE channel_sessions ADD COLUMN recall TEXT
    CHECK (recall IS NULL OR (recall_start_run_id IS NOT NULL AND length(recall) >= 1));

ALTER TABLE channel_sessions ADD COLUMN refresh_recall INTEGER NOT NULL DEFAULT 0
    CHECK (refresh_recall IN (0, 1) AND (refresh_recall = 0 OR memory_type_id IS NOT NULL));

ALTER TABLE channel_sessions ADD COLUMN app_content_read INTEGER NOT NULL DEFAULT 0
    CHECK (app_content_read IN (0, 1));
