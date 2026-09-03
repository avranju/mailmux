# Mailmux Historical Processor Backfill

## 1. Purpose

Add a generic historical backfill facility to `mailmux` so an operator can explicitly run one configured processor against durable emails that were ingested in the past, filtered by date/account/mailbox and related criteria.

The immediate consumer is a mail-index submission processor that forwards archived mail to a separate indexing service. The feature must remain generic: `mailmux` must not know anything about search engines, Tantivy, Turso, MCP, or AI.

This feature complements the existing event replay command. Replay operates on retained events; backfill operates directly on durable `emails` rows and their associated raw `.eml` files.

## 2. Current architecture and constraints

Mailmux currently:

- syncs one or more IMAP accounts into PostgreSQL;
- stores email metadata in `emails`;
- stores raw RFC 5322 messages on disk and records `raw_message_path` in `emails`;
- creates `email_arrived` events for newly ingested mail;
- invokes configured processors for events;
- supports a built-in command processor that sends JSON containing `{ "event": ..., "email": ... }` on stdin;
- supports `mailmux replay --event-id ...`, but replay requires an event row;
- periodically deletes old completed/abandoned events according to the configured retention period.

Therefore historical processor execution must query `emails`, not rely on old `events` rows still existing.

## 3. Goals

The implementation MUST:

1. Add a `mailmux backfill` CLI subcommand.
2. Require the operator to select exactly one configured processor per invocation.
3. Select historical emails using explicit filters.
4. Stream/paginate matching emails rather than loading the entire result set into memory.
5. Invoke the selected processor using the same `Processor` trait used for normal event processing.
6. Preserve compatibility with existing processors, especially the command processor and `mailtx`.
7. Avoid inserting fake historical events into the `events` table.
8. Avoid creating `processor_jobs` rows for historical backfill.
9. Support processor timeouts and retry settings.
10. Continue processing other emails after an individual email permanently fails, unless `--fail-fast` is supplied.
11. Produce a clear final summary and a non-zero exit status if one or more emails permanently fail.
12. Be safe to re-run. Mailmux itself does not guarantee downstream idempotency, but the command must make repeated execution predictable and must document that backfill processors SHOULD be idempotent.

## 4. Non-goals

This specification does NOT include:

- persistent resumable backfill-job state;
- a new scheduler or queue;
- automatic discovery of which historical emails a processor has already seen;
- creation of one database event per historical email;
- mutation of email metadata;
- automatic deletion/reconciliation of downstream data;
- changes to normal `email_arrived` processing semantics;
- a general event-history replay query language.

A future version may add persistent backfill runs/checkpoints if real operational experience justifies it.

## 5. CLI contract

Add a `Backfill` variant to `cli::Command`.

### 5.1 Basic form

```bash
mailmux backfill --processor mail-indexer --after 2021-01-01 --before 2022-01-01
```

### 5.2 Proposed options

```text
mailmux backfill [OPTIONS] --processor <NAME>

Required:
  --processor <NAME>          Configured processor to invoke

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
  --fail-fast                 Stop after the first permanently failed email
  --dry-run                   Print/count selected emails without invoking processor
```

### 5.3 Safety rule

A backfill invocation MUST contain at least one selection filter (`--after`, `--before`, `--account`, `--mailbox`, `--email-id`) unless `--all` is explicitly present.

This prevents accidentally invoking a side-effecting processor over the entire archive due to a typo or omitted argument.

`--all` MAY be combined with execution controls but MUST NOT be required when another selection filter is present.

### 5.4 Date parsing

Accept:

- `YYYY-MM-DD`
- RFC 3339 timestamp, including timezone offset

Date-only values are interpreted as midnight UTC.

Semantics:

- `--after 2021-01-01` means `email.date >= 2021-01-01T00:00:00Z`.
- `--before 2022-01-01` means `email.date < 2022-01-01T00:00:00Z`.

Using an exclusive upper bound makes year/month ranges straightforward and avoids end-of-day ambiguity.

Emails whose `date` is NULL do not match `--after` or `--before`. They may still match account/mailbox/email-id selections without date filters.

Reject `--after >= --before` after normalization.

## 6. Processor selection and validation

The command MUST resolve the selected processor by exact configured name.

Errors that MUST abort before processing begins:

