# mailtx

A mailmux command processor that extracts structured transaction data from bank
notification emails and posts it to Firefly III.

For each incoming email it:

1. Checks the sender against a configured allow-list — skips silently if not matched
2. Reads the raw `.eml` file and extracts a plain-text body (strips HTML if needed)
3. Sends the subject and body to the configured LLM backend (`auto` via genai, or an explicit OpenAI-compatible Chat Completions endpoint)
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
| `src/llm.rs`    | Backend dispatch, shared prompts/schema, and extraction validation |
| `src/endpoint/` | Endpoint abstraction and Firefly III implementation          |

### LLM prompt

The shared prompt asks the configured model to return a JSON object with six fields:

```json
{
  "status": "found",
  "amount": 1234.56,
  "transaction_type": "withdrawal",
  "narration": "Amazon Pay",
  "transaction_date": "2026-02-26T13:45:00+05:30",
  "category": "Shopping"
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
  "error_if_duplicate_hash": true,
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
`Authorization: Bearer <token>`.  The base URL **must use HTTPS** — the
`allow_insecure_http` loopback override exists only for local development.

## Metrics

mailtx emits Prometheus metrics via mailmux's command processor stdout protocol.
Each invocation writes a `ProcessorOutput` JSON object to stdout; mailmux reads
it and registers the metrics under the `mailmux_proc_mailtx_` namespace in its
`/metrics` endpoint.  The complete structured output is also persisted in the
`processor_jobs.output` column, providing an authoritative per-email outcome
record beyond aggregate metrics.

### Metric reference

| Metric | Type | Labels | Description |
|---|---|---|---|
| `mailmux_proc_mailtx_emails_processed_total` | counter | `result` | One increment per invocation, labelled with the terminal outcome |
| `mailmux_proc_mailtx_llm_calls_total` | counter | `result` | LLM API calls attempted |
| `mailmux_proc_mailtx_firefly_requests_total` | counter | `operation`, `result` | Firefly HTTP requests made |
| `mailmux_proc_mailtx_account_match_total` | counter | `method` | Account-resolution attempts and the method used |
| `mailmux_proc_mailtx_transfer_legs_stored_total` | counter | — | Transfer legs stored pending a counterpart |
| `mailmux_proc_mailtx_transfer_coalesced_total` | counter | — | Transfers successfully coalesced from two legs |
| `mailmux_proc_mailtx_transfer_expired_total` | counter | — | Expired transfer legs flushed as unmatched transactions |

### Label values

**`emails_processed_total{result=…}`**

| Value | Meaning |
|---|---|
| `skipped_sender` | Sender not in allow-list, or no email record attached |
| `no_transaction` | LLM returned `status=not_found` |
| `posted` | Regular transaction posted to Firefly |
| `transfer_stored` | First leg of a transfer stored; waiting for counterpart |
| `transfer_coalesced` | Both transfer legs matched; transfer posted to Firefly |
| `error` | Invocation failed (non-zero exit) |

### ProcessorOutput metadata

In addition to the metrics counters above, mailtx embeds outcome metadata
inside the `ProcessorOutput.metadata` object. This metadata is persisted in
the `processor_jobs.output` column by mailmux and serves as the authoritative
per-email/per-processor outcome record.

| Field | Type | Description |
|---|---|---|
| `metadata.outcome` | string | One of `posted`, `no_transaction`, `skipped_sender`, `transfer_stored`, `transfer_coalesced`, or `error` |
| `metadata.firefly_transaction_id` | string (optional) | Firefly transaction ID when a transaction was successfully posted (regular or coalesced transfer). Absent for non-posting outcomes. |

**`llm_calls_total{result=…}`**

| Value | Meaning |
|---|---|
| `success` | LLM responded and JSON was parsed successfully |
| `error` | API call failed or JSON parse of the response failed |

**`firefly_requests_total{operation=…, result=…}`**

| `operation` | Meaning |
|---|---|
| `get_categories` | `GET /v1/categories` (fetched once per eligible email) |
| `post_transaction` | `POST /v1/transactions` (regular, transfer, or expired-leg flush) |

`result` is `success` or `error`.

**`account_match_total{method=…}`**

| Value | Matching strategy used |
|---|---|
| `debit_card_last4` | Card number last-4 digits found in email |
| `account_suffix` | Account suffix found in email |
| `alias` | Alias substring match |
| `default_fallback` | `default_asset_account_id` used |
| `failed` | No account could be resolved (invocation errors out) |

## Prerequisites

- Rust 1.80+ (edition 2024) — for `std::sync::LazyLock`
- An API key for the selected cloud provider, if required
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

In `auto` mode, genai infers provider keys from the environment. In custom mode, no provider keys are inferred: use `api_key_env` for an explicit Bearer key or omit it for no authentication.

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
# Senders matched as exact normalized mailbox addresses.
allowed_senders = ["alerts@mybank.com", "noreply@anotherbank.com"]
state_db = "/var/lib/mailtx/state.sqlite" # durable validation review queue

# Model name passed to genai. Provider (and required API key) is inferred from
# the name. Default: "claude-haiku-4-5-20251001"
llm_model = "claude-haiku-4-5-20251001"

# Tag applied to every posted transaction. Default: "mailmux-mailtx"
# tag = "mailmux-mailtx"

[firefly]
base_url     = "https://firefly.example.com/api"
access_token = "eyJ..."
currency_code = "INR" # set the native currency; per-account overrides supported

# Optional — shown with defaults:
# default_asset_account_id = "12"
# currency_code            = "USD"
# apply_rules              = false
# fire_webhooks            = true
# error_if_duplicate_hash  = true
# allow_insecure_http      = false
```

