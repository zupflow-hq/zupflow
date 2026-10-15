-- The tokens a model read from its provider's prompt cache and wrote to it,
-- per answered model attempt, per committed turn and summed on the run.
-- Both are parts of `input_tokens`, which counts every token the model
-- read, never added on top. Runs, turns and attempts from before this
-- migration read as none.
ALTER TABLE runs ADD COLUMN cache_read_input_tokens INTEGER NOT NULL DEFAULT 0
    CHECK (cache_read_input_tokens >= 0);
ALTER TABLE runs ADD COLUMN cache_write_input_tokens INTEGER NOT NULL DEFAULT 0
    CHECK (cache_write_input_tokens >= 0 AND cache_read_input_tokens + cache_write_input_tokens <= input_tokens);

ALTER TABLE run_turns ADD COLUMN cache_read_input_tokens INTEGER NOT NULL DEFAULT 0
    CHECK (cache_read_input_tokens >= 0);
ALTER TABLE run_turns ADD COLUMN cache_write_input_tokens INTEGER NOT NULL DEFAULT 0
    CHECK (cache_write_input_tokens >= 0 AND cache_read_input_tokens + cache_write_input_tokens <= input_tokens);

-- Set with the attempt's answer, as its input and output tokens are.
ALTER TABLE run_model_attempts ADD COLUMN cache_read_input_tokens INTEGER
    CHECK (cache_read_input_tokens IS NULL OR (cache_read_input_tokens >= 0 AND state = 'answered'));
ALTER TABLE run_model_attempts ADD COLUMN cache_write_input_tokens INTEGER CHECK (
    (cache_read_input_tokens IS NULL) = (cache_write_input_tokens IS NULL)
    AND (cache_write_input_tokens IS NULL
        OR (cache_write_input_tokens >= 0 AND cache_read_input_tokens + cache_write_input_tokens <= input_tokens))
);

-- Stored JSON that embeds a run's usage gets the two counts too, as none,
-- so what was written before reads as the contract's `RunUsage`: the
-- `run.completed` events, and run bodies saved for idempotent retries.
UPDATE run_events
SET data = json_insert(data, '$.usage.cache_read_input_tokens', 0, '$.usage.cache_write_input_tokens', 0)
WHERE type = 'run.completed' AND json_type(data, '$.usage') = 'object';

UPDATE idempotency_keys
SET body = CAST(json_insert(
    CAST(body AS TEXT), '$.data.usage.cache_read_input_tokens', 0, '$.data.usage.cache_write_input_tokens', 0
) AS BLOB)
WHERE CASE WHEN json_valid(CAST(body AS TEXT))
    THEN json_type(CAST(body AS TEXT), '$.data.usage.tool_calls') IS NOT NULL
    ELSE 0 END;

UPDATE user_idempotency_keys
SET body = CAST(json_insert(
    CAST(body AS TEXT), '$.data.usage.cache_read_input_tokens', 0, '$.data.usage.cache_write_input_tokens', 0
) AS BLOB)
WHERE CASE WHEN json_valid(CAST(body AS TEXT))
    THEN json_type(CAST(body AS TEXT), '$.data.usage.tool_calls') IS NOT NULL
    ELSE 0 END;
