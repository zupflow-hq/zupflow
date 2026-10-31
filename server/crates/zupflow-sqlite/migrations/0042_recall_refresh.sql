-- A forget reaches channel sessions at their next run.
--
-- - `channel_sessions.refresh_recall`: set when what the session recalls
--   may hold a memory forgotten since: a memory of its agent, in its
--   workspace and type, forgotten through the API or by a run (one of the
--   session's own included), in the forget's own transaction; and every
--   session kept on a type when the type is deleted. The session's next run
--   clears it and reads the recall anew, from the same history start: a
--   run under way keeps the recall it started with. Saves and changes still
--   reach a session only at a trim or a new run session.

ALTER TABLE channel_sessions ADD COLUMN refresh_recall INTEGER NOT NULL DEFAULT 0
    CHECK (refresh_recall IN (0, 1) AND (refresh_recall = 0 OR memory_type_id IS NOT NULL));
