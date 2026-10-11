-- Manual resends keep their source and share its event; original deliveries
-- retain their uniqueness. Rebuild both parent and attempt children so an
-- enabled foreign-key cascade cannot delete recorded history during upgrade.
CREATE TABLE webhook_deliveries_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    endpoint_id UUID NOT NULL,
    event_id UUID NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'succeeded', 'failed', 'skipped')),
    attempts INTEGER NOT NULL CHECK (attempts >= 0),
    last_status_code INTEGER CHECK (last_status_code IS NULL OR last_status_code BETWEEN 100 AND 999),
    last_error TEXT CHECK (last_error IS NULL OR length(last_error) BETWEEN 1 AND 100),
    last_attempt_at INTEGER,
    next_attempt_at INTEGER,
    created_at INTEGER NOT NULL,
    resend_of_delivery_id UUID,
    CHECK ((state = 'pending') OR next_attempt_at IS NULL),
    FOREIGN KEY (organization_id, endpoint_id) REFERENCES webhook_endpoints (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, event_id) REFERENCES webhook_events (organization_id, id) ON DELETE CASCADE,
    FOREIGN KEY (organization_id, resend_of_delivery_id) REFERENCES webhook_deliveries_next (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, id)
);

INSERT INTO webhook_deliveries_next
    (id, organization_id, endpoint_id, event_id, state, attempts, last_status_code, last_error,
     last_attempt_at, next_attempt_at, created_at)
SELECT id, organization_id, endpoint_id, event_id, state, attempts, last_status_code, last_error,
       last_attempt_at, next_attempt_at, created_at FROM webhook_deliveries;

CREATE TABLE webhook_delivery_attempts_next (
    id UUID NOT NULL PRIMARY KEY CHECK (typeof(id) = 'blob' AND length(id) = 16),
    organization_id UUID NOT NULL REFERENCES organizations (id) ON DELETE CASCADE,
    delivery_id UUID NOT NULL,
    number INTEGER NOT NULL CHECK (number >= 1),
    status_code INTEGER CHECK (status_code IS NULL OR status_code BETWEEN 100 AND 999),
    error_class TEXT CHECK (error_class IS NULL OR error_class IN (
        'status', 'refused', 'redirected', 'timeout', 'too_large', 'unreachable',
        'failed', 'store_unavailable', 'secret_unavailable'
    )),
    attempted_at INTEGER NOT NULL,
    duration_ms INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
    FOREIGN KEY (organization_id, delivery_id)
        REFERENCES webhook_deliveries_next (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, delivery_id, number)
);

INSERT INTO webhook_delivery_attempts_next
SELECT id, organization_id, delivery_id, number, status_code, error_class, attempted_at, duration_ms
FROM webhook_delivery_attempts;

DROP TABLE webhook_delivery_attempts;
DROP TABLE webhook_deliveries;
ALTER TABLE webhook_deliveries_next RENAME TO webhook_deliveries;
ALTER TABLE webhook_delivery_attempts_next RENAME TO webhook_delivery_attempts;

CREATE UNIQUE INDEX webhook_original_deliveries ON webhook_deliveries (organization_id, event_id, endpoint_id)
    WHERE resend_of_delivery_id IS NULL;
CREATE INDEX webhook_deliveries_by_endpoint ON webhook_deliveries (organization_id, endpoint_id, created_at);
CREATE INDEX webhook_deliveries_history ON webhook_deliveries (organization_id, endpoint_id, created_at DESC, id DESC);
