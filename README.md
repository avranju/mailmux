# mailmux workspace

This repository is a Rust workspace with:

- `mailmux`: an event-driven IMAP email processing daemon
- `mailtx`: a command-processor companion crate for extracting bank transaction
  data from emails and posting it to an HTTP endpoint
- `mailindex`: a local-first RFC 5322 normalization, Tantivy search, HTTP, and MCP service

`mailmux` synchronizes emails from IMAP servers into PostgreSQL and triggers
configurable processing pipelines for each incoming message.

The core value of `mailmux` is **reliable, replayable, event-driven mail processing** with
emphasis on data integrity, recoverability, and idempotency.

## Workspace layout

```text
.
├── Cargo.toml                  # workspace manifest
├── Cargo.lock                  # shared workspace dependencies
├── Dockerfile                  # builds mailmux, mailtx, and mailindex
├── README.md                   # workspace overview (this file)
├── mailmux/                    # daemon crate
│   ├── config.example.toml
│   ├── docker-compose.yml
│   ├── migrations/
│   └── src/
├── mailtx/                     # command processor crate
│   ├── README.md
│   └── src/
├── mailindex/                  # normalized mail search service
│   ├── README.md
│   ├── config.example.toml
│   └── src/
└── run/                        # local helper scripts
```

## mailmux features

- **Multi-account IMAP sync** — monitor multiple accounts and mailboxes
  concurrently with UID-based incremental sync
- **IMAP IDLE** — real-time push notifications when the server supports it,
  with automatic fallback to polling
- **Atomic event creation** — email ingestion and event creation happen in a
  single database transaction so nothing is lost
- **Processor pipelines** — built-in logger and CLI command processors; add
  your own by implementing the `Processor` trait or pointing at an external
  script
- **Retry with backoff** — failed processor jobs are retried on a configurable
  schedule and eventually marked as abandoned
- **Supervision** — crashed IMAP watchers are automatically restarted with a
  circuit breaker (10 failures in 5 minutes triggers a 15-minute cooldown)
- **Prometheus metrics** — counters for messages ingested, events created, and
  processor runs; gauges for active connections and monitored mailboxes,
  exposed at `/metrics`
- **Health checks** — `/health` (database connectivity) and `/ready`
  (startup complete) HTTP endpoints
- **Replay & dry-run** — CLI subcommands to re-run processors for a specific
  event or test a processor without persisting results
- **Historical backfill** — explicitly run one configured processor against
  durable historical emails (filtered by date/account/mailbox) without
  depending on retained events
- **Event retention** — background cleanup of old processed events
  (configurable, default 30 days)
- **systemd integration** — `Type=notify` service with `sd-notify` readiness
  signaling
- **Environment variable substitution** — use `${VAR}` in config values to
  inject secrets

## Architecture

```mermaid
graph TD
    A[IMAP Servers] --> B[AccountManager<br/>per account]
    B --> C[MailboxWatcher<br/>per mailbox]
    C --> D[Atomic Transaction<br/>INSERT email + INSERT event + pg_notify]
    C --> E[(Filesystem<br/>raw .eml files)]
    D --> F[(PostgreSQL<br/>metadata + events)]
    D --> G[EventLoop<br/>LISTEN/NOTIFY + poll fallback]
    G --> H[JobScheduler]
    H --> I[Processor<br/>logger / command / custom]
    I --> J[(processor_jobs<br/>status tracking + retries)]
```

Storage is split between PostgreSQL (metadata, events, job state) and the
filesystem (raw RFC 5322 messages stored as `.eml` files).

## Prerequisites

