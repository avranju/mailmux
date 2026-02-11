-- Tracks sync state per mailbox
CREATE TABLE IF NOT EXISTS mailbox_states (
    id              BIGSERIAL PRIMARY KEY,
    account_id      TEXT NOT NULL,
    mailbox_name    TEXT NOT NULL,
    uid_validity    BIGINT NOT NULL,
    last_seen_uid   BIGINT NOT NULL DEFAULT 0,
    last_sync_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, mailbox_name)
);

-- Email metadata (raw message stored on filesystem)
CREATE TABLE IF NOT EXISTS emails (
    id              BIGSERIAL PRIMARY KEY,
    account_id      TEXT NOT NULL,
    mailbox_name    TEXT NOT NULL,
    uid             BIGINT NOT NULL,
    message_id      TEXT,
    subject         TEXT,
    sender          TEXT,
    recipients      JSONB,
    date            TIMESTAMPTZ,
    flags           TEXT[] NOT NULL DEFAULT '{}',
    raw_message_path TEXT NOT NULL,
    size_bytes      BIGINT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, mailbox_name, uid)
);

CREATE INDEX IF NOT EXISTS idx_emails_message_id ON emails (message_id) WHERE message_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_emails_account_mailbox ON emails (account_id, mailbox_name);
CREATE INDEX IF NOT EXISTS idx_emails_date ON emails (date);

-- Append-only event log
CREATE TABLE IF NOT EXISTS events (
    id              BIGSERIAL PRIMARY KEY,
    event_type      TEXT NOT NULL,
    account_id      TEXT NOT NULL,
    mailbox_name    TEXT NOT NULL,
    email_id        BIGINT REFERENCES emails(id),
    payload         JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_events_type ON events (event_type);
CREATE INDEX IF NOT EXISTS idx_events_created_at ON events (created_at);

-- Tracks per-processor progress and job state
CREATE TABLE IF NOT EXISTS processor_jobs (
    id              BIGSERIAL PRIMARY KEY,
    event_id        BIGINT NOT NULL REFERENCES events(id),
    processor_name  TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',
    attempts        INT NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_retry_at   TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (event_id, processor_name)
);

CREATE INDEX IF NOT EXISTS idx_processor_jobs_pending ON processor_jobs (processor_name, status)
    WHERE status IN ('pending', 'failed');
CREATE INDEX IF NOT EXISTS idx_processor_jobs_retry ON processor_jobs (next_retry_at)
    WHERE status = 'failed' AND next_retry_at IS NOT NULL;
