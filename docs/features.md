# Detailed Features List

> Each feature is tagged with a target phase:
> - **Phase 1** (Foundation): Essential for a working skeleton
> - **Phase 2** (Ingestion): Core IMAP sync and storage
> - **Phase 3** (Events & Processing): Event loop and processor execution
> - **Phase 4** (Refinement): IDLE, retries, polish
> - **Future**: Aspirational / deferred until core is stable

---

## 1. Core Application Architecture

### 1.1 Service Model — Phase 1

* Long-running daemon / service
* Graceful startup and shutdown
* Configurable concurrency limits (global and per account)
* Structured logging (JSON logs recommended)
* Health and readiness signals (for systemd / containers)

### 1.2 Configuration Management — Phase 1

* Single primary configuration file in **TOML**
* Support for:
  * Multiple IMAP accounts
  * Multiple mailboxes per account
  * Plugin (processor) definitions and settings
* Config reload: restart-required is the default approach; hot-reload is a **Future** enhancement
* Configuration validation with clear error reporting

---

## 2. IMAP Account Management

### 2.1 Account Definition — Phase 2

* Support multiple IMAP accounts
* Per-account configuration:
  * Server hostname
  * Port
  * TLS / STARTTLS settings
  * Authentication method
  * Credentials (password, app password)
* Connection timeout and retry policies
* **OAuth/XOAUTH2 — Future.** Token refresh flows, provider-specific registration, and secure token storage add significant complexity. App passwords work with Gmail and Outlook and are the recommended starting point.

### 2.2 Connection Handling — Phase 2

* Connection pooling or reuse per account
* Automatic reconnect on network failure
* Backoff strategy on repeated failures
* IMAP IDLE support — Phase 4

### 2.3 Rate Limiting — Phase 2

* Per-account IMAP operation rate limiting (fetches per second, commands per minute)
* Per-account connection limits
* Providers like Gmail throttle or temporarily ban aggressive IMAP clients; rate limiting is essential for real-world reliability

---

## 3. Mailbox Monitoring

### 3.1 Mailbox Selection — Phase 2

* Monitor one or more mailboxes per account
* Support standard folders (INBOX, Sent, Archive, etc.)
* Ability to include/exclude mailboxes explicitly
* **Wildcard or prefix-based mailbox selection — Future (nice-to-have).** IMAP LIST with wildcards has provider-specific behavior.

### 3.2 Change Detection — Phase 2 / Phase 4

* Detect:
  * New message arrival — Phase 2
  * Message flag changes (read/unread, starred, etc.) — Phase 3
  * Message deletions — Phase 3
  * Mailbox resets or expunges — Phase 4
* Support both:
  * Polling mode — Phase 2
  * IMAP IDLE mode (if supported by server) — Phase 4

### 3.3 Sync Strategy — Phase 2

* **Primary strategy: UID-based sync**
  * UID validity tracking
  * UID tracking (last seen UID per mailbox)
  * Recovery from desyncs (UIDVALIDITY change)
* MODSEQ / CONDSTORE — Phase 4 (if available). Most IMAP servers don't support this; UID-based sync is the primary mechanism.
* **Initial sync bounds:** Configurable limits on first sync (e.g., sync only messages from the last N days, or last N messages). Without bounds, a large mailbox (100K+ messages) could take hours and consume enormous storage.

---

## 4. Message Ingestion & Storage

### 4.1 Message Retrieval — Phase 2

* Fetch full RFC 5322 message bodies
* Parse and extract:
  * Headers
  * Body (plain + HTML)
  * Attachments (optional: inline vs deferred)
* Configurable size limits per message or attachment

### 4.2 Local Persistence (PostgreSQL) — Phase 2

* **Storage strategy decision:** Raw RFC 5322 messages and attachments should be stored on the filesystem (or object storage in the future), with only metadata and parsed headers stored in PostgreSQL. This prevents DB bloat — a mailbox with a few thousand messages with attachments can easily reach tens of gigabytes.
* Store message metadata with:
  * Account reference
  * Mailbox reference
  * UID / Message-ID
  * Timestamps
  * Flags / state
  * Filesystem path to raw message
* Deduplication by Message-ID

### 4.3 Data Integrity — Phase 2

* Transactional writes
* Idempotent message ingestion
* Schema migration support

---

## 5. Event System

### 5.1 Event Types — Phase 3

* Message arrived
* Message updated (flags/state)
* Message deleted
* Mailbox sync completed
* Mailbox error / account error — Future