> **Security note:** The `Authorization: Bearer` header and all transaction
> payloads are sent to `base_url`.  They **must never** be transmitted over
> plaintext HTTP.  `allow_insecure_http = true` is provided only for local
> development with `http://localhost`, `http://127.0.0.1`, or
> `http://[::1]`.  Private/internal plaintext URLs (e.g. `http://firefly.lan`)
> are always rejected — configure HTTPS instead.

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
debit_card_last4   = []
aliases            = ["sbm bank"]
```

With no `[llm]` table, `llm_model` retains genai's automatic provider routing. Or configure auto explicitly (a model in this table is allowed; do not also set `llm_model`):

```toml
[llm]
backend = "auto"
model = "gpt-4o-mini"
response_format = "json_schema"
# timeout_secs = 60
# connect_timeout_secs = 10
# max_tokens = 1024
# temperature = 0.0
```

Custom mode is explicit and sends the configured model ID unchanged:

| Provider       | Example model               | API key env var     |
| -------------- | --------------------------- | ------------------- |
| Anthropic      | `claude-haiku-4-5-20251001` | `ANTHROPIC_API_KEY` |
| OpenAI         | `gpt-4o-mini`               | `OPENAI_API_KEY`    |
| Google Gemini  | `gemini-2.0-flash`          | `GEMINI_API_KEY`    |
| Groq           | `llama-3.1-8b-instant`      | `GROQ_API_KEY`      |
| Ollama (local) | `llama3.2`                  | _(no key required)_ |

Example custom configuration (llama.cpp or llama-swap Chat Completions):

```toml
[llm]
backend = "openai_compatible"
model = "bank-extractor"
base_url = "http://127.0.0.1:8080"
allow_insecure_http = true
endpoint = "/v1/chat/completions"
response_format = "json_schema" # json_schema, json_object, or prompt
# api_key_env = "MAILTX_LLM_API_KEY" # omitted means no Authorization header
# timeout_secs = 60; connect_timeout_secs = 10
# max_tokens = 1024; temperature = 0.0
```

Custom mode defaults to `/v1/chat/completions`, 60s total/10s connect timeout,
1024 max tokens, and temperature 0.0. A `[llm]` table conflicts with an explicitly
set legacy `llm_model`. `extra_body` supports server-specific JSON fields but
cannot override `model`, `messages`, `stream`, `response_format`, `max_tokens`,
`max_completion_tokens`, `temperature`, or `n`. Base URL paths are preserved as prefixes;
no `/v1` is inferred. Native Ollama, llama.cpp completion, Anthropic Messages,
and OpenAI Responses protocols are not supported here. There is no discovery,
model management, fallback, or internal retry. HTTP to a custom LLM host requires
`allow_insecure_http = true` (LAN/container hosts are permitted); prefer HTTPS or
encrypted tunnels. This permission is separate from Firefly's loopback-only HTTP
exception. Invalid model output is an error; only valid `not_found` is skipped.
The 60-second deadline is new; cold models may need `timeout_secs = 180`,
mailmux timeout around 240 seconds, and `concurrency = 1`.

### Sender matching

`allowed_senders` entries are matched by exact normalized mailbox address.

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
re-runs the LLM call even if it succeeded the first time. Firefly writes carry a
stable mailmux event ID as Firefly's `external_id`. On a retry, mailtx checks
that ID before posting and treats an existing transaction as success.
`error_if_duplicate_hash` also defaults to `true` as a backstop.

For `mailmux backfill`, transient events have ID `0`. mailtx instead uses
`mailmux:email:<email.id>` as the external ID, so distinct historical emails
stay distinct and repeated backfills retain the same identity. The email ID
must be positive. Normal events continue to use `mailmux:event:<event.id>`.
These namespaces are separate: backfilling mail already processed through a
normal event does not deduplicate against that event's transaction.

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

### Amounts, booking dates, and category caching

Amounts use decimal arithmetic from LLM JSON through Firefly posting. Transfer
state retains checked integer minor units. Zero, out-of-range, and sub-cent
amounts are rejected rather than silently rounded. Existing SQLite amounts
remain compatible. Cleanup failures after a successful transfer POST emit a
warning and preserve success; stable external IDs protect replayed posts.
Legacy expired legs without an external ID receive a stable ID derived from
persistent store identity and row ID.

Date-only transactions use an IANA bank timezone and retain its offset in the
Firefly payload. Configure a global default and optional account overrides:

```toml
[firefly]
transaction_timezone = "Asia/Kolkata" # default: UTC
categories_cache_ttl_secs = 300       # 0 disables; maximum 3600
# Optional persistent path; the parent directory must be private to this user.
# categories_cache_path = "/var/lib/mailtx/private-cache/categories.sqlite"

