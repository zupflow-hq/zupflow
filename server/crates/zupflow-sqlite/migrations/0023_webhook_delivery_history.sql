-- New attempts are immutable records, committed with their delivery's
-- summary. Existing summary-only deliveries stay as they are: the history
-- cannot reconstruct their older attempts or durations.
CREATE UNIQUE INDEX webhook_deliveries_scoped_id ON webhook_deliveries (organization_id, id);

CREATE TABLE webhook_delivery_attempts (
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
        REFERENCES webhook_deliveries (organization_id, id) ON DELETE CASCADE,
    UNIQUE (organization_id, delivery_id, number)
);

CREATE INDEX webhook_deliveries_history ON webhook_deliveries (organization_id, endpoint_id, created_at DESC, id DESC);
