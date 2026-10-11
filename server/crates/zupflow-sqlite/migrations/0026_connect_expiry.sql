-- The bounded cross-tenant expiry sweep visits only due pending sessions.
CREATE INDEX connect_sessions_pending_expiry ON connect_sessions (expires_at, id)
    WHERE status = 'pending';
