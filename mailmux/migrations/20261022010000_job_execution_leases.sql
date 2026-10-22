ALTER TABLE processor_jobs ADD COLUMN claim_version BIGINT NOT NULL DEFAULT 0;
ALTER TABLE processor_jobs ADD COLUMN lease_expires_at TIMESTAMPTZ;
CREATE INDEX idx_processor_jobs_expired_leases ON processor_jobs (lease_expires_at)
    WHERE status = 'in_progress';