### 5.2 Event Model — Phase 3

* Each event has:
  * Unique event ID
  * Timestamp
  * Event type
  * Associated message/mailbox/account
  * Event payload (structured JSON)
* **Events are created atomically with the data change that triggers them** (e.g., email insert + event insert in the same DB transaction). Events are persisted before processing.

---

## 6. Processor / Plugin System

### 6.1 Processor Definition — Phase 3

* Processors are configured in TOML
* Each processor has:
  * Name / ID
  * Enabled/disabled state
  * Subscribed event types
  * Processor-specific configuration

### 6.2 Processor Interface — Phase 3 / Future

* Well-defined contract:
  * Input: event payload + message metadata
  * Output: success / failure + optional metadata
* **Primary model: Trait-based compiled-in processors (Phase 3).** These are idiomatic Rust, require no ABI management, and are the simplest path to a working system.
* **Out-of-process plugins (CLI / HTTP) — Phase 4.** This is simpler than shared libraries and more practical for third-party extensibility.
* **In-process shared library plugins (libloading) — Future.** This is complex in Rust (ABI management, limits plugins to Rust/C-compatible). Defer indefinitely; the compiled-in + out-of-process model covers all practical use cases.

### 6.3 Processor Execution — Phase 3

* Processors invoked asynchronously
* Configurable execution order (per event type)
* Per-processor concurrency limits
* Execution timeouts

---

## 7. Processing State & Retry Management

### 7.1 Processing State Tracking — Phase 3

* Track processing status per:
  * Event
  * Processor
* Status states:
  * Pending
  * In-progress
  * Succeeded
  * Failed
  * Abandoned (after max retries)

### 7.2 Retry Logic — Phase 4

* Configurable retry policy per processor:
  * Max retries
  * Retry delay / backoff strategy
* Retry isolation (one processor failing does not block others)
* Dead-letter state for permanently failed events

### 7.3 Idempotency — Phase 3

* Ensure processors can safely receive retried events
* Event delivery guarantees:
  * At-least-once processing
  * No silent drops

---

## 8. Observability & Diagnostics

### 8.1 Logging — Phase 1

* Structured logs for:
  * IMAP operations
  * Sync cycles
  * Event creation
  * Processor execution
* Log levels per module

### 8.2 Metrics — Future

* Mailboxes monitored
* Messages ingested
* Events generated
* Processor success/failure counts
* Retry counts

### 8.3 Debugging Tools — Future

* Dry-run mode for processors
* Replay events from database
* Inspect per-message processing history

---

## 9. Security

### 9.1 Credential Handling — Phase 1

* No plaintext credentials in logs
* Support secret injection via env vars or secret files
* **Application-level field encryption — Future.** This adds significant complexity (key management, rotation, encrypted field querying). For a self-hosted daemon, rely on PostgreSQL-level or disk-level encryption instead, which covers the same threat model with far less effort.

### 9.2 Database Security — Phase 2

* Minimal privileges for the application DB user
* Connection pooling with limits
* **Role-based DB access — Future.** Not needed for a single-daemon architecture.

---

## 10. Operational Features

### 10.1 Lifecycle Management — Phase 1 / Phase 4

* systemd service support — Phase 4
* Container-friendly (12-factor style) — Phase 1
* Graceful shutdown with in-flight processing completion — Phase 4

### 10.2 Backup & Recovery — Future

* Database-only state recovery
* Rebuild local state from IMAP if DB is lost (configurable)
* Safe reprocessing of historical messages

---

## 11. Extensibility & Future Enhancements

### 11.1 Plugin Ecosystem — Future

* Official processor examples:
  * Webhook sender
  * Indexer (search)
  * Notification sender
  * Rule-based classifier
* Plugin versioning and compatibility checks

### 11.2 APIs & UI — Future

* Read-only HTTP API for:
  * Mailbox status
  * Message metadata
  * Processing status
* Web UI or TUI for monitoring

---

## 12. Non-Goals (Explicitly Out of Scope)

* Acting as an email client UI
* Sending mail (SMTP)
* Real-time user interaction
* Full mail search engine (unless via plugins)
* **Cross-mailbox message lifecycle tracking.** The same message moved between folders gets different UIDs; correlating them requires Message-ID matching and heuristics. This is deceptively complex and is not worth pursuing in any initial version.

---

### Final Thought

Mailmux's **core strength** is not "email syncing" but **reliable, replayable, event-driven mail processing**. The design above makes it:

* deterministic,
* auditable,
* extensible,
* and safe to run unattended.
