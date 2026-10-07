# Processor source filtering

Status: implemented; PostgreSQL-backed acceptance verification is pending.

## 1. Problem

Processors currently subscribe to event types through `processors.events`.
Every enabled processor subscribed to a type receives matching events from
every account and mailbox. Operators cannot restrict a processor to a
particular account or select different mailboxes for different accounts.

Add an optional processor-level source allowlist. Apply it centrally and
consistently to normal dispatch, replay, event dry-run, historical backfill,
and queued-job execution.

## 2. Configuration

Add `sources` alongside `events` in each `[[processors]]` entry. Each source
requires an account ID and optionally specifies mailbox names.

### One account, all its mailboxes

```toml
[[processors]]
name = "mail-indexer"
events = ["email_arrived"]
sources = [{ account = "personal" }]

[processors.config]
command = "/usr/local/bin/mailmux-submit"
```

### Different mailbox selections for different accounts

```toml
[[processors]]
name = "notify"
events = ["email_arrived"]
sources = [
    { account = "personal", mailboxes = ["INBOX", "Archive"] },
    { account = "work", mailboxes = ["INBOX"] },
]

[processors.config]
command = "/usr/local/bin/notify-new-mail"
```

This processor receives personal INBOX and Archive events, plus work INBOX
events. It does not receive work Archive events.

Keeping mailboxes attached to account selectors avoids the cross-product
semantics of separate processor-level `accounts` and `mailboxes` lists.

## 3. Matching rules

- Omitted `sources` means all accounts and mailboxes, preserving existing
  behavior.
- `account` matches the configured `[[accounts]].id`, not the login username
  or an email address.
- Omitted `mailboxes` means all mailboxes belonging to that account.
- Multiple mailbox names within a source are ORed.
- Multiple source entries are ORed. Overlapping entries never cause a
  processor to receive the same event more than once.
- The existing event-type subscription must also match.
- Account IDs and mailbox names match their stored values exactly. Do not
  introduce case folding, wildcard expansion, globbing, or regular expressions.
- Source filtering affects processing only. It does not change ingestion,
  mailbox monitoring, or durable email storage.

The eligibility predicate is:

```text
processor is enabled and registered
AND event type is subscribed
AND (
    sources is omitted
    OR any source satisfies (
        event.account_id == source.account
        AND (
            source.mailboxes is omitted
            OR event.mailbox_name is in source.mailboxes
        )
    )
)
```

## 4. Validation

Reject configuration containing:

- An explicit empty `sources` list. Use `enabled = false` to disable a
  processor.
- A source with a missing or empty account ID.
- An account ID absent from the configured accounts. A configured but
  disabled account remains a valid reference for historical processing.
- An explicit empty `mailboxes` list or an empty mailbox name.
- Unknown fields inside a source selector, so a misspelled restriction cannot
  silently broaden its scope.

Warn when a selected mailbox is not currently monitored for its account.
Do not reject it: stored historical emails may belong to a mailbox that is
no longer monitored. A historical account must remain declared in the
configuration, optionally disabled, to be referenced by a source selector.

If selectors support the existing `${VAR}` substitution convention, resolve
their values and account IDs before checking references and empty values.
Preserve the existing validation of password environment-variable references.

## 5. Shared implementation

Represent omission separately from explicit emptiness, for example with
`Option<Vec<ProcessorSource>>` and `Option<Vec<String>>` for mailbox names.

Keep the routing predicate in shared processor configuration or registry
code. The registry should retain source configuration alongside each
processor and change `processors_for_event` to accept the full `Event`
instead of only its type. Provide an eligibility check for an explicitly
named processor as well.

The event already carries `account_id` and `mailbox_name`; matching requires
no additional database query. Preserve the `Processor::process` contract and
the command-processor `{event, email}` stdin JSON shape. Built-in and external
processors must not implement their own copies of source filtering.

## 6. Execution paths

| Path | Required behavior |
|---|---|
| Normal dispatch | Create jobs only for eligible processors. |
| Replay without a named processor | Run only processors eligible under the current configuration. Report when none match. |
| Replay or event dry-run with a named processor | Reject an ineligible selection with a clear error before creating or resetting a job or invoking the processor. |
| Historical backfill | Intersect CLI selection with the selected processor's sources. |
| Queued jobs and retries | Recheck eligibility under the current process configuration before invoking the processor. |

