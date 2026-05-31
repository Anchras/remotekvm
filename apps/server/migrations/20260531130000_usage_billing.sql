ALTER TABLE sessions
    ADD COLUMN bytes_sent BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN bytes_received BIGINT NOT NULL DEFAULT 0;

ALTER TABLE organizations
    ADD COLUMN stripe_customer_id TEXT,
    ADD COLUMN seat_count INTEGER NOT NULL DEFAULT 1;

CREATE INDEX idx_sessions_started_at ON sessions(started_at);
CREATE INDEX idx_sessions_usage_machine ON sessions(machine_id, started_at);
