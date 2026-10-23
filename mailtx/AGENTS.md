# AGENTS.md

This file provides guidance to AI coding agents working with code in this
repository.

## What This Project Is

**mailtx** is a mailmux command processor binary. It is invoked by
mailmux once per `email_arrived` event, reads a JSON payload from stdin,
extracts bank transaction data from the email using either genai provider routing or an explicitly configured OpenAI-compatible Chat Completions endpoint,
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
  → parse Input and stable external identity
  → exact normalized sender allow-list
  → authenticate raw email using trusted receiver DMARC results
  → consult durable review queue (held replays make no API calls)
  → fetch/cache categories; run extraction
  → resolve account, native currency, and booking timezone
  → validate required fields, date, amount/currency evidence; independent extraction
  → build canonical transaction with configurable and automatic tags
  → observe/check closing balance; hold suspicious candidates in SQLite
  → normalize display narration; transfer rules use original narration
  → flush expired accepted legs; match/store transfer or POST normal transaction
  → emit ProcessorOutput JSON on stdout, logs on stderr

```

## Key Modules (`src/`)

| File        | Role                                                                                                              |
| ----------- | ----------------------------------------------------------------------------------------------------------------- |
| `main.rs`   | Orchestrates the pipeline; owns stdin reading and exit code                                                       |
| `config.rs` | Loads config from a TOML file (`MAILTX_CONFIG`); `sender_allowed()` does exact normalized mailbox matching                         |
| `input.rs`  | Minimal serde types mirroring mailmux's stdin schema; only used fields are declared                               |
| `email.rs`  | Reads `.eml` file with `mail-parser`; prefers text/plain, converts HTML with html2text if only text/html is available    |
| `llm.rs`    | LlmClient dispatch, shared prompt/schema, and strict transaction validation |
| `llm/openai_compatible.rs` | Dedicated custom Chat Completions transport with explicit auth and bounded responses |
| `endpoint/` | Firefly and endpoint HTTP operations                                                           |

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
# Senders matched as exact normalized mailbox addresses.
allowed_senders = ["alerts@mybank.com", "noreply@anotherbank.com"]
state_db = "/var/lib/mailtx/state.sqlite" # durable validation review queue

# Required: authentication service IDs stamped by your receiving provider.
trusted_authserv_ids = ["mx.receiver.example"]

# Model name passed to genai. Provider is inferred from the name.
# Default: "claude-haiku-4-5-20251001"
llm_model = "claude-haiku-4-5-20251001"
# tag = "mailmux-mailtx"

[firefly]
base_url     = "https://firefly.example.com/api"
currency_code = "INR" # set the native currency; per-account overrides supported

access_token = "${FIREFLY_ACCESS_TOKEN}"

# Optional examples (account ID/currency have no default):
# default_asset_account_id = "12"
# currency_code            = "USD"
# apply_rules              = false
# fire_webhooks            = true
# error_if_duplicate_hash  = true
# allow_insecure_http      = false
```

> **Warning:** Firefly access tokens and transaction data must never be sent
> over plaintext HTTP transport.  `allow_insecure_http = true` is provided only
> for loopback-only local testing (localhost, 127.0.0.0/8, ::1).  Real Firefly
> servers must use HTTPS.

```toml
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
name = "mailtx"
enabled = true
events = ["email_arrived"]
max_retries = 3
retry_backoff_secs = [30, 120, 600]
timeout_secs = 360 # allow two default 60s LLM calls plus Firefly requests
concurrency = 1

[processors.config]
command = "/usr/local/bin/mailtx"
[processors.config.env]
MAILTX_CONFIG = "/etc/mailtx/config.toml"
FIREFLY_ACCESS_TOKEN = "${FIREFLY_ACCESS_TOKEN}"
ANTHROPIC_API_KEY = "${ANTHROPIC_API_KEY}"
```

Command processors receive a cleared environment plus PATH/LANG and their
explicit `processors.config.env` map. Set secrets in the environment that starts
mailmux, then forward only required variables via `${VAR}` entries in that map.
For mailtx, pass MAILTX_CONFIG and its chosen provider/custom backend key.

## Important Design Constraints

- **stdout is reserved for mailmux.** All logging must go to stderr. The
  `tracing_subscriber` is initialised with `.with_writer(std::io::stderr)`.
