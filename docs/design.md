# High-Level Design: Mailmux

## 1. System Overview

Mailmux is a background daemon designed to reliably synchronize emails from IMAP servers into a local PostgreSQL database and trigger event-driven processing pipelines. It emphasizes data integrity, recoverability, and idempotency.

The system is composed of two active stages:

1.  **Ingestion + Event Creation (IMAP -> DB)**: Fetches emails and stores them alongside their corresponding events in a single atomic transaction. This guarantees that no events are lost and no data changes go unnoticed.
2.  **Processing (Event -> Processor)**: Consumes events from the database and executes registered processors (business logic).

There is no separate "event generation" stage — event creation is embedded in the ingestion transaction, not a separate polling loop that scans the DB for changes.

## 2. Architecture Modules

### 2.1 Core
*   **Main**: Application entry point. Parses CLI arguments (via `clap`), initializes runtime, logger, database pool, and spawns manager tasks.
*   **Config**: Handles loading `config.toml`, merging environment variables, and validating settings. Uses `serde` + `toml`.

### 2.2 Storage Layer (PostgreSQL)
*   **Database**: PostgreSQL is the single source of truth for metadata and events.
*   **Message Store**: Raw RFC 5322 messages and attachments are stored on the filesystem, with only metadata and parsed headers in PostgreSQL. This prevents DB bloat.
*   **Models**:
    *   `MailboxState`: Tracks `uid_validity` and `last_seen_uid`.
    *   `Email`: Stores parsed headers, metadata, flags, and a filesystem path to the raw message.
    *   `Event`: Append-only log of interesting occurrences (NewEmail, FlagChange).
    *   `ProcessorJob`: Tracks the state of an event being processed by a specific processor (Pending, Success, Failed).
*   **Library**: `sqlx` for async, type-safe queries.

### 2.3 Ingestion (The "Source")
*   **AccountManager**: Manages the lifecycle of IMAP connections. Supervises per-account tasks and restarts them on failure (see section 6).
*   **MailboxWatcher**: A task per mailbox.
    *   **Sync**: Performs initial fetch (bounded by configurable limits) and incremental updates (UID fetch). Rate-limited per account.
    *   **Idle** (Phase 4): Uses IMAP IDLE to listen for real-time updates. Sends DONE on shutdown, reconnects on timeout.
*   **Library**: `imap-next` (built on `imap-codec`, actively maintained, designed for async from the ground up). `mail-parser` for RFC 5322 parsing.
*   **Note on `async-imap`**: This crate has had periods of low maintenance. `imap-next` is preferred. This decision should be revisited if `imap-next` proves insufficient during Phase 2 implementation.

### 2.4 Event Bus
*   **EventLog**: Part of the storage transaction. When an email is inserted or updated, an `Event` is created in the **same transaction**.
*   **EventLoop**: Notified of new events via **PostgreSQL `LISTEN/NOTIFY`**. Falls back to periodic polling as a safety net (e.g., every 5 seconds). This ensures low-latency event dispatch without excessive DB load.

### 2.5 Processor System (The "Sink")
*   **ProcessorRegistry**: Loads compiled-in trait-based processors at startup. Each processor is a Rust type implementing the `Processor` trait. Out-of-process processors (CLI/HTTP) are a Phase 4 addition.
*   **JobScheduler**: Distributes events to processors.
    *   Ensures a processor processes events in order (if required) or concurrently.
    *   Handles retries and backoff logic.
*   **State Tracking**: Updates `processor_jobs` table to ensure at-least-once delivery.

## 3. Data Flow

1.  **Startup**: Parse CLI args, load config, connect to DB, run migrations.
2.  **Connect**: `AccountManager` connects to IMAP servers.
3.  **Sync/Idle**:
    *   IMAP Server reports new message (UID 123).
    *   `MailboxWatcher` fetches message 123 (respecting rate limits).
4.  **Persist** (atomic):
    *   Start DB Transaction.
    *   Write raw message to filesystem.
    *   Insert `email` metadata record (with path to raw message).
    *   Insert `event` record (`type: email_arrived`, `ref: email_id`).
    *   Commit Transaction.
    *   Send `NOTIFY` on the events channel.
5.  **Dispatch**:
    *   `EventLoop` receives notification.
    *   `JobScheduler` sees new `event`.
    *   Finds matching processors (e.g., "WebhookProcessor").
    *   Creates/Updates `processor_job` entry to `IN_PROGRESS`.
