-- A model attempt left unanswered because it was sent and no answer came
-- in time. The provider may have worked on it, and billed it, all the same,
-- so a turn is tried again after one of these at most once.
ALTER TABLE run_model_attempts ADD COLUMN timed_out INTEGER NOT NULL DEFAULT 0
    CHECK (timed_out IN (0, 1) AND (timed_out = 0 OR state = 'unanswered'));
