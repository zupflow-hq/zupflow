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
-- - `recall_weight`: the tokens the recall the session keeps takes, kept
--   when a mark clears its text (below), so the next run's trim still
--   counts what the runs from that start sent. Set with the recall, and
--   back to 0 when a new run session starts; never more than 0 without
--   the session's memory, nor 0 beside a recall.
-- - `refresh_recall`: set when what the session recalls may hold a memory
--   forgotten or changed since: a memory of its agent, in its workspace and
--   type, that its audience is shown (the workspace's, or a `direct`
--   session's own conversation's), forgotten through the API or by a run
--   (one of the session's own included), or changed through the API or the
--   Console, in the change's own transaction; and every session kept on a
--   type when the type is deleted. The same transaction clears the recall's
--   text and start on a session with no run under way, and a marked
--   session's run clears them as it ends, so no forgotten or changed text
--   stays at rest. Set too by the first model attempt of a run that read
--   the recall while the session's type couldn't give one (its kind not
--   served, or refusing the type's settings), so that nothing is kept for
--   the run alone. The session's next run clears the mark and reads the
--   recall anew, from the same history start: a run under way keeps the
--   recall it started with. Saves, and a run's changes, still reach a
--   session only at a trim or a new run session. Never set on a session
--   without memory.
-- - `app_content_read`: whether any run of the session read an app's tool
--   result, set as such a run ends and never cleared, so a save in any
--   later run in the thread says it may come from that content, even one
--   after the session started over (a trim, a new run session, the
--   retention sweep): a new run session reads the thread again, the
--   agent's earlier replies too.
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

ALTER TABLE channel_sessions ADD COLUMN recall_weight INTEGER NOT NULL DEFAULT 0
    CHECK (recall_weight >= 0 AND (recall_weight = 0 OR memory_type_id IS NOT NULL)
        AND (recall IS NULL OR recall_weight >= 1));

ALTER TABLE channel_sessions ADD COLUMN refresh_recall INTEGER NOT NULL DEFAULT 0
    CHECK (refresh_recall IN (0, 1) AND (refresh_recall = 0 OR memory_type_id IS NOT NULL));

ALTER TABLE channel_sessions ADD COLUMN app_content_read INTEGER NOT NULL DEFAULT 0
    CHECK (app_content_read IN (0, 1));