[[firefly.asset_accounts]]
id = "usd-account"
firefly_account_id = "42"
timezone = "America/New_York"
```

Explicit RFC3339 offsets are preserved. A nonexistent or ambiguous local date
is rejected. Category results are cached across mailtx invocations for five
minutes by default, isolated by Firefly URL and a digest of its access token;
the token itself is not stored. On Unix, the default cache lives in a private
user-owned directory under the system temporary directory. The cache is
bounded to 64 endpoint/token entries of at most 1 MiB each. Expired or failed
cache reads fetch fresh data, and cache write errors do not fail the email.
Transfer account references are validated at startup, and invalid sender-list
entries emit a warning identifying their index.

### US bank accounts and native currencies

Capital One and Bank of America notifications use the same extractor. Configure
exact sender mailboxes from your authenticated bank emails in `allowed_senders`,
then match the account/card identifiers actually present in those notifications:

```toml
[firefly]
# Keep the other required Firefly settings here.
currency_code = "INR" # fallback for accounts without an override

[[firefly.asset_accounts]]
id = "capital-one"
firefly_account_id = "42" # an existing USD asset account in Firefly
card_last4 = ["1234"] # alias of debit_card_last4; credit cards also match
currency = "USD" # alias of currency_code
timezone = "America/New_York" # choose the bank booking timezone

