# Mailmux implementation design

This document describes the implemented daemon. The [workspace README](../../README.md)
and [configuration example](../config.example.toml) contain runnable setup and
configuration examples. The SQL files in [migrations](../migrations/) are the
schema source of truth.

## Ingestion and storage

One account manager supervises a watcher per configured mailbox. Each watcher
owns one authenticated IMAP connection; there is no per-account connection pool.
All watchers for an account share its synchronization request rate limit.
`database.max_connections` controls the separate PostgreSQL pool.

A watcher selects its mailbox, requires UIDVALIDITY, synchronizes UIDs, then
waits in IDLE. An explicit IDLE rejection switches to polling on the same selected
connection. Transport/protocol failures reconnect. Heartbeats run an incremental
sync even without a notification. When leaving active IDLE, the client sends DONE and drains the matching tagged
completion before reusing the connection; no DONE is needed if the server already
completed IDLE. Drain failure forces reconnect.
Command, drain, inactivity, and fetch-drain deadlines are configurable per account.
Aggregate result/body limits bound each IMAP command's retained data.

Emails are identified by account, mailbox, UIDVALIDITY, and UID. A generation
change resets synchronization for the new generation while preserving earlier
emails, events, jobs, and raw files. New raw messages use
`{data_dir}/{account}/{mailbox}/uv_{uidvalidity}/{uid}.eml`; existing paths remain
valid after migration. Filesystem writes precede the metadata transaction, so a
failed SQL insert can leave an unreferenced raw file. The filesystem and database
are not one atomic transaction.

UID responses are sorted and deduplicated. Metadata and the `email_arrived` event
are inserted in one PostgreSQL transaction, with NOTIFY delivered after commit.
A failed write aborts the batch before checkpoint advancement. Successfully
stored messages are deduplicated on retry. Checkpoints use checked UID arithmetic;
a maximum UID waits for a new generation. Initial-scan filters still checkpoint
an excluded mailbox so later cycles can ingest new arrivals without repeating the
initial scan. Flag-change and deletion detection remain deferred.

Subjects and sender metadata are bounded; raw `.eml` files retain full content.
SELECT's message count is diagnostic evidence, not a completeness guarantee for
a mailbox that can change during synchronization.

## Event dispatch and processors

LISTEN/NOTIFY lowers latency; periodic polling recovers missed notifications.
Events remain undispatched until the scheduler atomically registers jobs for all
currently eligible processors and sets `events.dispatched_at`. Zero-match events
are marked dispatched too. Unique `(event_id, processor_name)` jobs and atomic
claims deduplicate notification/poll overlap.

The registry supports `logger` and `command`. An arbitrary processor name also
selects the command implementation when its `config` contains `command`.
Unknown names without that key are logged and skipped. There is no built-in
HTTP/webhook processor: use an executable that reads the stdin event JSON and
performs the HTTP operation.

For example, the following is a command processor (install its executable first):

```toml
[[processors]]
name = "mail-webhook"
enabled = true
events = ["email_arrived"]
max_retries = 3
retry_backoff_secs = [5, 30, 300]
timeout_secs = 30
concurrency = 2
[processors.config]
command = "/usr/local/bin/mail-webhook"
[processors.config.env]
WEBHOOK_URL = "https://example.com/hooks/new-email"
WEBHOOK_TOKEN = "${WEBHOOK_TOKEN}"
```

Source filters and event subscriptions apply to dispatch, retries, replay, and
backfill. The scheduler shares a positive semaphore limit per processor across
new work and retries. It bounds batch workers at 16; channels provide backpressure
and pending work remains durable in PostgreSQL. Slow work does not block unrelated
processors. Backfill/replay are separate invocations, so the daemon's semaphore is
not a global limit across those processes.

Command processors receive only default PATH/LANG and explicitly configured
environment entries (plus SystemRoot on Windows). They read event/email JSON on
stdin. Successful stdout can contain ProcessorOutput JSON, including namespaced
metrics and metadata; stderr is for logs. Nonzero exit means retryable failure.
Cancellation/timeouts terminate the child process group, including descendants.
Downstream actions must be idempotent because execution is at least once.

## Persistent state and retention

The schema is built by applying all migrations, rather than copying a standalone
DDL fragment from this document:

| Table | Purpose |
| --- | --- |
| `mailbox_states` | Per-mailbox UIDVALIDITY, checkpoint, and sync timestamps |
| `emails` | Headers, flags, booking/email date and raw-message path; generation-aware UID identity |
| `events` | Event payload, source identifiers and `dispatched_at` |
| `processor_jobs` | Unique event/processor job, status, attempts, output, retry time, claim version and execution lease |

The undispatched-event index filters `dispatched_at IS NULL`; there is no
processor-cursor table. Job states are `pending`, `in_progress`, `completed`,
`failed`, and `abandoned`. A claim increments a version and carries a lease lasting
the processor timeout plus 30 seconds. Result writes require the same claim
version, fencing late attempts. Every ten seconds, the scheduler recovers expired
claims and old in-progress jobs without leases, and executes pending/due jobs.

Replay uses the same atomic leased claim. It may re-run a terminal job, but an
active claim is skipped, so replay cannot race the retry sweep into executing the
same current claim. External side effects can still repeat if a crash occurs
after the downstream write and before recording success. Stop older daemon
versions before upgrading because they do not participate in leases.

Hourly retention selects old dispatched events with only terminal jobs, locks
them, removes terminal child jobs before their parent events, and rechecks for
remaining children. Undispatched events and nonterminal work are preserved.
Retention does not delete email metadata or raw files.

## Supervision and failures

Account managers and mailbox watchers each have independent restart supervisors.
Errors, panics, and unexpected clean exits trigger a five-second delay. Ten
failures in a sliding five-minute window trigger a fifteen-minute cooldown;
shutdown cancels both delay and cooldown. A failed mailbox does not restart its
healthy peers. This failure window counts task exits; connection errors handled
inside the watcher's retry loop use its own exponential backoff with jitter.

Authentication failures are typed and redacted. With a credential file, rejected
sessions wait for a valid update; valid rotation reconnects only that mailbox.
Temporary missing/invalid updates preserve a healthy session. With legacy
credentials, the watcher exits and its supervisor retries after the normal delay;
changing environment-based configuration requires a daemon restart. Logs do not
include server-provided LOGIN rejection text or private account/mailbox labels
on authentication-failure paths.

Database and filesystem failures propagate before the affected sync checkpoint is
advanced. The next cycle retries. Processor failures follow their configured
backoff and eventually become abandoned. The daemon does not promise that every
task pauses synchronously while PostgreSQL is unavailable.

## Shutdown and observability

SIGINT/SIGTERM cancels the root token. Watcher shutdown tries to drain IDLE and
LOGOUT, while task supervisors cancel their outstanding work. The scheduler
drains active batches within `shutdown_grace_period_secs` (default ten seconds).
At the grace deadline, remaining tasks are aborted. In-progress jobs are not
blindly reset during shutdown: expired leases recover them in a subsequent
scheduler sweep. A partial batch repeats from its last durable checkpoint.

The health listener binds to loopback by default, can require a bearer token on
metrics, and shares a request budget across its endpoints. Metrics describe
recorded counters/gauges only; the previously advertised active-connection gauge
was never recorded and has been removed. See the README's current metric table.
The systemd unit uses notification readiness; watchdog signaling is not enabled.
