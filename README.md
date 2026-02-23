# bank-tx-processor

A mailmux command processor that extracts structured transaction data from bank
notification emails and posts it to a configurable HTTP endpoint.

For each incoming email it:

1. Checks the sender against a configured allow-list — skips silently if not matched
2. Reads the raw `.eml` file and extracts a plain-text body (strips HTML if needed)
3. Sends the subject and body to the Anthropic Claude API with a structured prompt
4. If Claude identifies a bank transaction, posts the extracted data (amount,
   type, narration) to a configured HTTP endpoint

## Design

The processor is implemented as a standalone Rust binary that plugs into
mailmux's `command` processor type. mailmux spawns it once per event, writes a
JSON payload to stdin, and interprets the exit code:

- **Exit 0** — success, or deliberate skip (sender not in allow-list, LLM found
  no transaction data); mailmux marks the job as completed
- **Exit non-0** — error; mailmux marks the job as failed and retries according
  to the processor's `retry_backoff_secs` schedule

Log output goes to **stderr** so it does not interfere with the stdout channel
that mailmux reads.

### stdin schema

mailmux writes a JSON object to stdin:

```json
{
  "event": {
    "id": 42,
    "event_type": "email_arrived",
    ...
  },
  "email": {
    "id": 7,
    "subject": "Your transaction of INR 1,234.56",
    "sender": "alerts@mybank.com",
    "raw_message_path": "/var/lib/mailmux/personal/INBOX/12345.eml",
    ...
  }
}
```

Only the fields used by this processor are declared in `src/input.rs`; serde
ignores the rest.

### Module layout

| File | Responsibility |
|---|---|
| `src/main.rs` | Reads stdin, orchestrates the pipeline, owns exit code |
| `src/config.rs` | Loads configuration from environment variables |
| `src/input.rs` | Serde types mirroring the mailmux stdin schema |
| `src/email.rs` | Reads `.eml` file, extracts plain-text body (HTML stripping) |
| `src/llm.rs` | Anthropic Messages API call and JSON response parsing |
| `src/post.rs` | HTTP POST to the configured endpoint |

### LLM prompt

The prompt asks Claude to return a JSON object with four fields:

```json
{
  "status": "found",
  "amount": 1234.56,
  "transaction_type": "withdrawal",
  "narration": "Amazon Pay"
}
```

`status` is `"found"` when the email is a bank transaction notification with a
monetary amount, `"not_found"` otherwise. Processing stops without error when
`status` is `"not_found"`.

### Endpoint payload

```json
{
  "amount": 1234.56,
  "transaction_type": "withdrawal",
  "narration": "Amazon Pay"
}
```

Sent as `Content-Type: application/json` with an `Authorization` header whose
value is taken directly from `ENDPOINT_AUTH`.

## Prerequisites

- Rust 1.80+ (edition 2024) — for `std::sync::LazyLock`
- An Anthropic API key
- A running mailmux instance

## Build

```bash
cargo build --release
```

The binary is written to `target/release/bank-tx-processor`.

## Configuration

All configuration is via environment variables. Secrets should be injected at
runtime rather than stored in the mailmux config file.

| Variable | Required | Description |
|---|---|---|
| `ALLOWED_SENDERS` | yes | Comma-separated list of sender addresses or substrings to accept, e.g. `alerts@mybank.com,noreply@anotherbank.com` |
| `ANTHROPIC_API_KEY` | yes | Anthropic API key |
| `ENDPOINT_URL` | yes | URL to POST extracted transaction data to |
| `ENDPOINT_AUTH` | yes | Full value for the `Authorization` header, e.g. `Bearer eyJ...` |
| `ANTHROPIC_MODEL` | no | Claude model ID (default: `claude-haiku-4-5-20251001`) |
| `RUST_LOG` | no | Log level filter, e.g. `debug` or `bank_tx_processor=debug` |

### Sender matching

`ALLOWED_SENDERS` entries are matched as **case-insensitive substrings** of the
full sender field, which may be in `"Display Name <email@domain.com>"` format.
Matching on the bare email address (e.g. `alerts@mybank.com`) is sufficient and
is the recommended approach.

## mailmux integration

### 1. Install the binary

```bash
cargo build --release
sudo cp target/release/bank-tx-processor /usr/local/bin/
```

### 2. Add a processor block to your mailmux config

```toml
[[processors]]
name = "bank-tx"
enabled = true
events = ["email_arrived"]
max_retries = 3
retry_backoff_secs = [30, 120, 600]
timeout_secs = 90
concurrency = 1

[processors.config]
command = "/usr/local/bin/bank-tx-processor"
```

**`timeout_secs`** must be generous enough to cover an Anthropic API round-trip
(typically 2–10 s) plus the downstream HTTP POST. 90 s is a safe default.

**`concurrency = 1`** is appropriate for low-volume bank notification emails.
Increase it only if you observe a queue backlog.

### 3. Set environment variables

The variables must be present in the environment that runs mailmux, since the
command processor inherits mailmux's environment.

#### systemd

Add an `EnvironmentFile` directive to your mailmux service unit, or extend the
existing one:

```ini
[Service]
EnvironmentFile=/etc/mailmux/env
```

`/etc/mailmux/env`:

```
ALLOWED_SENDERS=alerts@mybank.com,noreply@anotherbank.com
ANTHROPIC_API_KEY=sk-ant-...
ENDPOINT_URL=https://your-api.example.com/transactions
ENDPOINT_AUTH=Bearer eyJ...
```

Reload after changes:

```bash
sudo systemctl daemon-reload
sudo systemctl restart mailmux
```

#### Docker Compose

Add to the `environment` section of the mailmux service:

```yaml
environment:
  ALLOWED_SENDERS: alerts@mybank.com
  ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
  ENDPOINT_URL: ${ENDPOINT_URL}
  ENDPOINT_AUTH: ${ENDPOINT_AUTH}
```

And set the values in your `.env` file alongside `docker-compose.yml`.

### 4. Test with mailmux dry-run

Once mailmux has ingested at least one bank email, test the processor without
persisting results:

```bash
mailmux dry-run --event-id <id> --processor bank-tx
```

Find a recent event ID:

```sql
SELECT id, payload->>'subject' AS subject, created_at
FROM events
ORDER BY created_at DESC
LIMIT 10;
```

### 5. Replay for missed emails

To retroactively process emails that arrived before the processor was configured:

```bash
mailmux replay --event-id <id> --processor bank-tx
```

## Retry behaviour

mailmux retries the entire invocation on non-zero exit. This means a retry
re-runs the LLM call even if it succeeded the first time. The downstream
endpoint should therefore be **idempotent** — posting the same transaction twice
should be a no-op or produce a meaningful error that the endpoint handles
gracefully.

## Logging

Logs are written to stderr in the default tracing compact format. To enable
debug logging when running under mailmux, set `RUST_LOG=bank_tx_processor=debug`
in the mailmux environment.

To inspect logs when running standalone:

```bash
echo '{"event":{"id":1},"email":{"subject":"Test","sender":"alerts@mybank.com","raw_message_path":"/tmp/test.eml"}}' \
  | RUST_LOG=debug \
    ALLOWED_SENDERS=alerts@mybank.com \
    ANTHROPIC_API_KEY=sk-ant-... \
    ENDPOINT_URL=https://... \
    ENDPOINT_AUTH="Bearer ..." \
    ./target/debug/bank-tx-processor
```