- processor name does not exist;
- processor is disabled / absent from the runtime registry;
- selected processor does not subscribe to `email_arrived`;
- invalid filters;
- concurrency is zero;
- database connection/migration failure.

The backfill command explicitly selects one processor; it MUST NOT execute every processor subscribed to `email_arrived`.

## 7. Historical email query

### 7.1 New DB API

Add a backfill query abstraction in `src/db/emails.rs`, for example:

```rust
pub struct EmailBackfillFilter {
    pub after: Option<DateTime<Utc>>,
    pub before: Option<DateTime<Utc>>,
    pub accounts: Vec<String>,
    pub mailboxes: Vec<String>,
    pub email_ids: Vec<i64>,
}
```

Do not construct SQL by concatenating user input. Use `sqlx::QueryBuilder<Postgres>` or equivalent parameterized queries.

### 7.2 Pagination

The implementation MUST paginate by `emails.id`, not by OFFSET.

Example logical query:

```sql
SELECT ...
FROM emails
WHERE id > $last_id
  AND <filters>
ORDER BY id ASC
LIMIT $page_size;
```

Recommended page size: 500. It may be an internal constant.

This ensures stable iteration and bounded memory use for very large mailboxes.

### 7.3 Ordering

Backfill processing order is ascending `emails.id`.

Do not order by message `date`: malformed or delayed email dates can be non-monotonic, NULL, or misleading. `emails.id` gives deterministic ingestion order.

`--limit` is applied to the selected stream in this deterministic order.

## 8. Processor invocation semantics

### 8.1 Do not persist synthetic events

For each selected email, construct a transient in-memory `Event` and invoke the processor directly.

Do NOT insert this event into PostgreSQL.

Use:

```text
id          = 0
email_id    = Some(email.id)
event_type  = "email_arrived"
account_id  = email.account_id
mailbox_name= email.mailbox_name
created_at  = current UTC time
```

The payload MUST make the synthetic nature explicit:

```json
{
  "backfill": true,
  "email_id": 12345,
  "uid": 67890,
  "subject": "...",
  "sender": "..."
}
```

`event.id == 0` is reserved/documented for non-persisted backfill invocations.

This preserves the existing `Processor::process(&Event, Option<&EmailRecord>)` contract and the existing command-processor stdin JSON shape.

### 8.2 Why this approach

Do not refactor the processor trait solely to add backfill. Existing processors are already written around an `Event` plus optional `EmailRecord`, and a transient `email_arrived` event is sufficient for this use case.

If a future processor requires richer invocation metadata, introduce an invocation-context abstraction in a separate change rather than coupling it to this backfill feature.

## 9. Retry, timeout, and concurrency behavior

### 9.1 Timeout

Use the selected processor's configured `timeout_secs` for every attempt.

### 9.2 Retries

For each email:

1. invoke processor;
2. if it succeeds (`ProcessorOutput.success == true`), mark that email successful in in-memory counters;
3. if it returns failure, errors, or times out, retry according to the processor's configured `max_retries` and `retry_backoff_secs`;
4. after retries are exhausted, mark the email permanently failed and continue unless `--fail-fast` is set.

Reuse/factor existing retry-delay logic where practical. Do not persist retry state in `processor_jobs`.

### 9.3 Concurrency

Default concurrency is the selected processor's configured `concurrency`.

`--concurrency N` overrides it for this backfill invocation only.

Use bounded concurrency (for example `buffer_unordered` / semaphore + `JoinSet`) so memory remains bounded.

Even when processing concurrently, enumeration and final counters must remain deterministic. Log each email by `email_id` so failures are identifiable.

### 9.4 Failure exit status

Exit 0 when:

- dry-run succeeds; or
- all invoked emails eventually succeed; or
- no emails match (with a warning/summary).

Exit non-zero when:

- configuration/filter/setup fails; or
- at least one email permanently fails.

A batch with partial success MUST still print the success/failure counts before returning the error status.

## 10. Logging and progress

At startup log:

- processor name;
- normalized filters;
- concurrency;
- dry-run state.

During processing:

- debug-level per-email start/success;
- warn/error per-email retry/permanent failure;
- info-level progress every 100 completed emails (or a similarly modest fixed interval).

Final summary MUST contain:

