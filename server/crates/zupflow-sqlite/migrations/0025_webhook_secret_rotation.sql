-- One previous sealed version gives consumers a bounded overlap. Existing
-- endpoints keep their current ciphertext and begin with no overlap.
ALTER TABLE webhook_endpoints ADD COLUMN previous_secret_version INTEGER;
ALTER TABLE webhook_endpoints ADD COLUMN previous_secret_key_id TEXT;
ALTER TABLE webhook_endpoints ADD COLUMN previous_secret_wrapped_key BLOB;
ALTER TABLE webhook_endpoints ADD COLUMN previous_secret_nonce BLOB;
ALTER TABLE webhook_endpoints ADD COLUMN previous_secret_ciphertext BLOB;
ALTER TABLE webhook_endpoints ADD COLUMN previous_secret_expires_at INTEGER
    CHECK (
        (previous_secret_version IS NULL AND previous_secret_key_id IS NULL
            AND previous_secret_wrapped_key IS NULL AND previous_secret_nonce IS NULL
            AND previous_secret_ciphertext IS NULL AND previous_secret_expires_at IS NULL)
        OR
        (previous_secret_version IS NOT NULL AND previous_secret_version >= 1
            AND previous_secret_version < secret_version AND previous_secret_key_id IS NOT NULL
            AND previous_secret_wrapped_key IS NOT NULL AND previous_secret_nonce IS NOT NULL
            AND length(previous_secret_nonce) = 12 AND previous_secret_ciphertext IS NOT NULL
            AND previous_secret_expires_at IS NOT NULL)
    );
