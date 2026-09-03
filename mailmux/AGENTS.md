# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

## What This Project Is

**mailmux** is an event-driven IMAP email processing daemon written in Rust (edition 2024). It synchronizes emails from IMAP servers into PostgreSQL and triggers configurable processing pipelines for each incoming message. It is not an email client and does not send mail.

## Build & Run Commands

```bash
# Build
cargo build
cargo build --release

# Run all tests
cargo test

# Run a single test (substring match on test path/name)
cargo test test_load_valid_config
cargo test config::tests
cargo test -- --nocapture   # show stdout

# Run the daemon
mailmux --config config.toml
mailmux --config config.toml --log-level debug

# Replay/dry-run subcommands
mailmux replay --event-id 42
mailmux replay --event-id 42 --processor notify
mailmux dry-run --event-id 42 --processor notify

# Historical backfill (runs one processor over durable emails; no events or
# processor_jobs rows are persisted)
mailmux backfill --processor mail-indexer --account personal --after 2021-01-01 --before 2022-01-01
mailmux backfill --processor mail-indexer --account personal --after 2021-01-01 --before 2022-01-01 --dry-run

# Docker
docker compose up -d
docker compose logs -f mailmux
```

No Makefile — everything goes through standard Cargo.

## Architecture

Two-stage pipeline:

1. **Ingestion** — IMAP servers → `AccountManager` → `MailboxWatcher` → atomic PostgreSQL transaction (INSERT email + INSERT event + `pg_notify`) + `.eml` file on disk
2. **Processing** — PostgreSQL `LISTEN/NOTIFY` → `EventLoop` → `JobScheduler` → `Processor`(s) → `processor_jobs` table

```
Main
├── AccountManager          one per configured account
│   └── MailboxWatcher      one per mailbox; sync loop + IMAP IDLE with poll fallback
├── EventLoop               PgListener on `mailmux_events` + 5s poll fallback
├── JobScheduler            dispatches events → processors, retry sweep every 10s
├── HealthServer            axum HTTP: /health, /ready, /metrics
└── Housekeeping            event cleanup every 1 hour
```

**Shutdown:** `CancellationToken` broadcast to all tasks on SIGINT/SIGTERM; tasks are aborted after a grace period (default 10s).

**Circuit breaker:** 10 failures within 5 minutes → 15-minute cooldown for MailboxWatcher restarts.

## Key Modules (`src/`)

| File | Role |
|---|---|
| `main.rs` | Wires all tasks together; implements `cmd_run`, `cmd_replay`, `cmd_dry_run`; dispatches `backfill::run` |
| `cli.rs` | clap CLI: `--config`, `--log-level`, subcommands; `BackfillArgs` + date parsing/validation |
| `backfill.rs` | Historical backfill orchestration: processor resolution, transient event construction, retry/timeout execution, bounded chunk concurrency, keyset pagination, summary + exit status |
| `config.rs` | TOML loading with `${VAR}` env substitution + validation |
| `store.rs` | Filesystem store for raw `.eml` files at `{data_dir}/{account}/{mailbox}/{uid}.eml` |
| `db/mod.rs` | PgPool setup + sqlx migrate runner |
| `db/emails.rs` | `EmailRecord`, `MailboxState`, email fetch/upsert; backfill `EmailBackfillFilter` + parameterized count/keyset-page queries |
| `db/events.rs` | `Event`, atomic `insert_email_with_event` (with pg_notify), unprocessed event query |
| `db/jobs.rs` | `ProcessorJob` CRUD: create, status update, pending/retryable fetch |
| `events/listener.rs` | `EventLoop`: PgListener + poll fallback, dispatches to scheduler |
| `imap/mod.rs` | `AccountManager`: spawns/supervises MailboxWatchers, circuit breaker logic |
| `imap/connection.rs` | `ImapConnection` wrapping `imap-next`: connect+TLS, login, SELECT, UID FETCH, IDLE |
| `imap/sync.rs` | `MailboxWatcher`: sync+IDLE cycle, polling fallback, rate limiting, message ingestion |
| `processor/mod.rs` | `Processor` trait + `ProcessorOutput` |
| `processor/registry.rs` | Builds processors from config, matches events to processors |
| `processor/scheduler.rs` | Dispatches events, executes processors, handles failures/retries |
| `processor/builtin/logger.rs` | Logs event details via tracing |
| `processor/builtin/command.rs` | Spawns external CLI, passes event JSON on stdin |