[[firefly.asset_accounts]]
id = "bank-of-america"
firefly_account_id = "43"
account_suffixes = ["5678"]
currency_code = "USD"
timezone = "America/New_York"
```

The matched account's currency overrides the global fallback for deposits,
withdrawals, and persisted pending legs. Currency codes are normalized to three
uppercase ASCII letters. Enable USD in Firefly and use USD asset accounts for
these native amounts. For INR equivalents in reports, configure INR as Firefly's
primary currency, enable exchange rates and primary-currency display, and supply
appropriate historical rates; `currency_code` alone does not enable conversion.
See the official [currency documentation](https://github.com/firefly-iii/docs/blob/main/docs/docs/how-to/firefly-iii/features/currencies.md)
and [exchange-rate documentation](https://github.com/firefly-iii/docs/blob/main/docs/docs/explanation/financial-concepts/exchange-rates.md).

Cross-currency transfer rules are rejected: Firefly needs both native and foreign
amounts, which mailtx does not invent. Pending legs match only within the same
currency; legacy legs without a currency expire separately using the account's
configured currency. Configure every account's currency when mixing currencies.

### Payee display names

Common whole-token payees (`Swiggy`, `Zomato`, `Amazon`) are normalized to readable
names. Other descriptions retain their text with whitespace collapsed. Add ordered,
case-insensitive literal rules at the top level, before `[firefly]` or `[llm]`:

```toml
[[payee_rules]]
contains = "SWIGGY*ORDER"
name = "Swiggy"
```

Explicit rules take precedence over built-ins; the first matching rule wins.
Transfer keywords match the original extracted narration before cleanup. The
original email remains the source record under mailmux's retention policy.

### Counterparties and the Firefly cash label

Withdrawals send the cleaned narration as `destination_name` (expense account);
deposits send it as `source_name` (revenue account). Previously both were omitted,
leaving Firefly to use its anonymous/cash counterparty. Firefly can create or reuse
the named account. Transfers retain both explicit asset-account IDs. This affects
new imports; it does not rewrite existing transactions or their counterparties.

### Additional canonical tags

The legacy top-level `tag` remains the provenance tag. Optional top-level `tags`
and per-asset-account `tags` add arbitrary categories or workflow labels:

```toml
tag = "mailmux-mailtx"
tags = ["imported"]
# Other top-level settings, then the Firefly table...
[[firefly.asset_accounts]]
id = "hdfc"
firefly_account_id = "12"
tags = ["bank-hdfc"]
```

Tags are trimmed, empty values removed, and duplicates removed in first-seen
order. They are carried by CanonicalTransaction through posting and pending
transfer storage, rather than reconstructed by the Firefly transport.

### Automatic source, month, and workflow tags

Default tagging adds `mailmux-<local-account-id>` (lowercase, punctuation collapsed
to hyphens), `mailmux-YYYY-MM` using the transaction's bank-local booking month,
and `auto-categorized` when a category was extracted. For example, the account
`hdfc` and a March 2026 transaction produce `mailmux-hdfc` and `mailmux-2026-03`.
Individual groups can be disabled:

```toml
[tagging]
source = true
month = true
workflow = true
```

`needs-review` is reserved for checks requiring attention, and `reconciled` for
successful balance verification; ordinary imports do not claim reconciliation.
Custom tags remain supported independently of these automatic groups.

### Running bank balance monitoring

Enable monitoring when notifications contain explicit `Closing balance`, `Avl Bal`,
or `Available balance` figures, and set the native currency of each account:

```toml
state_db = "/var/lib/mailtx/state.sqlite" # top level
[balance_monitor]
enabled = true # default: false
tolerance_minor_units = 1
```

The first observation establishes a baseline. Later withdrawals subtract and
deposits add their amounts; agreement earns `reconciled`, and gaps earn
`needs-review` with a warning and `metadata.balance_status = "drift"`. Monitoring
continues from the observed bank balance after a gap so one missing email does
not trigger drift forever. Persisted bank observations are independent of Firefly
posting success; replay retains the original verdict. Older/equal-timestamp email
observations are `out_of_order`, require review, and do not rewind the baseline.
Email timestamps order observations, with transaction timestamps as fallback.

Currencies and accounts have separate baselines. Missing balance evidence is
reported as `no_balance`; contradictory figures or currency mismatches require
review. Labels must describe the post-transaction available/closing balance.
Credit-limit/available-credit figures are not supported as account balances.
This is evidence from notifications, not an independent bank statement audit.

### Validation and manual review

Validation is enabled by default and requires top-level `state_db` for a durable
review queue. Set each account's native currency (or the global fallback). A
found transaction must have a nonzero exact amount, a narration, and a valid date
within seven calendar days of `email.date` in the booking timezone. Missing email
dates are held. INR amounts over 500,000 are held by default. Currency limits are
independent; add appropriate limits for other currencies:

```toml
[validation]
enabled = true
double_extraction = true # two calls with different instructions, compared exactly
date_window_days = 7
regex_tolerance_percent = 5
[validation.max_amounts]
INR = "500000"
USD = "10000"
```

Explicit currency figures in the email must agree with the account currency. The
first prominent currency-labelled amount is cross-checked against the extraction;
labelled closing balances are excluded. This is a conservative heuristic:
notifications with multiple unrelated amounts can require manual review. Zero,
sub-cent, or malformed model amounts fail extraction without posting. The second
extraction must agree on absolute amount and direction; agreement is a consistency
check, not proof of model accuracy. When explicit closing-balance evidence is
present, validation also runs persistent balance monitoring; drift, out-of-order
observations, and ambiguous balances are held before posting or transfer storage.

Held items return success with `metadata.outcome = "held_for_review"` and
`review_reasons`. No transaction is posted. SQLite `review_queue` keeps one row per
stable event identity, its reasons, original email path, and available transaction
fields/tags. Replays remain held and make no LLM or Firefly calls. Inspect the row
and original email, correct the extraction/configuration or import manually, then
explicitly remove the reviewed queue row before an intentional replay. Retain
original email files until review is complete; use private durable state storage.
There is no automatic approval or deletion of held items.

Setting `[validation] enabled = false` explicitly restores the previous posting
behavior; use this only when validation/review is handled separately. An outer
mailmux command timeout must allow two LLM deadlines plus Firefly work.