```text
selected=<N>
processed=<N>
succeeded=<N>
failed=<N>
skipped=<N>
elapsed=<duration>
```

For `--dry-run`, report `selected=<N>` and do not invoke the processor.

## 11. Suggested code organization

Keep `main.rs` from growing further if possible.

Suggested additions:

```text
mailmux/src/
  backfill.rs             # orchestration + transient event construction
  cli.rs                  # Backfill args
  db/emails.rs            # filtered keyset-paginated query
```

Possible API shape:

```rust
pub async fn run(config: Config, args: BackfillArgs) -> Result<BackfillSummary>;
```

Extract small reusable helpers from replay/scheduler only when doing so clearly reduces duplication. Avoid a broad scheduler rewrite.

## 12. Command-processor compatibility

The existing command processor receives JSON shaped like:

```json
{
  "event": { ... },
  "email": { ... }
}
```

Backfill MUST preserve this shape.

A shell processor can distinguish a historical invocation using:

```jq
.event.payload.backfill == true
```

The `email.raw_message_path` field MUST remain present. This is how a mail-index submission script can read the archived RFC 5322 message and upload it to the external indexer.

## 13. Tests

Add unit tests for at least:

1. CLI parsing for all new options.
2. safety validation rejects no-filter/no-`--all` invocation.
3. `--all` permits an unfiltered run.
4. date-only parsing and RFC3339 parsing.
5. invalid date range rejection.
6. transient event construction:
   - `id == 0`;
   - `event_type == "email_arrived"`;
   - correct `email_id`, account and mailbox;
   - `payload.backfill == true`.
7. processor lookup failure.
8. processor subscription validation.
9. `--limit` handling.
10. retry count behavior using a fake processor.
11. timeout handling using a fake slow processor.
12. continue-on-error vs `--fail-fast`.
13. bounded concurrency using a fake processor with an atomic active-call counter.

For database selection, add tests around query construction/filter semantics. If the repository does not yet have a disposable PostgreSQL test harness, do not make the entire feature dependent on creating one. A small opt-in/ignored integration test using `DATABASE_URL` is acceptable.

## 14. Documentation

Update:

- `README.md` with a Historical Backfill section and examples;
- `mailmux/AGENTS.md` with the new command/module and semantics;
- CLI `--help` text.

Example documentation:

```bash
# Re-submit all 2021 mail from one account to the mail indexer processor
mailmux backfill \
  --processor mail-indexer \
  --account personal \
  --after 2021-01-01 \
  --before 2022-01-01

# Preview selection first
mailmux backfill \
  --processor mail-indexer \
  --account personal \
  --after 2021-01-01 \
  --before 2022-01-01 \
  --dry-run
```

## 15. Acceptance criteria

The feature is complete when all of the following hold:

- [ ] `mailmux backfill --help` documents the new command and filters.
- [ ] An operator can select one processor and historical emails by date/account/mailbox.
- [ ] Running without filters requires explicit `--all`.
- [ ] Processing reads durable `emails` rows and does not require historical `events` rows.
- [ ] No synthetic event or processor-job rows are persisted.
- [ ] The command processor receives the same `{event,email}` JSON shape as normal processing.
- [ ] `email.raw_message_path` is available to the external processor.
- [ ] Timeout/retry/concurrency settings work as specified.
- [ ] A failure for one email does not stop the batch unless `--fail-fast` is used.
- [ ] Partial failure produces a final summary and non-zero exit status.
- [ ] The command can process a large archive with bounded memory.
- [ ] Existing replay, dry-run, daemon processing, and `mailtx` behavior remain unchanged.
- [ ] Tests and project documentation are updated.

## 16. Implementation guidance for an autonomous coding agent

1. Read `mailmux/AGENTS.md`, `src/cli.rs`, `src/main.rs`, `src/db/emails.rs`, `src/processor/mod.rs`, `src/processor/registry.rs`, `src/processor/scheduler.rs`, and `src/processor/builtin/command.rs` before editing.
2. Prefer a narrow addition over refactoring the processor system.
3. Preserve the external command-processor JSON contract.
4. Keep database reads parameterized and keyset-paginated.
5. Run `cargo fmt`, `cargo clippy --all-targets --all-features`, and `cargo test` before completion.
6. Report any deviation from this specification explicitly rather than silently changing semantics.