6.  **Execute**:
    *   `WebhookProcessor` runs (sends HTTP POST).
    *   If success: Update `processor_job` to `COMPLETED`.
    *   If fail: Update `processor_job` to `FAILED` (schedule retry).

## 4. Key Technology Stack

*   **Language**: Rust (Edition 2024)
*   **Runtime**: `tokio` (Async I/O)
*   **CLI**: `clap` (argument parsing and subcommands)
*   **Database**: PostgreSQL
*   **ORM/Query**: `sqlx`
*   **IMAP**: `imap-next` + `mail-parser`
*   **Serialization**: `serde` + `toml`
*   **Logging**: `tracing` + `tracing-subscriber`

## 5. Database Schema

### 5.1 Tables

```sql
-- Tracks sync state per mailbox
CREATE TABLE mailbox_states (
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
CREATE TABLE emails (
    id              BIGSERIAL PRIMARY KEY,
    account_id      TEXT NOT NULL,
    mailbox_name    TEXT NOT NULL,
    uid             BIGINT NOT NULL,
    message_id      TEXT,           -- RFC 5322 Message-ID header
    subject         TEXT,
    sender          TEXT,
    recipients      JSONB,
    date            TIMESTAMPTZ,
    flags           TEXT[] NOT NULL DEFAULT '{}',
    raw_message_path TEXT NOT NULL,  -- filesystem path to raw RFC 5322 message
    size_bytes      BIGINT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id, mailbox_name, uid)
);

CREATE INDEX idx_emails_message_id ON emails (message_id) WHERE message_id IS NOT NULL;
CREATE INDEX idx_emails_account_mailbox ON emails (account_id, mailbox_name);
CREATE INDEX idx_emails_date ON emails (date);

-- Append-only event log
CREATE TABLE events (
    id              BIGSERIAL PRIMARY KEY,
    event_type      TEXT NOT NULL,   -- 'email_arrived', 'flags_changed', 'email_deleted', 'sync_completed'
    account_id      TEXT NOT NULL,
    mailbox_name    TEXT NOT NULL,
    email_id        BIGINT REFERENCES emails(id),
    payload         JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_events_unprocessed ON events (id) WHERE id > (
    SELECT COALESCE(MAX(last_processed_event_id), 0) FROM processor_cursors
);
CREATE INDEX idx_events_type ON events (event_type);
CREATE INDEX idx_events_created_at ON events (created_at);

-- Tracks per-processor progress and job state
CREATE TABLE processor_jobs (
    id              BIGSERIAL PRIMARY KEY,
    event_id        BIGINT NOT NULL REFERENCES events(id),
    processor_name  TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',  -- pending, in_progress, completed, failed, abandoned
    attempts        INT NOT NULL DEFAULT 0,
    last_error      TEXT,
    next_retry_at   TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (event_id, processor_name)
);

CREATE INDEX idx_processor_jobs_pending ON processor_jobs (processor_name, status)
    WHERE status IN ('pending', 'failed');
CREATE INDEX idx_processor_jobs_retry ON processor_jobs (next_retry_at)
    WHERE status = 'failed' AND next_retry_at IS NOT NULL;
```

### 5.2 Schema Management

*   Migrations managed via `sqlx migrate`.
*   **Event table retention**: Old processed events should be cleaned up periodically (configurable retention period, e.g., 30 days). A background task truncates events older than the retention window where all processor_jobs are completed.

## 6. Error Handling & Supervision Strategy

### 6.1 Supervision Tree

The system uses a hierarchical supervision model built on `tokio::JoinSet`:

```
Main
├── AccountManager (one per account)
│   ├── MailboxWatcher (one per mailbox)
│   │   ├── Sync loop
│   │   └── IDLE listener (Phase 4)
│   └── Connection management
├── EventLoop
│   └── JobScheduler
│       └── Processor tasks
└── Housekeeping (event cleanup, health checks)
```

### 6.2 Failure Handling

| Failure | Response |
|---------|----------|
| IMAP connection refused / timeout | Exponential backoff with jitter, max interval (e.g., 5 min). Log warning. |
| IMAP auth failure | Log error, stop retrying for this account. Requires config fix + restart. |
| PostgreSQL unavailable | All tasks pause. Retry connection with backoff. Resume from last known state. |
| PostgreSQL transaction failure | Retry the transaction (up to 3 times). If persistent, log error and skip the message (resume on next sync). |
| MailboxWatcher task panic | `AccountManager` detects via `JoinSet`, logs error, restarts the watcher after a delay. |
| AccountManager task panic | Main loop detects via `JoinSet`, logs error, restarts the account manager after a delay. |
| Processor execution failure | Handled by retry logic (see features.md 7.2). Does not affect other processors or ingestion. |
| Filesystem write failure (raw message) | Log error, skip message, retry on next sync cycle. |