- **Rust** 1.93+ (edition 2024) — install via [rustup](https://rustup.rs)
- **PostgreSQL** 12+

## Build

```bash
cargo build --workspace --release
```

Workspace binaries are written to:

- `target/release/mailmux`
- `target/release/mailtx`
- `target/release/mailindex`

Run the tests:

```bash
cargo test --workspace
```

## Database setup

Create a database and user:

```bash
createdb mailmux
createuser mailmux
```

Grant the required privileges:

```sql
GRANT ALL PRIVILEGES ON DATABASE mailmux TO mailmux;
```

Migrations run automatically on startup. The schema creates four tables:

| Table            | Purpose                                                     |
| ---------------- | ----------------------------------------------------------- |
| `mailbox_states` | Tracks sync state per mailbox (uid_validity, last_seen_uid) |
| `emails`         | Email metadata with path to raw message on disk             |
| `events`         | Append-only event log (email_arrived, etc.)                 |
| `processor_jobs` | Processing state per event/processor pair; includes nullable `output` JSONB column persisting the complete ProcessorOutput from each execution. Replays clear stale output before re-executing. |

## mailmux configuration

`mailmux` uses a TOML configuration file. By default it looks for `config.toml`
in the working directory; override with `-c /path/to/config.toml`. A complete
example lives at `mailmux/config.example.toml`.

Values can reference environment variables with `${VAR_NAME}` syntax — useful
for secrets.

### Example

```toml
[general]
data_dir = "/var/lib/mailmux"
log_level = "info"             # trace | debug | info | warn | error
log_format = "json"            # json | pretty
shutdown_grace_period_secs = 10
event_retention_days = 30
health_port = 8080             # omit to disable HTTP endpoints

[database]
url = "postgres://mailmux:${DB_PASSWORD}@localhost:5432/mailmux"
max_connections = 10

[[accounts]]
id = "personal"
enabled = true
imap_host = "imap.gmail.com"
imap_port = 993
tls = true
username = "user@gmail.com"
password = "${GMAIL_APP_PASSWORD}"
poll_interval_secs = 60
rate_limit_per_second = 5
mailboxes = ["INBOX"]
initial_sync_max_messages = 1000
idle_heartbeat_interval_secs = 600  # optional safety net; omit to disable

[[accounts]]
id = "work"
enabled = true
imap_host = "outlook.office365.com"
imap_port = 993
tls = true
username = "user@company.com"
password = "${WORK_PASSWORD}"
poll_interval_secs = 120
rate_limit_per_second = 3
mailboxes = ["INBOX", "Sent"]

[[processors]]
name = "logger"
enabled = true
events = ["email_arrived"]
timeout_secs = 5
concurrency = 1

[[processors]]
name = "notify"
enabled = true
events = ["email_arrived"]
max_retries = 3
retry_backoff_secs = [5, 30, 300]
timeout_secs = 30
concurrency = 2

[processors.config]
command = "/usr/local/bin/notify-new-mail"
args = ["--json"]
env = { NOTIFY_TOKEN = "${NOTIFY_TOKEN}" }
```

### Account options

| Field                       | Default    | Description                                                                                                               |
| --------------------------- | ---------- | ------------------------------------------------------------------------------------------------------------------------- |
| `id`                        | _required_ | Unique identifier for the account                                                                                         |
| `enabled`                   | `true`     | Whether the account is active; if `false`, mailmux logs and skips it                                                      |
| `imap_host`                 | _required_ | IMAP server hostname                                                                                                      |
| `imap_port`                 | `993`      | IMAP server port                                                                                                          |
| `tls`                       | `true`     | Use implicit TLS (port 993). See note below.                                                                              |
| `username`                  | _required_ | IMAP username                                                                                                             |
| `password`                  | _required_ | IMAP password (supports `${VAR}`)                                                                                         |
| `poll_interval_secs`        | `60`       | Polling interval when IDLE is unavailable                                                                                 |
| `rate_limit_per_second`     | `5`        | Max IMAP synchronization fetch/search requests per second across all mailboxes of this account                                                                                               |
| `mailboxes`                 | _required_ | List of mailbox names to monitor                                                                                          |
| `initial_sync_max_messages` | unlimited  | Cap on messages during first sync                                                                                         |
| `initial_sync_start_date`   | (none)     | Only download messages on or after this date (e.g. `"2024-01-01"`) during first sync; uses IMAP UID SEARCH SINCE         |
| `idle_drain_timeout_secs` | `5` | Time to drain IDLE DONE completion; increase for slow servers. |
| `idle_inactivity_timeout_secs` | `1800` | Maximum IDLE inactivity before forcing a sync; also caps the heartbeat interval. |
| `fetch_drain_timeout_millis` | `200` | Grace period for FETCH data sent after tagged completion (e.g. Proton Bridge). |
| `imap_command_timeout_secs` | `60`       | Timeout for individual IMAP command exchanges                                                                             |
| `tls_ca_file`               | —          | Path to a PEM file with extra CA cert(s) to trust (e.g. for local bridges with self-signed certs). Requires `tls = true`. |
| `tls_accept_invalid_certs`  | `false`    | Disable TLS certificate verification entirely. Only for local bridges on loopback. Requires `tls = true`.                 |
| `idle_heartbeat_interval_secs` | (none) | When IDLE is active, force a sync after this many seconds even if no server notification has arrived. Acts as a safety net for missed IDLE notifications. Should be larger than `poll_interval_secs`. Omit or set to null to disable. |

> **TLS mode note:** mailmux supports **implicit TLS** only (`tls = true` wraps
> the connection in TLS before any IMAP traffic, typically port 993). **STARTTLS**
> — where a plain-text connection is upgraded in-band via the `STARTTLS` command
> (typically port 143) — is not supported. If your mail server or local bridge
> (e.g. Proton Mail Bridge) offers a choice, select **SSL/TLS** rather than
> **STARTTLS**.

### Processor options

| Field                | Default    | Description                                                                        |
| -------------------- | ---------- | ---------------------------------------------------------------------------------- |
| `name`               | _required_ | Unique processor name                                                              |
| `enabled`            | `true`     | Whether the processor is active                                                    |
| `events`             | `[]`       | Event types to subscribe to. Currently `email_arrived` is the only supported type. |
| `sources`            | omitted    | Optional account/mailbox allowlist; omitted means all sources. Selectors use exact account IDs and mailbox names. |
| `max_retries`        | `0`        | Max retry attempts on failure                                                      |
| `retry_backoff_secs` | `[]`       | Backoff schedule (seconds per attempt)                                             |
| `timeout_secs`       | `30`       | Execution timeout                                                                  |
| `concurrency`        | `1`        | Max concurrent executions                                                          |
| `config`             | `{}`       | Processor-specific key/value config                                                |

Source selectors preserve account/mailbox pairing, for example:

```toml
sources = [
  { account = "personal", mailboxes = ["INBOX", "Archive"] },
  { account = "work", mailboxes = ["INBOX"] },
]
# Account-only: sources = [{ account = "personal" }]
```

Explicit empty source/mailbox lists, empty names, unknown account IDs, and
unknown selector fields are rejected. A configured disabled account remains a
valid historical source. Mailbox names not currently monitored produce startup
warnings rather than errors. Source strings support `${VAR}` substitution.

Current eligibility is enforced for dispatch, retries and pending jobs, replay,
event dry-run, and historical backfill. CLI selection (including backfill
`--all`) cannot bypass it. Filters take effect on restart. Broadening filters
does not automatically reroute already dispatched events; use explicit replay
or backfill for historical processing.

Dispatch completion is tracked in `events.dispatched_at`, independently of
processor-job rows. Events with no eligible processors are marked routed too,
so zero-match batches cannot starve later events. The migration marks existing
events that already have jobs as dispatched without changing those jobs;
existing events without jobs receive one routing pass after upgrade. Retention
keeps undispatched events and outstanding jobs, but may delete old dispatched
events with no jobs.

Built-in processor types:

- **`logger`** — logs event details via tracing
- **`command`** — executes a CLI command, passing event JSON on stdin
  (set `config.command` and optionally `config.args` and `config.env`)

## mailindex

`mailindex` owns bounded normalized mail content in local Turso and a rebuildable
Tantivy index. It exposes authenticated HTTP ingestion/search/retrieval and MCP
search/retrieval, plus `index-status` and offline `rebuild-index` maintenance
commands. See [`mailindex/README.md`](mailindex/README.md) and the example config.
The reference mailmux adapter uploads raw bytes at `mailindex/contrib/mailmux-submit.sh`.

## mailtx crate notes

`mailtx` is designed to be run from `mailmux`'s built-in `command` processor.
It reads event/email JSON from stdin, extracts bank transaction details via an
LLM, and posts the result to Firefly III — tagging every transaction for easy
identification.

- Crate path: `mailtx/`
- Detailed docs: `mailtx/README.md`

Configuration lives in a TOML file whose path is given by the `MAILTX_CONFIG`
environment variable. Provider API keys are inferred from the environment only
in `auto` mode. Custom OpenAI-compatible endpoints use an explicit `api_key_env`
or no authentication; see [mailtx custom endpoint configuration](mailtx/README.md#configuration).
For cold local models, allow enough time beyond the LLM deadline (for example,
180 seconds for LLM plus a 240-second mailmux processor timeout, concurrency 1).

Typical integration in `mailmux` config:

```toml
[[processors]]
name = "mailtx"
enabled = true
events = ["email_arrived"]
max_retries = 3
retry_backoff_secs = [30, 120, 600]
timeout_secs = 90
concurrency = 1

[processors.config]
command = "/usr/local/bin/mailtx"
env = { MAILTX_CONFIG = "/etc/mailtx/config.toml", ANTHROPIC_API_KEY = "${ANTHROPIC_API_KEY}" }
```

## Running

Start the daemon:

```bash
mailmux --config config.toml
# or from workspace root:
cargo run -p mailmux -- --config mailmux/config.toml
```

Override the log level:

```bash
mailmux --config config.toml --log-level debug
```

Stop gracefully with `Ctrl+C` (SIGINT) or `SIGTERM`. In-flight work gets a
configurable grace period before tasks are aborted.

### Subcommands

**Replay** — re-run all (or one) processor(s) for a specific event:

```bash
mailmux replay --event-id 42
mailmux replay --event-id 42 --processor notify
```

**Dry-run** — execute a processor against an event without persisting results:

```bash
mailmux dry-run --event-id 42 --processor notify
```

**Backfill** — run one configured processor against durable historical
emails. This operates directly on `emails` rows (and their raw `.eml`
files), so it works for mail whose `events` rows were already cleaned up by
retention:

```bash
mailmux backfill [OPTIONS] --processor <NAME>

Selection filters:
  --after <DATE_OR_TIME>      Include emails whose message date is >= this value
  --before <DATE_OR_TIME>     Include emails whose message date is < this value
  --account <ID>              Include an account; repeatable
  --mailbox <NAME>            Include a mailbox; repeatable
  --email-id <ID>             Include a specific emails.id; repeatable
  --all                       Explicitly allow selecting all stored emails

Execution controls:
  --limit <N>                 Stop after N selected emails
  --concurrency <N>           Override processor concurrency for this run
  --fail-fast                 Stop scheduling after the first permanently failed email
  --dry-run                   Count selected emails without invoking the processor
```

Behavior and safety:

- At least one selection filter is required unless `--all` is explicitly
  given, so a typo cannot silently run a side-effecting processor over the
  entire archive.
- Date filters accept `YYYY-MM-DD` (interpreted as midnight UTC) or an RFC
  3339 timestamp (normalized to UTC). `--after` is inclusive
  (`date >= after`), `--before` is exclusive (`date < before`), which makes
  year/month ranges straightforward. Emails with a NULL date never match date
  filters but can still be selected by account/mailbox/email-id.
- Repeated values of one filter kind are ORed; different filter kinds are
  combined with AND. `--after >= --before` is rejected.
- Emails are always processed in ascending `emails.id` order (deterministic
  ingestion order; message dates can be NULL or non-monotonic). `--limit`
  applies to that ordered selection.
- For each selected email, mailmux constructs a transient in-memory
  `email_arrived` event and invokes exactly the one selected processor
  through the normal `Processor` trait. **No `events` rows and no
  `processor_jobs` rows are created or modified.** The command-processor
  `{event, email}` JSON shape is unchanged, including
  `email.raw_message_path`; historical invocations are identifiable via
  `.event.payload.backfill == true` (the event `id` is `0`, a value reserved
  for non-persisted backfill events).
- Timeouts, `max_retries`, and `retry_backoff_secs` come from the selected
  processor's configuration; `--concurrency` overrides its `concurrency` for
  this run only. Concurrency is bounded, and processing order/counters remain
  deterministic.
  On Unix, timing out or cancelling a command processor terminates its process
  group, including shell-launched descendants; stdin writes share the timeout.
- A permanently failed email does not stop the batch unless `--fail-fast` is
  set (with `--fail-fast`, remaining selected emails are reported as
  `skipped`).
- The final summary is logged as `selected`, `processed`, `succeeded`,
  `failed`, `skipped`, and `elapsed`. Exit status is 0 for dry runs, no
  matches, and all-success runs, and non-zero when setup fails or at least
  one email permanently fails (the summary is printed first).
  A pagination error also prints the accumulated summary before exiting
  non-zero, with unprocessed selected emails counted as `skipped`.
- Backfill is safe to re-run (same command, same selection, same order), but
  mailmux does not track what a processor has already seen — **backfill
  processors SHOULD be idempotent**.

Examples:

```bash
# Re-submit all 2021 mail from one account to the mail-index processor
mailmux backfill \
  --processor mail-indexer \
  --account personal \
  --after 2021-01-01 \
  --before 2022-01-01

# Preview the selection first (counts matches, invokes nothing)
mailmux backfill \
  --processor mail-indexer \
  --account personal \
  --after 2021-01-01 \
  --before 2022-01-01 \
  --dry-run

# Re-process two specific emails with lower concurrency
mailmux backfill --processor mail-indexer --email-id 1042 --email-id 1043 --concurrency 1
```

## Docker

Build the image:

```bash
docker build -t mailmux .
```

Run with an existing PostgreSQL instance:

```bash
docker run --rm \
  -v $(pwd)/mailmux/config.toml:/etc/mailmux/config.toml:ro \
  -v mailmux-data:/var/lib/mailmux \
  -e DB_PASSWORD=secret \
  -e GMAIL_APP_PASSWORD=xxxx-xxxx-xxxx-xxxx \
  -p 8080:8080 \
  mailmux
```

### Docker Compose

A `docker-compose.yml` is provided at `mailmux/docker-compose.yml` and starts
both PostgreSQL and `mailmux`. Compose and release CI use the root Dockerfile
and workspace lockfile, with `mailmux`, `mailtx`, `mailindex`, and the
`mailmux-submit` adapter included in the same image.

1. Create `mailmux/config.toml` (see [mailmux configuration](#mailmux-configuration)).
   Use the Compose-internal hostname `postgres` for the database:

   ```toml
   [database]
   url = "postgres://mailmux:${DB_PASSWORD}@postgres:5432/mailmux"
   ```

2. Set IMAP credentials as environment variables in `docker-compose.yml` or in
   a `.env` file next to it:

   ```
   GMAIL_APP_PASSWORD=xxxx-xxxx-xxxx-xxxx
   ```

3. Start the stack:

   ```bash
   docker compose -f mailmux/docker-compose.yml up -d
   ```

4. View logs:

   ```bash
   docker compose -f mailmux/docker-compose.yml logs -f mailmux
   ```

5. Stop:

   ```bash
   docker compose -f mailmux/docker-compose.yml down
   ```

Data is persisted in named Docker volumes (`pgdata` for the database,
`mailmux-data` for raw `.eml` files). To start fresh, add `-v` when
tearing down: `docker compose -f mailmux/docker-compose.yml down -v`.

## Deployment with systemd

An example unit file is provided in `mailmux/contrib/mailmux.service`. Install it:

```bash
sudo cp mailmux/contrib/mailmux.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now mailmux
```

The service uses `Type=notify` — mailmux signals readiness to systemd after
connecting to the database and spawning all account watchers.

Environment variables for secrets can be placed in `/etc/mailmux/env`:

```
DB_PASSWORD=secret
GMAIL_APP_PASSWORD=xxxx-xxxx-xxxx-xxxx
```

Manage the service:

```bash
sudo systemctl status mailmux
sudo journalctl -u mailmux -f
```

## Monitoring

When `health_port` is set in the config:

| Endpoint       | Description                                      |
| -------------- | ------------------------------------------------ |
| `GET /health`  | Returns `200 OK` if the database is reachable    |
| `GET /ready`   | Returns `200 OK` after initial setup is complete |
| `GET /metrics` | Prometheus metrics in text exposition format     |

Exported metrics:

| Metric                            | Type    | Labels            |
| --------------------------------- | ------- | ----------------- |
| `mailmux_messages_ingested_total` | counter | account, mailbox  |
| `mailmux_events_created_total`    | counter | event_type        |
| `mailmux_processor_runs_total`    | counter | processor, status |
| `mailmux_active_connections`      | gauge   | account           |
| `mailmux_mailboxes_monitored`     | gauge   | account           |
| `mailmux_idle_heartbeat_catches_total` | counter | account, mailbox |

## License

[MIT](LICENSE)

Per-account `max_connections` was removed: every configured mailbox owns one
connection, and all mailboxes of an account share its synchronization request
budget. `database.max_connections` still controls the PostgreSQL pool. UID
responses are sorted defensively; missing UIDVALIDITY aborts sync, and a
checkpoint at the maximum UID waits for a new UIDVALIDITY generation. SELECT
message counts are included in diagnostics, without treating a changing
EXISTS count as a completeness guarantee.

### IMAP response limits

Each IMAP command accepts at most 1,000,000 result entries. Message FETCH
results also have a combined 128 MiB body/flag budget, checked before copying
message data. Individual wire responses are limited to 100 MiB by the IMAP
parser. Exceeding a limit logs a warning and fails the sync without returning
partial results; the watcher reconnects through its normal error handling.
Split very large mailboxes if their UID scan exceeds the entry limit.

IMAP timeout settings are per account and must be positive values no greater than 86,400,000 in their documented units. Defaults preserve the existing timing.

IMAP authentication failures use a typed, redacted error. Failure logs omit
account IDs, mailbox names, usernames, and server-provided rejection text, since
servers can echo credentials. Watchers stop after rejection; operators should
check their configured credentials.

### Rotating IMAP credentials without restarting

For accounts managed by a secret service, omit `username` and `password` and
configure an atomically replaced credential file:

```toml
[[accounts]]
id = "personal"
imap_host = "imap.example.com"
mailboxes = ["INBOX"]
credentials_file = "/run/secrets/personal-imap.toml"
credential_reload_interval_secs = 30
```

The secret file contains `username = "..."` and `password = "..."`. Restrict it
to mode 0600 or 0640. Mailmux reads it before every connection and compares
its contents at the configured interval, including while IDLE is active. A
valid change closes that mailbox's old session and authenticates a new one.
Rejected logins wait for a valid update instead of permanently stopping.
Temporary missing or malformed files keep an existing session alive. Changes
are limited to these IMAP credentials; database, processor, other configuration,
and legacy environment-sourced credential changes still require a restart.
Do not configure both a credential file and inline username/password fields.

The health server shares a 10-request/second token bucket with a burst of 20
across `/health`, `/ready`, and `/metrics`. Excess requests receive HTTP 429
and `Retry-After: 1` before database access or metrics rendering. The global
budget uses constant memory and applies across clients; it does not trust
forwarded IP headers. Monitor `mailmux_health_rate_limited_total` to detect
excessive scrape/probe traffic.

### Event header limits

Ingested subjects are capped at 4096 UTF-8 bytes without splitting a character.
Sender addresses over 1024 bytes are omitted from metadata and event payloads;
partial sender addresses are never used for processor matching. The raw `.eml`
file retains the complete message. With the UID field, the generated
`email_arrived` payload stays below 31 KiB even with JSON escaping. Each
truncation or omission emits a warning without header contents and increments
`mailmux_oversized_headers_total{header="subject"|"sender"}`.

### Health listener exposure

The health server binds to `127.0.0.1` by default. Set
`general.health_bind_addr` to an explicit IP address for network access (for
example `0.0.0.0` inside Docker), and use a firewall or private network.
Set `general.health_metrics_token_env = "MAILMUX_METRICS_TOKEN"` to require
`Authorization: Bearer <token>` on `/metrics`; the secret comes only from that
environment variable. Missing or empty configured secrets fail startup.
`/health` and `/ready` remain available to local health checks. External binds
without metrics authentication emit a startup warning.

### UIDVALIDITY and archived messages

Email identity includes the account, mailbox, UIDVALIDITY generation, and UID.
New raw messages are stored at `{data_dir}/{account}/{mailbox}/uv_{uidvalidity}/{uid}.eml`.
When UIDVALIDITY changes, the mailbox is scanned again; previous emails, raw
files, events, and processor jobs remain available under their original IDs.
The migration assigns existing emails their mailbox state's UIDVALIDITY and
keeps their existing raw file paths. Emails without a mailbox state retain
generation 0 (unknown) and may be imported again on the next sync.

### Processor crash recovery

Scheduler and replay claims carry an execution lease lasting the processor
timeout plus 30 seconds for persisting the result. The 10-second retry sweep
requeues expired claims and legacy in-progress jobs without leases; pending
jobs are also recovered. A monotonically increasing claim version prevents
a late result from overwriting a newer attempt. Recovery can repeat external
side effects after a crash, so processors still need downstream idempotency.
Stop old daemon versions before upgrading: they do not participate in leases.

### Processor concurrency

`concurrency` must be positive and bounds active executions per processor across
both new dispatches and retries. Slow processors do not block other processors
or the scheduler's retry timer. The scheduler allows at most 16 batch workers
in memory; additional dispatches apply channel backpressure, while pending jobs
remain durable in PostgreSQL. Shutdown drains active work within the daemon's
configured grace period.

The mailtx bank processor requires a configured list of receiving
`trusted_authserv_ids` and a trusted aligned DMARC pass before processing mail.
See [sender authentication](mailtx/README.md#sender-authentication) for the
receiving-provider requirements and migration guidance.

### Command processor environment

Commands start with a cleared environment and receive only
`PATH=/usr/local/bin:/usr/bin:/bin`, `LANG=C.UTF-8`, and the explicit
`processors.config.env` map (which can override these defaults). Windows also
passes `SystemRoot`. Pass each processor's required credentials explicitly,
using `${VAR}` references so secret values stay in the daemon's environment
file. Database and IMAP credentials are not inherited.

```toml
[processors.config.env]
MAILTX_CONFIG = "/etc/mailtx/config.toml"
ANTHROPIC_API_KEY = "${ANTHROPIC_API_KEY}"
```

When using `mailmux-submit`, explicitly pass `MAILINDEX_URL` and
`MAILINDEX_API_TOKEN` in that processor's environment map.

Each mailbox watcher and account manager has an independent restart loop.
Errors, panics, and unexpected clean exits trigger a five-second restart delay.
Ten failures within five minutes trigger a 15-minute cooldown for that task.
A failed watcher restarts without interrupting healthy peer mailboxes; shutdown
cancels restart delays and cooldowns.

IMAP retry decisions and sync-failure metrics use typed errors for authentication,
deadlines, transport, protocol, and unsupported IDLE. Error context and server
message wording do not determine retry behavior or metric categories. Only an
explicit IDLE rejection selects polling; other failures reconnect.

Plaintext IMAP (`tls = false`) to a non-loopback host emits a startup warning.
Missing `${VAR}` substitutions also emit warnings naming only the unresolved
variable; the reference remains unchanged so existing optional/disabled
configuration can still load.