## Database Schema

Four tables managed by a single migration in `migrations/`:
- `mailbox_states` — `uid_validity` + `last_seen_uid` per account+mailbox
- `emails` — parsed email metadata + path to raw `.eml` on disk
- `events` — append-only event log (`event_type`, account, mailbox, email_id, JSONB payload)
- `processor_jobs` — per (event, processor) tracking: status (`pending`/`in_progress`/`completed`/`failed`/`abandoned`), attempts, last_error, next_retry_at

## Configuration

Config is TOML with `${VAR}` env substitution. See `config.example.toml` for an annotated reference. Top-level sections:
- `[general]` — data_dir, log_level, log_format, shutdown_grace_period_secs, event_retention_days, health_port
- `[database]` — url, max_connections
- `[[accounts]]` — per IMAP account settings (host, port, TLS, credentials, mailboxes, rate limits)
- `[[processors]]` — name, enabled, events filter, retry settings, concurrency, processor-specific `config` map

## Tests

Tests live in `#[cfg(test)] mod tests` blocks within each source file. Async tests use `#[tokio::test]`. Store tests use `tempfile` for temporary directories; config tests use `tempfile::NamedTempFile`.

Current test coverage: `src/config.rs` and `src/store.rs` have unit tests. No integration tests or test database setup exists.

## Historical backfill (`mailmux backfill`)

`backfill.rs` runs exactly one configured processor (exact name match, must be
enabled and subscribed to `email_arrived`) over durable `emails` rows:

- Selection reads `emails` directly (never `events`) via parameterized,
  keyset-paginated queries (`id > last_id ORDER BY id ASC LIMIT ≤ 500`) in
  `db/emails.rs`; `--limit` caps the count, and a parameterized COUNT query
  computes `selected` for dry-run/summary.
- Each email is invoked with a **transient** `Event` whose `id == 0` (reserved
  for non-persisted backfill events — see `db/events.rs`), `event_type =
  "email_arrived"`, and a payload containing `backfill: true` plus
  `email_id`/`uid`/`subject`/`sender`. The `Processor::process(&Event,
  Option<&EmailRecord>)` contract and the command-processor `{event, email}`
  stdin JSON (including `email.raw_message_path`) are unchanged.
- **Never** insert `events` or `processor_jobs` rows from the backfill path —
  retry state is in-memory only, and downstream processors are expected to be
  idempotent.
- Per-attempt timeout, `max_retries`, and `retry_backoff_secs` come from the
  selected `ProcessorConfig`; concurrency is bounded by the processor's
  `concurrency` (overridable via `--concurrency`) using deterministic
  contiguous chunks. `--fail-fast` stops before scheduling the chunk after a
  permanent failure; the final summary (`selected`/`processed`/`succeeded`/
  `failed`/`skipped`/`elapsed`) is logged before a partial-failure run returns
  a non-zero exit.

## Important Notes

- The project uses `imap-next` (not `async-imap`) for the IMAP client.
- `sqlx` is used with compile-time query checking; the database must be available (or `DATABASE_URL` set) when running `sqlx` macros during a fresh build.
- Systemd integration uses `sd-notify` (`Type=notify` in the service unit). See `contrib/mailmux.service`.
- Task 4.4 (flag change + deletion detection) is the only deferred feature; all other planned phases are complete.
