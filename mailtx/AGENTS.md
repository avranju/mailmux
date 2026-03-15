# AGENTS.md

This file provides guidance to AI coding agents working with code in this
repository.

## What This Project Is

**mailtx** is a mailmux command processor binary. It is invoked by
mailmux once per `email_arrived` event, reads a JSON payload from stdin,
extracts bank transaction data from the email using the Anthropic Claude API,
and posts the result to a configured HTTP endpoint.

It is a **standalone Rust binary** — it has no shared code with mailmux and
communicates with it only through stdin (JSON) and exit code (0 = success/skip,
non-0 = retriable error).

## Build & Run Commands

```bash
# Build (debug)
cargo build

# Build (release)
cargo build --release

# Run tests
cargo test

# Lint
cargo clippy

# Run standalone (pipe JSON on stdin)
echo '{"event":{"id":1},"email":{"subject":"Txn alert","sender":"alerts@mybank.com","raw_message_path":"/tmp/test.eml"}}' \
  | MAILTX_CONFIG=/path/to/mailtx.toml \
    ANTHROPIC_API_KEY=sk-ant-... \
    ./target/debug/mailtx
```

No Makefile — everything goes through standard Cargo.

## Architecture

Single async pipeline in `main.rs`:

```
stdin JSON
  → parse Input { event, email }
  → check email.sender against ALLOWED_SENDERS        (skip → exit 0)
  → read raw_message_path from disk
  → extract plain-text body (text/plain preferred, HTML stripped if needed)
  → call Anthropic Messages API with structured prompt
  → if status = "not_found" → skip → exit 0
  → HTTP POST { amount, transaction_type, narration } to ENDPOINT_URL
  → exit 0 on success, exit 1 on any error
```

## Key Modules (`src/`)

| File        | Role                                                                                                              |
| ----------- | ----------------------------------------------------------------------------------------------------------------- |
| `main.rs`   | Orchestrates the pipeline; owns stdin reading and exit code                                                       |
| `config.rs` | Loads config from a TOML file (`MAILTX_CONFIG`); `sender_allowed()` does substring match                         |
| `input.rs`  | Minimal serde types mirroring mailmux's stdin schema; only used fields are declared                               |
| `email.rs`  | Reads `.eml` file with `mail-parser`; prefers text/plain, strips HTML via regex if only text/html is available    |
| `llm.rs`    | Anthropic Messages API (`POST /v1/messages`); parses JSON response; strips markdown code fences from model output |
| `post.rs`   | HTTP POST to `ENDPOINT_URL` with `Authorization` header                                                           |

## Configuration

Configuration is split between a TOML file (most settings) and a few env vars
(secrets and runtime knobs that are better kept out of files).

### Environment variables

| Variable            | Required           | Notes                                                                           |
| ------------------- | ------------------ | ------------------------------------------------------------------------------- |
| `MAILTX_CONFIG`     | yes                | Path to the TOML config file                                                    |
| `ANTHROPIC_API_KEY` | provider-dependent | Required when using any `claude-` model; read by `genai`, not by our code       |
| `OPENAI_API_KEY`    | provider-dependent | Required when using any `gpt-` or `o1-`/`o3-` model                            |
| `GEMINI_API_KEY`    | provider-dependent | Required when using any `gemini-` model                                         |
| `RUST_LOG`          | no                 | Standard tracing env filter                                                     |

### TOML config file

```toml
# Senders matched as case-insensitive substrings of the From header.
allowed_senders = ["alerts@mybank.com", "noreply@anotherbank.com"]

# Model name passed to genai. Provider is inferred from the name.
# Default: "claude-haiku-4-5-20251001"
llm_model = "claude-haiku-4-5-20251001"
# tag = "mailmux-mailtx"

[firefly]
base_url     = "https://firefly.example.com/api"
access_token = "eyJ..."

# Optional Firefly settings (shown with their defaults):
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
```

## mailmux Integration

mailmux config block:

```toml
[[processors]]
name = "mailtx
enabled = true
events = ["email_arrived"]
max_retries = 3
retry_backoff_secs = [30, 120, 600]
timeout_secs = 90
concurrency = 1

[processors.config]
command = "/usr/local/bin/mailtx"
```

The binary inherits mailmux's environment, so all env vars must be set in the
environment that starts mailmux (systemd `EnvironmentFile`, Docker Compose
`environment`, etc.).

## Important Design Constraints

- **stdout is reserved for mailmux.** All logging must go to stderr. The
  `tracing_subscriber` is initialised with `.with_writer(std::io::stderr)`.
- **Exit code is the success signal.** Exit 0 covers both success and
  deliberate skips (sender not in list, LLM found no transaction). Exit non-0
  triggers mailmux retry.
- **Retries replay the full pipeline.** The LLM call is repeated on retry, so
  the downstream endpoint must be idempotent.
- **State is mostly stateless.** Normal transaction processing uses only stdin
  and env vars. When `transfer_rules` are configured, a local SQLite database
  (`state_db` in TOML) is used to persist pending transfer legs between
  invocations via `rusqlite` (bundled — no system library required). Do not add
  `sqlx` or any server-backed database dependency.
- **HTML-to-text conversion uses `html2text`** (`src/email.rs`), which is built
  on `html5ever` (the Servo HTML parser). It handles malformed HTML gracefully
  and is spec-compliant. The `width` parameter (120) controls line wrapping and
  has no effect on LLM input quality.
- **LLM output may include markdown fences.** `llm.rs` strips ` ```json ` /
  ` ``` ` wrappers before parsing the JSON response.

## Adding or Changing Behaviour

**To add a new extracted field** (e.g. account number):

1. Add the field to `TransactionData` in `src/llm.rs`
2. Update the prompt string in `PROMPT_TEMPLATE`
3. Update `TransactionPayload` in `src/post.rs` if the field should be posted

**To change the LLM model**, update `llm_model` in the TOML config file — no
code change needed. `genai` infers the provider from the model name and reads
the relevant API key env var automatically.

**To change the endpoint payload shape**, edit `TransactionPayload` in
`src/post.rs`.

**To add a new config variable**, add it to the appropriate struct in
`src/config.rs` (derive `Deserialize`), document it in the TOML example in
README.md and this file.

## Tests

No tests currently exist. When adding tests:

- Unit-test `config::Config::sender_allowed` with edge cases (full "Name
  <email>" format, case differences, partial substrings)
- Unit-test `email::html_to_text` with samples of real bank notification HTML
- Integration-test the LLM and post modules with a mock HTTP server (e.g.
  `wiremock`)
- Async tests use `#[tokio::test]`
