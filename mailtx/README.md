# mailtx

A mailmux command processor that extracts structured transaction data from bank
notification emails and posts it to Firefly III.

For each incoming email it:

1. Checks the sender against a configured allow-list — skips silently if not matched
2. Reads the raw `.eml` file and extracts a plain-text body (strips HTML if needed)
3. Sends the subject and body to the Anthropic Claude API with a structured prompt
4. If Claude identifies a bank transaction, maps it to Firefly's transaction
   schema and posts it to Firefly III

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

| File            | Responsibility                                               |
| --------------- | ------------------------------------------------------------ |
| `src/main.rs`   | Reads stdin, orchestrates the pipeline, owns exit code       |
| `src/config.rs` | Loads configuration from a TOML file (`MAILTX_CONFIG`)       |
| `src/input.rs`  | Serde types mirroring the mailmux stdin schema               |
| `src/email.rs`  | Reads `.eml` file, extracts plain-text body (HTML stripping) |
| `src/llm.rs`    | Anthropic Messages API call and JSON response parsing        |
| `src/endpoint/` | Endpoint abstraction and Firefly III implementation          |

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

### Firefly request payload

```json
{
  "apply_rules": false,
  "fire_webhooks": true,
  "error_if_duplicate_hash": false,
  "transactions": [
    {
      "type": "withdrawal",
      "date": "2026-02-26T20:00:00+00:00",
      "amount": "1234.56",
      "description": "Amazon Pay",
      "source_id": "12",
      "destination_name": "Amazon Pay"
    }
  ]
}
```

Posted to `POST /v1/transactions` under your Firefly API base URL with
`Authorization: Bearer <token>`.

## Prerequisites

- Rust 1.80+ (edition 2024) — for `std::sync::LazyLock`
- An Anthropic API key
- A running mailmux instance

## Build

```bash
cargo build --release
```

The binary is written to `target/release/mailtx`.

## Configuration

Most configuration lives in a TOML file. Point `MAILTX_CONFIG` at it:

```
MAILTX_CONFIG=/etc/mailtx/config.toml
```

LLM API keys are read from the environment by the `genai` crate — they do not
go in the TOML file.

### Environment variables

| Variable            | Required           | Description                                                   |
| ------------------- | ------------------ | ------------------------------------------------------------- |
| `MAILTX_CONFIG`     | yes                | Path to the TOML config file                                  |
| `ANTHROPIC_API_KEY` | provider-dependent | Required for any `claude-` model                              |
| `OPENAI_API_KEY`    | provider-dependent | Required for any `gpt-` / `o1-` / `o3-` model                |
| `GEMINI_API_KEY`    | provider-dependent | Required for any `gemini-` model                              |
| `RUST_LOG`          | no                 | Log level filter, e.g. `debug`                                |

### TOML config file

```toml
# Senders matched as case-insensitive substrings of the From header.
allowed_senders = ["alerts@mybank.com", "noreply@anotherbank.com"]

# Model name passed to genai. Provider (and required API key) is inferred from
# the name. Default: "claude-haiku-4-5-20251001"
llm_model = "claude-haiku-4-5-20251001"

[firefly]
base_url     = "https://firefly.example.com/api"
access_token = "eyJ..."

# Optional — shown with defaults:
# default_asset_account_id = "12"
# currency_code            = "USD"
# apply_rules              = false
# fire_webhooks            = true
# error_if_duplicate_hash  = false

[[firefly.asset_accounts]]
id                 = "hdfc_9772"
firefly_account_id = "12"
account_suffixes   = ["9772"]
debit_card_last4   = ["7406"]
aliases            = ["hdfc salary account"]

[[firefly.asset_accounts]]
id                 = "sbm_3989"
firefly_account_id = "42"
account_suffixes   = ["3989"]
debit_card_last4   = []
aliases            = ["sbm bank"]
```

The LLM provider is inferred automatically from the model name by `genai`:

| Provider       | Example model               | API key env var     |
| -------------- | --------------------------- | ------------------- |
| Anthropic      | `claude-haiku-4-5-20251001` | `ANTHROPIC_API_KEY` |
| OpenAI         | `gpt-4o-mini`               | `OPENAI_API_KEY`    |
| Google Gemini  | `gemini-2.0-flash`          | `GEMINI_API_KEY`    |
| Groq           | `llama-3.1-8b-instant`      | `GROQ_API_KEY`      |
| Ollama (local) | `llama3.2`                  | _(no key required)_ |

### Sender matching

`allowed_senders` entries are matched as **case-insensitive substrings** of the
full sender field, which may be in `"Display Name <email@domain.com>"` format.
Matching on the bare email address (e.g. `alerts@mybank.com`) is sufficient and
is the recommended approach.

### Asset account matching

The processor resolves the Firefly asset account from email content using a deterministic matcher:

1. Debit card last4 (highest priority)
2. Account suffix in account mentions
3. Alias substring match
4. Optional default fallback (`default_asset_account_id`)

## mailmux integration

### 1. Install the binary

```bash
cargo build --release
sudo cp target/release/mailtx /usr/local/bin/
```

### 2. Add a processor block to your mailmux config

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
```

**`timeout_secs`** must be generous enough to cover an LLM API round-trip
(typically 2–10 s) plus the downstream HTTP POST. 90 s is a safe default.

**`concurrency = 1`** is appropriate for low-volume bank notification emails.
Increase it only if you observe a queue backlog.

### 3. Create a config file and set environment variables

Write your config file (e.g. `/etc/mailtx/config.toml`) — see the
[Configuration](#configuration) section for the full schema.

The env vars must be present in the environment that runs mailmux, since the
command processor inherits mailmux's environment.

#### systemd

Add an `EnvironmentFile` directive to your mailmux service unit:

```ini
[Service]
EnvironmentFile=/etc/mailmux/env
```

`/etc/mailmux/env`:

```
MAILTX_CONFIG=/etc/mailtx/config.toml
ANTHROPIC_API_KEY=sk-ant-...
```

Reload after changes:

```bash
sudo systemctl daemon-reload
sudo systemctl restart mailmux
```

#### Docker Compose

Mount the config file into the container and set env vars:

```yaml
environment:
  MAILTX_CONFIG: /etc/mailtx/config.toml
  ANTHROPIC_API_KEY: ${ANTHROPIC_API_KEY}
volumes:
  - ./mailtx.toml:/etc/mailtx/config.toml:ro
```

### 4. Test with mailmux dry-run

Once mailmux has ingested at least one bank email, test the processor without
persisting results:

```bash
mailmux dry-run --event-id <id> --processor mailtx
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
mailmux replay --event-id <id> --processor mailtx
```

## Retry behaviour

mailmux retries the entire invocation on non-zero exit. This means a retry
re-runs the LLM call even if it succeeded the first time. Firefly processing
should therefore be configured to tolerate retries (for example by enabling
`error_if_duplicate_hash = true` in the TOML config when duplicate hashes are available).

## Logging

Logs are written to stderr in the default tracing compact format. To enable
debug logging when running under mailmux, set `RUST_LOG=mailtx_processor=debug`
in the mailmux environment.

To inspect logs when running standalone:

```bash
echo '{"event":{"id":1},"email":{"subject":"Test","sender":"alerts@mybank.com","raw_message_path":"/tmp/test.eml"}}' \
  | RUST_LOG=debug \
    MAILTX_CONFIG=/path/to/mailtx.toml \
    ANTHROPIC_API_KEY=sk-ant-... \
    ./target/debug/mailtx
```