For historical backfill, apply the source predicate in both the parameterized
COUNT query and the paginated email query. Compute `selected` and apply
`--limit` to the eligible result set, rather than fetching a CLI-limited set
and discarding ineligible emails afterward. Preserve account/mailbox pairing
in SQL. Backfill dry-run must report the same selection as an actual run.
An empty intersection selects zero emails and follows existing no-match
behavior. `--all` does not override processor restrictions.

For an existing pending or retryable job that is no longer eligible, use the
existing terminal `abandoned` status with an explanatory reason and clear
`next_retry_at`. Do not increment attempts or invoke the processor. Check
eligibility before transitioning the job to `in_progress`. Disabling or
removing its processor should likewise terminate the queued job cleanly.

Configuration changes take effect when the daemon loads the new
configuration. Already running invocations are not interrupted. Broadening
a filter does not automatically replay previously dispatched events;
operators use replay or backfill explicitly. No filter-bypass CLI option is
part of this proposal.

## 7. Track dispatch independently of processor jobs

Source filtering makes it normal for an event to match zero processors.
Currently, `get_unprocessed_events` selects events with no `processor_jobs`
rows, ordered by ID with a batch limit. Unmatched events would therefore be
fetched repeatedly. Enough such events could occupy the entire batch and
prevent later matching events from being dispatched.

Add nullable `events.dispatched_at` to record that routing has finished,
independently of whether any processor ran:

1. Lock the event row inside a dispatch transaction.
2. If it is already dispatched, return without scheduling new work.
3. Insert all matching processor jobs using the existing uniqueness rule.
4. Set `dispatched_at`, including when no processors match.
5. Commit before executing newly registered jobs.

Polling selects events whose `dispatched_at IS NULL`, supported by an
appropriate partial index on event ID. Concurrent or duplicate dispatches
must not create or execute duplicate jobs. A failed registration transaction
must leave the event undispatched so polling can retry it.

The migration should mark existing events with processor jobs as dispatched
without modifying those jobs or their statuses. Existing events without jobs
remain eligible for one routing pass. This preserves the current treatment
of already dispatched history and prevents automatic execution by newly
added processors on that history.

Explicit replay remains independent of the dispatch marker and must not
clear it. Historical backfill continues to create no persisted events or
processor jobs. Keep terminal, unmatched events eligible for normal retention
cleanup, and preserve protection for events with outstanding jobs.

## 8. Scope and compatibility

Existing configurations without `sources` keep their current routing
behavior. The database migration is required for reliable zero-match
dispatch. Source filtering introduces no changes to processor input JSON,
email storage, timeout settings, retry settings, or concurrency settings.

This feature does not include content-based filtering, exclusions, wildcard
selectors, dynamic configuration reload, automatic historical replay, or a
general routing expression language.

## Implementation map

- Configuration and matching: `src/config.rs`; runtime registration and eligibility: `src/processor/registry.rs`.
- Transactional dispatch and queued claims: `src/db/jobs.rs` and `src/processor/scheduler.rs`.
- Source-restricted backfill SQL: `src/db/emails.rs` and `src/backfill.rs`.
- Dispatch marker and existing-history treatment: `migrations/20261008000000_add_event_dispatch_marker.sql`.

Verify with `cargo test -p mailmux` and `cargo clippy -p mailmux --all-targets -- -D warnings`. PostgreSQL-backed ignored tests use `cargo test -p mailmux -- --ignored` with `DATABASE_URL` configured.

## 9. Verification and acceptance criteria

- Configuration tests cover omission, valid selectors, unknown account IDs,
  unknown selector fields, empty lists, empty names, and resolved references
  if environment-variable substitution is supported.
- Matching tests cover event-type restrictions, account-only selection,
  multiple mailboxes, paired account/mailbox selection, overlapping sources,
  and exact matching.
- Execution-path tests verify normal dispatch, explicit replay and dry-run
  rejection, and queued-job abandonment without invocation or attempt
  increment.
- Backfill tests verify source/CLI intersection, paired predicates, counts,
  limits, pagination, and dry-run agreement.
- Database tests verify that at least one full batch of unmatched events
  cannot block a subsequent matching event; duplicate dispatch creates no
  duplicate execution; and transaction failure leaves events retryable.
- Migration tests preserve existing jobs and mark their events dispatched.
- Retention tests cover unmatched dispatched events and events with
  outstanding jobs.
- Update the configuration example, README, and agent guidance when the
  feature is implemented. Run formatting, Clippy, and the relevant test suite
  for the implementation.
