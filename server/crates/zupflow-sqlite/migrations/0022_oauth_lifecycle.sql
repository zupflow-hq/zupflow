-- A refresh token is single use. Commit its claim before sending it; a
-- process that dies after the exchange never permits that token to be reused.
ALTER TABLE installations ADD COLUMN refresh_started_at INTEGER;
ALTER TABLE oauth_client_registrations ADD COLUMN response_iss_supported INTEGER NOT NULL DEFAULT 0 CHECK (response_iss_supported IN (0, 1));
ALTER TABLE workspace_oauth_client_registrations ADD COLUMN response_iss_supported INTEGER NOT NULL DEFAULT 0 CHECK (response_iss_supported IN (0, 1));
