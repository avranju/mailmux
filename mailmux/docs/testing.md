# Testing the workspace

Run formatting, build the actual command binaries, and execute both ordinary and
ignored integration tests from the workspace root:

```bash
cargo fmt --all --check
cargo build --workspace --bins --locked
DATABASE_URL=postgres://postgres@127.0.0.1:15432/postgres \
  cargo test --workspace --locked -- --include-ignored
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Use a disposable PostgreSQL server; SQLx tests create and migrate their own test
databases. The configured database user must be able to create databases. A local
example (choose an unused name/port) is:

```bash
docker run --rm -d --name mailmux-test-postgres \
  -e POSTGRES_HOST_AUTH_METHOD=trust \
  -p 127.0.0.1:15432:5432 postgres:18
docker exec mailmux-test-postgres pg_isready
# After the commands above:
docker stop mailmux-test-postgres
```

Wait for `pg_isready` to succeed before testing. These credentials are for the
isolated local test container, never a deployed database. Ordinary `cargo test
--workspace` excludes tests marked ignored because they require PostgreSQL or a
built Mailtx executable. `--include-ignored` is required for the complete suite.
The [test workflow](../../.github/workflows/test.yml) provisions PostgreSQL and
runs this complete sequence on pushes and pull requests.

The real stdout protocol test resolves `mailtx` beside the test target's `deps`
directory, matching the standard debug/release layout. For another location, set
`MAILMUX_TEST_MAILTX_BIN` to the absolute built executable path. A missing binary
fails that test with the build command rather than silently skipping coverage.

## Coverage of high-risk paths

| Area | Regression evidence |
| --- | --- |
| Scheduler backoff/abandonment | `retry_schedule_honors_future_deadline_clamps_backoff_and_abandons` verifies first and clamped delays, no early execution, and terminal attempt limit |
| Retention SQL and foreign keys | `cleanup_deletes_terminal_jobs_before_old_events`, typed interval overflow, and partially dispatched event preservation |
| Job transitions/replay races | `replay_and_sweep_claims_are_mutually_exclusive`, concurrent claim/abandon tests, expired-lease recovery and late-result fencing |
| Sync checkpoints | `failed_insert_cannot_be_checkpointed_and_is_retried` injects a constraint failure; generation, empty-filter, and max-UID tests cover recovery boundaries |
| NOTIFY/poll and channel capacity | `notify_and_initial_poll_overlap_register_only_one_durable_job` uses real LISTEN/NOTIFY and atomic dispatch registration; `cancellation_interrupts_a_full_dispatch_channel_without_losing_event` exercises backpressure and shutdown |
| Circuit breaker/supervision | Sliding-window expiry, ordinary errors, panic, unexpected clean exit, healthy-peer isolation, cooldown cancellation |
| IMAP connection state | Local TCP servers verify greeting/login, SELECT UIDVALIDITY, unordered FETCH/SEARCH, IDLE keepalives/tagged completion, rotation, persistent polling, typed failures and response budgets |
| Pending transfer lifecycle | FIFO insert/match/delete/expiry across reopen, currency isolation, legacy migrations/identity, duplicate events and cleanup failure |
| Processor stdout contract | `real_mailtx_stdout_roundtrips_on_success_and_failure` executes the built Mailtx binary through Mailmux CommandProcessor and checks typed metadata/metrics for zero and nonzero exits |
| Expired leg flushing | `expired_legs_post_before_delete_and_failed_posts_remain_for_next_process` checks the pending row during HTTP POST, failed POST retention and retry in a new process; successful cleanup-failure/replay tests prevent duplicate writes |

Mailtx pipeline tests run real binary processes against local mock HTTP servers.
They additionally cover sender authentication, token isolation, category-cache
reuse, exact decimal money, native currencies/timezones, payee/tags, durable manual
review, balance drift and periodic reconciliation. These do not contact real
banks, model providers, or a live Firefly installation.

CommandProcessor and the scheduler both apply `timeout_secs`. Whichever deadline
fires first cancels the child; the command's inner timeout also covers stdin
writing. Unix process-group cleanup covers descendants. Tests exercise both
inner timeouts and external scheduler-style cancellation rather than assuming
one timeout always wins the race.