### 6.3 Circuit Breaker

If a task fails repeatedly (e.g., 10 consecutive failures within 5 minutes), it enters a "cooldown" state with a longer backoff (e.g., 15 minutes) before resuming. This prevents tight failure loops from consuming resources.

## 7. Graceful Shutdown Semantics

On receiving SIGTERM/SIGINT:

1.  **Signal all tasks to stop.** Use a `tokio::sync::watch` channel or `CancellationToken` to broadcast shutdown.
2.  **IDLE connections**: Send IMAP `DONE` command, then `LOGOUT`. Do not wait for new messages.
3.  **In-flight sync batches**: Abandon partially fetched batches. The UID-based sync strategy ensures no data loss — unfetched messages will be picked up on the next startup.
4.  **In-flight processor jobs**: Allow currently executing processors a grace period (configurable, default 10 seconds) to complete. After the grace period, mark remaining in-progress jobs as `pending` so they are retried on next startup.
5.  **Database connections**: Flush pending writes, close the connection pool.
6.  **Exit.** Return appropriate exit code (0 for clean shutdown).

The system's idempotency guarantees (UID-based sync, at-least-once event processing) ensure correctness across unclean shutdowns as well.

## 8. Example Configuration

```toml
[general]
data_dir = "/var/lib/mailmux"       # where raw messages are stored
log_level = "info"                   # trace, debug, info, warn, error
log_format = "json"                  # json or pretty
shutdown_grace_period_secs = 10

[database]
url = "postgres://mailmux:password@localhost:5432/mailmux"
max_connections = 10

# Each [[accounts]] entry defines one IMAP account to monitor.
[[accounts]]
id = "personal"
imap_host = "imap.gmail.com"
imap_port = 993
tls = true
username = "user@gmail.com"
# Password can be provided via env var: MAILMUX_ACCOUNT_PERSONAL_PASSWORD
password = "${MAILMUX_ACCOUNT_PERSONAL_PASSWORD}"
poll_interval_secs = 60
rate_limit_per_second = 5
max_connections = 2

# Mailboxes to monitor for this account.
mailboxes = ["INBOX", "Sent"]

# Initial sync bounds (optional). Omit to sync all messages.
initial_sync_max_messages = 1000
# initial_sync_max_age_days = 90

[[accounts]]
id = "work"
imap_host = "outlook.office365.com"
imap_port = 993
tls = true
username = "user@company.com"
password = "${MAILMUX_ACCOUNT_WORK_PASSWORD}"
poll_interval_secs = 120
rate_limit_per_second = 3
max_connections = 2
mailboxes = ["INBOX"]

# Processor definitions.
[[processors]]
name = "webhook"
enabled = true
events = ["email_arrived"]
max_retries = 3
retry_backoff_secs = [5, 30, 300]   # exponential backoff schedule
timeout_secs = 30
concurrency = 2

[processors.config]
url = "https://example.com/hooks/new-email"
method = "POST"
headers = { "Authorization" = "Bearer ${WEBHOOK_TOKEN}" }

[[processors]]
name = "logger"
enabled = true
events = ["email_arrived", "flags_changed", "email_deleted"]
max_retries = 0
timeout_secs = 5
concurrency = 1
```

## 9. Implementation Phases

1.  **Phase 1 — Foundation**: Basic CLI (clap), config loading + validation, DB connection + migrations, structured logging (tracing), graceful shutdown skeleton (CancellationToken).
2.  **Phase 2 — Ingestion**: Connect to IMAP (imap-next), fetch emails, store raw messages on filesystem, store metadata in DB, UID-based sync, rate limiting, initial sync bounds.
3.  **Phase 3 — Events & Processing**: Atomic event creation in ingestion transaction, LISTEN/NOTIFY event dispatch, processor trait + registry, job scheduler, basic processor execution.
4.  **Phase 4 — Refinement**: IMAP IDLE support, retry logic + backoff, out-of-process processors (CLI/HTTP), supervision/restart on failure, systemd integration.
5.  **Phase 5 — Observability & Operational Hardening**: Prometheus metrics export, dry-run / replay debugging tools, health check HTTP endpoint, event retention/cleanup automation.
