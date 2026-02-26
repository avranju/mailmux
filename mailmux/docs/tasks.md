# Mailmux Implementation Tasks

## Phase 1 — Foundation

- [x] **Task 1.1: Add Phase 1 dependencies**
- [x] **Task 1.2: Define config structs + loader** (`src/config.rs`)
- [x] **Task 1.3: CLI setup with clap** (`src/cli.rs`)
- [x] **Task 1.4: Structured logging setup** (`src/logging.rs`)
- [x] **Task 1.5: Database connection + migrations** (`src/db/mod.rs`, `migrations/`)
- [x] **Task 1.6: Graceful shutdown skeleton** (`src/shutdown.rs`)
- [x] **Task 1.7: Wire up main.rs** (`src/main.rs`)
- [x] **Task 1.8: Verify Phase 1**

## Phase 2 — Ingestion

- [x] **Task 2.1: Add Phase 2 dependencies** (imap-next, mail-parser, governor, etc.)
- [x] **Task 2.2: Message store (filesystem)** (`src/store.rs`)
- [x] **Task 2.3: Email metadata DB operations** (`src/db/emails.rs`, `src/db/events.rs`, `src/db/jobs.rs`)
- [x] **Task 2.4: IMAP connection + account manager** (`src/imap/connection.rs`, `src/imap/mod.rs`)
- [x] **Task 2.5: Mailbox sync logic** (`src/imap/sync.rs`)
- [x] **Task 2.6: Wire ingestion into main loop**
- [x] **Task 2.7: Verify Phase 2**

## Phase 3 — Events & Processing

- [x] **Task 3.1: Atomic event creation in ingestion** (`src/db/events.rs`)
- [x] **Task 3.2: LISTEN/NOTIFY event dispatch** (`src/events/mod.rs`, `src/events/listener.rs`)
- [x] **Task 3.3: Processor trait + registry** (`src/processor/mod.rs`, `src/processor/registry.rs`)
- [x] **Task 3.4: Job scheduler + state tracking** (`src/processor/scheduler.rs`, `src/db/jobs.rs`)
- [x] **Task 3.5: Built-in logger processor** (`src/processor/builtin/logger.rs`)
- [x] **Task 3.6: Wire events + processing into main loop**
- [x] **Task 3.7: Verify Phase 3**

## Phase 4 — Refinement

- [x] **Task 4.1: IMAP IDLE support** (`src/imap/connection.rs`, `src/imap/sync.rs`)
- [x] **Task 4.2: Retry logic + backoff** (`src/processor/scheduler.rs`)
- [x] **Task 4.3: Supervision + restart on failure** (`src/imap/mod.rs`)
- [ ] **Task 4.4: Flag change + deletion detection** (deferred — requires more IMAP protocol work)
- [x] **Task 4.5: Out-of-process processors (CLI)** (`src/processor/builtin/command.rs`)
- [x] **Task 4.6: Verify Phase 4**

## Phase 5 — Observability & Operational Hardening

- [x] **Task 5.1: Health check HTTP endpoint** (`src/health.rs`)
- [x] **Task 5.2: Prometheus metrics** (`src/metrics.rs`, integrated into health server at `/metrics`)
- [x] **Task 5.3: Event retention + cleanup** (`src/housekeeping.rs`)
- [x] **Task 5.4: Dry-run + replay tools** (`src/cli.rs`, `src/main.rs` — `replay` and `dry-run` subcommands)
- [x] **Task 5.5: systemd integration** (`contrib/mailmux.service`, `sd-notify` crate)