- **Exit code is the success signal.** Exit 0 covers posts, skips, and durable
  held_for_review outcomes. Check stdout metadata to distinguish them. Nonzero
  retries the pipeline. The separate reconcile command exits 2 for drift or an
  incomplete scan and emits a reconciliation report rather than ProcessorOutput.
- **Retries replay the full pipeline.** The LLM call is repeated on retry, so
  the downstream endpoint must be idempotent.
- **State uses local SQLite.** Default validation requires state_db for held
  reviews and bank observations. Pending transfers share that database. A private
  TTL-bounded category cache can use a separate SQLite path. Use rusqlite; do not
  add sqlx or a server-backed database dependency.
- **HTML-to-text conversion uses `html2text`** (`src/email.rs`), which is built
  on `html5ever` (the Servo HTML parser). It handles malformed HTML gracefully
  and is spec-compliant. The `width` parameter (120) controls line wrapping and
  has no effect on LLM input quality.
- **LLM outputs are untrusted.** Shared parsing accepts bare JSON or one enclosing Markdown fence and validates status and business fields without exposing model output in errors.
- **Configuration is resolved before pipeline side effects.** `Config::resolve_llm` validates backend and endpoint settings; custom credentials are read only from an explicitly named `api_key_env`.
- **Timeouts are coordinated.** LLM calls have bounded connection and total deadlines; configure mailmux's outer timeout longer than two LLM deadlines plus Firefly work when double extraction is enabled.

## Adding or Changing Behaviour

**To add a new extracted field** (e.g. account number):

1. Add the field to `TransactionData` in `src/llm.rs`
2. Update `build_prompt` and `transaction_schema` in `src/llm.rs`
3. Update the canonical Firefly transaction payload in `src/endpoint/firefly.rs` if the field should be posted

**To change the LLM model**, update `llm_model` in the TOML config file — no
code change needed. `genai` infers the provider from the model name and reads
the relevant API key env var automatically.

**To change the Firefly endpoint payload shape**, edit the payload types in
`src/endpoint/firefly.rs`.

**To add a new config variable**, add it to the appropriate struct in
`src/config.rs` (derive `Deserialize`), document it in the TOML example in
README.md and this file.

## Tests

Tests exist for sender normalization, Firefly behavior, transfer state, and pipeline helpers. When adding tests:

- Unit-test `config::Config::sender_allowed` with edge cases (full "Name
  <email>" format, case differences, and non-exact mailbox addresses)
- Unit-test `email::html_to_text` with samples of real bank notification HTML
- Integration-test the LLM and post modules with a mock HTTP server (e.g.
  `wiremock`)
- Async tests use `#[tokio::test]`

Transaction amounts are rust_decimal::Decimal, parsed from original JSON numeric
text without f64 conversion. Integer minor-unit conversions are checked and
sub-cent amounts are rejected. Bank date-only timestamps use transaction_timezone
(default UTC), overridable per asset-account timezone, and preserve fixed offsets
through pending transfer state. Category cache state is private, credential-scoped,
TTL-bounded SQLite; FetchedCategories reports real request counts for metrics.

Asset accounts may set currency_code (alias currency), overriding firefly.currency_code.
card_last4 aliases debit_card_last4 and accepts credit-card notifications too.
Canonical and pending transactions preserve the native currency. Do not coalesce
cross-currency transfers without explicit amounts in both currencies.

Config loading checks Unix permissions on the opened file and supports an
exact `${FIREFLY_ACCESS_TOKEN}` reference for `firefly.access_token`; unresolved
or empty references fail before side effects. Literal tokens remain supported.

Sender authentication is mandatory. `trusted_authserv_ids` names receiving
services that sanitize inbound Authentication-Results. `auth.rs` requires the
topmost result to report aligned DMARC pass for a single raw From address.
Unauthenticated mail exits successfully as skipped_authentication before any
LLM, Firefly, or transfer-store activity. Use raw subject/body from that same
message. Tests must include realistic trusted authentication results.

Default validation performs two differently prompted extractions, requires a
valid email.date and transaction_date within seven bank-local calendar days,
checks explicit currency/amount evidence and INR >500000, and holds suspect
transactions before any POST or transfer insert. Review replays remain held.
Balance observations are account/currency scoped and replay safe; only an actual
balance match earns reconciled. mailtx reconcile reads reference-file intervals,
pages Firefly using GET only, and reports drift/incomplete data without LLM work.
