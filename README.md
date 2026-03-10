# mailmux workspace

This repository is a Rust workspace with:

- `mailmux`: an event-driven IMAP email processing daemon
- `mailtx`: a command-processor companion crate for extracting bank transaction
  data from emails and posting it to an HTTP endpoint

`mailmux` synchronizes emails from IMAP servers into PostgreSQL and triggers
configurable processing pipelines for each incoming message.

The core value of `mailmux` is **reliable, replayable, event-driven mail processing** with
emphasis on data integrity, recoverability, and idempotency.

## Workspace layout

```text
.
├── Cargo.toml                  # workspace manifest
├── README.md                   # workspace overview (this file)
├── mailmux/                    # daemon crate
│   ├── config.example.toml
│   ├── Dockerfile
│   ├── docker-compose.yml
│   ├── migrations/
│   └── src/
├── mailtx/                     # command processor crate
│   ├── README.md
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
| `processor_jobs` | Processing state per event/processor pair                   |

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
max_connections = 2
mailboxes = ["INBOX"]
initial_sync_max_messages = 1000

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
max_connections = 2
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
| `rate_limit_per_second`     | `5`        | Max IMAP fetches per second                                                                                               |
| `max_connections`           | `2`        | Connection pool size for this account                                                                                     |
| `mailboxes`                 | _required_ | List of mailbox names to monitor                                                                                          |
| `initial_sync_max_messages` | unlimited  | Cap on messages during first sync                                                                                         |
| `initial_sync_start_date`   | (none)     | Only download messages on or after this date (e.g. `"2024-01-01"`) during first sync; uses IMAP UID SEARCH SINCE         |
| `imap_command_timeout_secs` | `60`       | Timeout for individual IMAP command exchanges                                                                             |
| `tls_ca_file`               | —          | Path to a PEM file with extra CA cert(s) to trust (e.g. for local bridges with self-signed certs). Requires `tls = true`. |
| `tls_accept_invalid_certs`  | `false`    | Disable TLS certificate verification entirely. Only for local bridges on loopback. Requires `tls = true`.                 |

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
| `max_retries`        | `0`        | Max retry attempts on failure                                                      |
| `retry_backoff_secs` | `[]`       | Backoff schedule (seconds per attempt)                                             |
| `timeout_secs`       | `30`       | Execution timeout                                                                  |
| `concurrency`        | `1`        | Max concurrent executions                                                          |
| `config`             | `{}`       | Processor-specific key/value config                                                |

Built-in processor types:

- **`logger`** — logs event details via tracing
- **`command`** — executes a CLI command, passing event JSON on stdin
  (set `config.command` and optionally `config.args` and `config.env`)

## mailtx crate notes

`mailtx` is designed to be run from `mailmux`'s built-in `command` processor.
It reads event/email JSON from stdin, extracts bank transaction details via an
LLM, and posts the result to Firefly III — tagging every transaction for easy
identification.

- Crate path: `mailtx/`
- Detailed docs: `mailtx/README.md`

Configuration lives in a TOML file whose path is given by the `MAILTX_CONFIG`
environment variable. LLM API keys (`ANTHROPIC_API_KEY`, etc.) are read from
the environment by the `genai` crate. These can be supplied either via
`config.env` in the processor block (added on top of the inherited environment)
or by setting them in the environment that starts mailmux (systemd
`EnvironmentFile`, Docker Compose `environment`, etc.).

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

## Docker

Build the image:

```bash
docker build -t mailmux -f mailmux/Dockerfile mailmux
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
both PostgreSQL and `mailmux`.

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

## License

[MIT](LICENSE)
