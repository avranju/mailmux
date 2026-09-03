//! Historical processor backfill.
//!
//! `mailmux backfill` runs exactly one configured processor against durable
//! `emails` rows selected by explicit filters. For each email a transient,
//! non-persisted `email_arrived` event (id 0) is constructed and the processor
//! is invoked through the normal `Processor` trait, so existing command
//! processors (e.g. mailtx) see the same `{event, email}` JSON shape as
//! normal processing. No `events` or `processor_jobs` rows are created or
//! modified; downstream processors are responsible for idempotency.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Utc;
use sqlx::PgPool;
use tracing::{debug, error, info, warn};

use crate::cli::BackfillArgs;
use crate::config::Config;
use crate::db;
use crate::db::emails::{
    EmailBackfillFilter, EmailRecord, count_emails_for_backfill, get_email_backfill_page,
};
use crate::db::events::Event;
use crate::processor::Processor;
use crate::processor::registry::ProcessorRegistry;

/// Keyset page size: bounds memory usage regardless of archive size.
const PAGE_SIZE: i64 = 500;

/// Log a progress line every this many completed emails.
const PROGRESS_INTERVAL: u64 = 100;

/// Final in-memory summary of one backfill invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillSummary {
    /// Emails matched by the filter, capped by `--limit`.
    pub selected: u64,
    /// Emails actually handed to the processor.
    pub processed: u64,
    /// Emails that eventually succeeded.
    pub succeeded: u64,
    /// Emails that permanently failed after all attempts.
    pub failed: u64,
    /// Selected emails never processed (fail-fast stop or selection drift).
    pub skipped: u64,
    /// Wall-clock duration of the run.
    pub elapsed: Duration,
}

/// Processor timeout/retry/concurrency settings copied for one invocation.
#[derive(Debug, Clone)]
pub(crate) struct ExecutionSettings {
    pub timeout: Duration,
    pub max_retries: u32,
    pub retry_backoff_secs: Vec<u64>,
    pub concurrency: usize,
    pub fail_fast: bool,
}

/// Outcome of executing one email against the selected processor.
#[derive(Debug)]
pub(crate) enum EmailExecutionResult {
    Succeeded {
        email_id: i64,
        attempts: u32,
    },
    Failed {
        email_id: i64,
        attempts: u32,
        error: String,
    },
}

/// Run the historical backfill command.
///
/// Validates the arguments, resolves the single configured processor,
/// connects to the database, streams matching emails in ascending `id` order,
/// and executes them with bounded concurrency. Returns the final summary; an
/// `Err` (after the summary has been logged) means at least one email
/// permanently failed, so the process exits non-zero.
pub async fn run(config: Config, args: BackfillArgs) -> Result<BackfillSummary> {
    args.validate()?;

    // Resolve the processor by exact configured name before any database
    // work so misconfiguration fails fast.
    let processor_config = config
        .processors
        .iter()
        .find(|c| c.name == args.processor)
        .with_context(|| format!("processor '{}' is not configured", args.processor))?;

    if !processor_config.enabled {
        bail!(
            "processor '{}' is disabled in configuration",
            args.processor
        );
    }

    let registry = ProcessorRegistry::from_config(&config.processors);
    let processor = registry.processor_by_name(&args.processor).ok_or_else(|| {
        anyhow::anyhow!(
            "processor '{}' is not available in the runtime registry \
                 (unknown processor type or missing 'command' config)",
            args.processor
        )
    })?;

    if !processor
        .subscribed_events()
        .iter()
        .any(|e| e == "email_arrived")
    {
        bail!(
            "processor '{}' is not subscribed to event type 'email_arrived'",
            args.processor
        );
    }

    let concurrency = args
        .concurrency
        .unwrap_or(processor_config.concurrency as usize);
    if concurrency == 0 {
        bail!(
            "backfill concurrency must be greater than zero \
             (processor '{}' concurrency = {}, --concurrency = {})",
            args.processor,
            processor_config.concurrency,
            args.concurrency
                .map_or_else(|| "none".to_string(), |v| v.to_string())
        );
    }

    let settings = ExecutionSettings {
        timeout: Duration::from_secs(processor_config.timeout_secs),
        max_retries: processor_config.max_retries,
        retry_backoff_secs: processor_config.retry_backoff_secs.clone(),
        concurrency,
        fail_fast: args.fail_fast,
    };

    let filter = EmailBackfillFilter {
        after: args.after,
        before: args.before,
        accounts: args.accounts.clone(),
        mailboxes: args.mailboxes.clone(),
        email_ids: args.email_ids.clone(),
    };

    let pool = db::connect(&config.database).await?;
    db::run_migrations(&pool).await?;

    info!(
        processor = %args.processor,
        after = ?filter.after,
        before = ?filter.before,
        accounts = ?filter.accounts,
        mailboxes = ?filter.mailboxes,
        email_ids = ?filter.email_ids,
        all = args.all,
        limit = ?args.limit,
        concurrency,
        max_retries = settings.max_retries,
        timeout_secs = settings.timeout.as_secs(),
        fail_fast = settings.fail_fast,
        dry_run = args.dry_run,
        "backfill starting"
    );

    let matched = count_emails_for_backfill(&pool, &filter).await?;
    let selected = cap_selected(matched, args.limit);

    let mut source = DbBackfillPageSource {
        pool: &pool,
        filter: &filter,
    };
    let summary = run_backfill(processor, &mut source, selected, args.dry_run, &settings).await?;

    pool.close().await;

    if summary.failed > 0 {
        // The summary is already logged above; surface a non-zero exit.
        return Err(anyhow::anyhow!(
            "backfill finished with {}/{} selected emails permanently failed",
            summary.failed,
            summary.selected
        ));
    }

    Ok(summary)
}

/// Cap the matched count by the optional `--limit` (ignoring negative counts).
fn cap_selected(matched: i64, limit: Option<u64>) -> u64 {
    let matched = u64::try_from(matched.max(0)).unwrap_or(u64::MAX);
    match limit {
        Some(limit) => matched.min(limit),
        None => matched,
    }
}

/// Drive the keyset pagination + bounded execution loop for one backfill
/// invocation and return the final in-memory summary.
///
/// `selected` is the caller-computed (and `--limit`-capped) number of
/// matching emails. No database state is written.
async fn run_backfill(
    processor: &dyn Processor,
    source: &mut impl BackfillPageSource,
    selected: u64,
    dry_run: bool,
    settings: &ExecutionSettings,
) -> Result<BackfillSummary> {
    let started = Instant::now();

    if selected == 0 {
        warn!("backfill selected 0 emails; nothing to do");
        let summary = BackfillSummary {
            elapsed: started.elapsed(),
            ..Default::default()
        };
        log_summary(&summary);
        return Ok(summary);
    }

    if dry_run {
        info!(
            selected,
            "backfill dry-run: selection counted; \
             no emails resolved and no processor invoked"
        );
        let summary = BackfillSummary {
            selected,
            elapsed: started.elapsed(),
            ..Default::default()
        };
        log_summary(&summary);
        return Ok(summary);
    }

    let mut summary = BackfillSummary {
        selected,
        ..Default::default()
    };
    let mut last_id: i64 = 0;
    let mut remaining: u64 = selected;
    let mut last_progress: u64 = 0;

    while remaining > 0 {
        let page_size = std::cmp::min(PAGE_SIZE, remaining as i64);
        let page = source
            .next_page(last_id, page_size)
            .await
            .with_context(|| format!("fetching backfill page after id {last_id}"))?;

        if page.is_empty() {
            // The initial count and this live page are separate queries, so
            // concurrent deletion can shrink the selection mid-run. Report
            // the difference as skipped rather than failing the run.
            warn!(
                remaining,
                "backfill selection returned no more matching emails; \
                 counting the remainder as skipped"
            );
            summary.skipped += remaining;
            break;
        }

        last_id = page[page.len() - 1].id;

        let mut stop = false;
        for chunk in page.chunks(settings.concurrency) {
            let chunk_results = execute_email_chunk(processor, chunk, settings).await;
            let mut chunk_failed = false;
            for result in chunk_results {
                match result {
                    EmailExecutionResult::Succeeded { email_id, attempts } => {
                        summary.processed += 1;
                        summary.succeeded += 1;
                        debug!(email_id, attempts, "backfill email succeeded");
                    }
                    EmailExecutionResult::Failed {
                        email_id,
                        attempts,
                        error: error_msg,
                    } => {
                        summary.processed += 1;
                        summary.failed += 1;
                        chunk_failed = true;
                        error!(
                            email_id,
                            attempts,
                            error = %error_msg,
                            "backfill email permanently failed"
                        );
                    }
                }
            }

            let next_progress = last_progress + PROGRESS_INTERVAL;
            if summary.processed >= next_progress {
                last_progress = next_progress;
                info!(
                    processed = summary.processed,
                    selected,
                    succeeded = summary.succeeded,
                    failed = summary.failed,
                    "backfill progress"
                );
            }

            if settings.fail_fast && chunk_failed {
                warn!(
                    processed = summary.processed,
                    selected, "backfill fail-fast: stopping before the next chunk"
                );
                summary.skipped = selected.saturating_sub(summary.processed);
                stop = true;
                break;
            }
        }
        if stop {
            break;
        }

        remaining = remaining.saturating_sub(page.len() as u64);
    }

    summary.elapsed = started.elapsed();
    log_summary(&summary);
    Ok(summary)
}

fn log_summary(summary: &BackfillSummary) {
    info!(
        selected = summary.selected,
        processed = summary.processed,
        succeeded = summary.succeeded,
        failed = summary.failed,
        skipped = summary.skipped,
        elapsed = ?summary.elapsed,
        "backfill summary"
    );
}

/// Supplies keyset pages of matching emails to the backfill loop.
#[async_trait]
trait BackfillPageSource {
    /// Fetch at most `page_size` emails with `id > last_id`, ascending id.
    async fn next_page(&mut self, last_id: i64, page_size: i64) -> Result<Vec<EmailRecord>>;
}

struct DbBackfillPageSource<'a> {
    pool: &'a PgPool,
    filter: &'a EmailBackfillFilter,
}

#[async_trait]
impl BackfillPageSource for DbBackfillPageSource<'_> {
    async fn next_page(&mut self, last_id: i64, page_size: i64) -> Result<Vec<EmailRecord>> {
        get_email_backfill_page(self.pool, self.filter, last_id, page_size).await
    }
}

/// Execute one contiguous chunk of at most `settings.concurrency` emails
/// concurrently, returning one result per email in input order.
///
/// The caller splits pages into deterministic contiguous chunks and awaits
/// one chunk at a time, so `--fail-fast` can stop before the next chunk is
/// ever scheduled and in-flight work never exceeds the effective concurrency.
async fn execute_email_chunk(
    processor: &dyn Processor,
    chunk: &[EmailRecord],
    settings: &ExecutionSettings,
) -> Vec<EmailExecutionResult> {
    let futures = chunk
        .iter()
        .map(|email| execute_email_with_retries(processor, email, settings));
    futures::future::join_all(futures).await
}

/// Invoke one processor/email pair with a per-attempt timeout and the
/// configured retry schedule. No database state is read or written.
async fn execute_email_with_retries(
    processor: &dyn Processor,
    email: &EmailRecord,
    settings: &ExecutionSettings,
) -> EmailExecutionResult {
    let event = build_backfill_event(email);
    let max_attempts = settings.max_retries.saturating_add(1);
    let mut last_error = String::new();

    for attempt in 1..=max_attempts {
        match tokio::time::timeout(settings.timeout, processor.process(&event, Some(email))).await {
            Ok(Ok(output)) if output.success => {
                return EmailExecutionResult::Succeeded {
                    email_id: email.id,
                    attempts: attempt,
                };
            }
            Ok(Ok(output)) => {
                last_error = output
                    .message
                    .unwrap_or_else(|| "processor reported failure".to_string());
            }
            Ok(Err(e)) => {
                last_error = e.to_string();
            }
            Err(_) => {
                last_error = format!("processor timed out after {:?}", settings.timeout);
            }
        }

        if attempt < max_attempts {
            let delay_secs = retry_delay_secs(settings, attempt - 1);
            warn!(
                processor = processor.name(),
                email_id = email.id,
                attempt,
                max_attempts,
                error = %last_error,
                retry_delay_secs = delay_secs,
                "backfill attempt failed; retrying"
            );
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        }
    }

    EmailExecutionResult::Failed {
        email_id: email.id,
        attempts: max_attempts,
        error: last_error,
    }
}

/// Retry delay for a zero-based retry index: the schedule value at that
/// index, clamped to the last schedule entry, or 60 s when the schedule is
/// empty.
fn retry_delay_secs(settings: &ExecutionSettings, retry_index: u32) -> u64 {
    let schedule = &settings.retry_backoff_secs;
    match schedule.get(retry_index as usize) {
        Some(delay) => *delay,
        None => schedule.last().copied().unwrap_or(60),
    }
}

/// Construct the transient, non-persisted `email_arrived` event passed to
/// the processor for one historical email.
///
/// `id` is 0, reserved for non-persisted backfill invocations (see
/// `crate::db::events::Event`). The payload explicitly marks the event as a
/// backfill so downstream processors can distinguish historical invocations.
pub fn build_backfill_event(email: &EmailRecord) -> Event {
    Event {
        id: 0,
        event_type: "email_arrived".to_string(),
        account_id: email.account_id.clone(),
        mailbox_name: email.mailbox_name.clone(),
        email_id: Some(email.id),
        payload: serde_json::json!({
            "backfill": true,
            "email_id": email.id,
            "uid": email.uid,
            "subject": email.subject,
            "sender": email.sender,
        }),
        created_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::config::{DatabaseConfig, GeneralConfig, ProcessorConfig};
    use crate::db::emails::EmailRecord;
    use crate::processor::ProcessorOutput;

    // ---------- helpers ----------

    fn sample_email(id: i64) -> EmailRecord {
        let now = Utc::now();
        EmailRecord {
            id,
            account_id: "personal".into(),
            mailbox_name: "INBOX".into(),
            uid: id * 1000,
            message_id: Some(format!("<{id}@example.com>")),
            subject: Some(format!("subject {id}")),
            sender: Some(format!("sender{id}@example.com")),
            recipients: Some(serde_json::json!([{"addr": "rcpt@example.com"}])),
            date: Some(now - chrono::Duration::days(1)),
            flags: vec!["\\Seen".into()],
            raw_message_path: format!("/var/lib/mailmux/personal/INBOX/{id}.eml"),
            size_bytes: Some(1234),
            created_at: now,
            updated_at: now,
        }
    }

    fn test_config(processors: Vec<ProcessorConfig>) -> Config {
        Config {
            general: GeneralConfig {
                data_dir: "/tmp/mailmux-test".into(),
                log_level: "error".into(),
                log_format: "json".into(),
                shutdown_grace_period_secs: 1,
                health_port: None,
                event_retention_days: 30,
            },
            database: DatabaseConfig {
                // Never dialed: every run() test aborts before connecting.
                url: "postgres://unused@localhost/unused".into(),
                max_connections: 1,
            },
            accounts: vec![],
            processors,
        }
    }

    fn test_processor_config(
        name: &str,
        enabled: bool,
        events: &[&str],
        max_retries: u32,
    ) -> ProcessorConfig {
        ProcessorConfig {
            name: name.into(),
            enabled,
            events: events.iter().map(|s| s.to_string()).collect(),
            max_retries,
            retry_backoff_secs: vec![],
            timeout_secs: 30,
            concurrency: 1,
            config: HashMap::new(),
        }
    }

    fn test_backfill_args(processor: &str) -> BackfillArgs {
        BackfillArgs {
            processor: processor.into(),
            after: None,
            before: None,
            accounts: vec![],
            mailboxes: vec![],
            email_ids: vec![],
            all: true,
            limit: None,
            concurrency: None,
            fail_fast: false,
            dry_run: false,
        }
    }

    fn test_settings(concurrency: usize, max_retries: u32, backoff: Vec<u64>) -> ExecutionSettings {
        ExecutionSettings {
            timeout: Duration::from_secs(30),
            max_retries,
            retry_backoff_secs: backoff,
            concurrency,
            fail_fast: false,
        }
    }

    /// Emulates the database keyset semantics (id > last_id, ASC, LIMIT) over
    /// an in-memory ascending slice.
    struct SlicePageSource {
        emails: Vec<EmailRecord>,
        polls: usize,
    }

    impl SlicePageSource {
        fn new(emails: Vec<EmailRecord>) -> Self {
            Self { emails, polls: 0 }
        }
    }

    #[async_trait]
    impl BackfillPageSource for SlicePageSource {
        async fn next_page(&mut self, last_id: i64, page_size: i64) -> Result<Vec<EmailRecord>> {
            self.polls += 1;
            Ok(self
                .emails
                .iter()
                .filter(|e| e.id > last_id)
                .take(page_size as usize)
                .cloned()
                .collect())
        }
    }

    /// Tracks call counts and concurrent activity for assertions.
    #[derive(Default, Clone)]
    struct CallStats {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        seen_ids: Arc<Mutex<Vec<i64>>>,
    }

    impl CallStats {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }

        fn seen_ids(&self) -> Vec<i64> {
            self.seen_ids.lock().unwrap().clone()
        }
    }

    struct ActiveGuard {
        stats: CallStats,
    }

    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.stats.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    enum FakeBehavior {
        Succeed,
        /// Fail this many consecutive calls (global), then succeed.
        FailFirstN(Arc<AtomicUsize>),
        /// Always fail.
        FailAlways,
        /// Permanently fail the given email ids.
        FailEmails(Vec<i64>),
        /// Return an Err on every call.
        ReturnError(String),
    }

    struct FakeProcessor {
        name: String,
        events: Vec<String>,
        stats: CallStats,
        delay: Duration,
        behavior: FakeBehavior,
    }

    impl FakeProcessor {
        fn new(name: &str, behavior: FakeBehavior, delay: Duration) -> Self {
            Self {
                name: name.to_string(),
                events: vec!["email_arrived".to_string()],
                stats: CallStats::default(),
                delay,
                behavior,
            }
        }

        fn succeed(name: &str, delay: Duration) -> Self {
            Self::new(name, FakeBehavior::Succeed, delay)
        }
    }

    #[async_trait::async_trait]
    impl Processor for FakeProcessor {
        fn name(&self) -> &str {
            &self.name
        }

        fn subscribed_events(&self) -> &[String] {
            &self.events
        }

        async fn process(
            &self,
            _event: &Event,
            email: Option<&EmailRecord>,
        ) -> Result<ProcessorOutput> {
            let email_id = email.map(|e| e.id).unwrap_or(0);
            let active = self.stats.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.stats.peak.fetch_max(active, Ordering::SeqCst);
            let _guard = ActiveGuard {
                stats: self.stats.clone(),
            };
            self.stats.calls.fetch_add(1, Ordering::SeqCst);
            self.stats.seen_ids.lock().unwrap().push(email_id);

            if self.delay > Duration::ZERO {
                tokio::time::sleep(self.delay).await;
            }

            let fail = match &self.behavior {
                FakeBehavior::Succeed => false,
                FakeBehavior::FailFirstN(remaining) => remaining.fetch_sub(1, Ordering::SeqCst) > 0,
                FakeBehavior::FailAlways => true,
                FakeBehavior::FailEmails(ids) => ids.contains(&email_id),
                FakeBehavior::ReturnError(msg) => {
                    return Err(anyhow::anyhow!("{msg}"));
                }
            };

            if fail {
                Ok(ProcessorOutput {
                    success: false,
                    message: Some("fake failure".into()),
                    metadata: None,
                    metrics: vec![],
                })
            } else {
                Ok(ProcessorOutput {
                    success: true,
                    message: None,
                    metadata: None,
                    metrics: vec![],
                })
            }
        }
    }

    // ---------- transient event construction ----------

    #[test]
    fn test_build_backfill_event_shape() {
        let before = Utc::now();
        let email = sample_email(42);
        let event = build_backfill_event(&email);
        let after = Utc::now();

        assert_eq!(event.id, 0, "transient backfill events must use id 0");
        assert_eq!(event.event_type, "email_arrived");
        assert_eq!(event.account_id, "personal");
        assert_eq!(event.mailbox_name, "INBOX");
        assert_eq!(event.email_id, Some(42));
        assert!(
            before <= event.created_at && event.created_at <= after,
            "created_at must be the current UTC time"
        );

        let payload = &event.payload;
        assert_eq!(payload.get("backfill"), Some(&serde_json::json!(true)));
        assert_eq!(payload.get("email_id"), Some(&serde_json::json!(42)));
        assert_eq!(payload.get("uid"), Some(&serde_json::json!(42_000)));
        assert_eq!(
            payload.get("subject"),
            Some(&serde_json::json!("subject 42"))
        );
        assert_eq!(
            payload.get("sender"),
            Some(&serde_json::json!("sender42@example.com"))
        );
    }

    // ---------- processor resolution errors ----------

    #[tokio::test]
    async fn test_lookup_rejects_missing_configured_processor() {
        let config = test_config(vec![test_processor_config(
            "logger",
            true,
            &["email_arrived"],
            0,
        )]);
        let err = run(config, test_backfill_args("ghost")).await.unwrap_err();
        assert!(err.to_string().contains("not configured"), "{err}");
    }

    #[tokio::test]
    async fn test_lookup_rejects_enabled_config_absent_from_runtime_registry() {
        // An unknown processor type without a "command" key is skipped by the
        // registry, so the enabled config never becomes a runtime processor.
        let config = test_config(vec![test_processor_config(
            "mystery",
            true,
            &["email_arrived"],
            0,
        )]);
        let err = run(config, test_backfill_args("mystery"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("runtime registry"), "{err}");
    }

    #[tokio::test]
    async fn test_rejects_disabled_processor() {
        let config = test_config(vec![test_processor_config(
            "logger",
            false,
            &["email_arrived"],
            0,
        )]);
        let err = run(config, test_backfill_args("logger")).await.unwrap_err();
        assert!(err.to_string().contains("disabled"), "{err}");
    }

    #[tokio::test]
    async fn test_rejects_processor_not_subscribed_to_email_arrived() {
        let config = test_config(vec![test_processor_config("logger", true, &[], 0)]);
        let err = run(config, test_backfill_args("logger")).await.unwrap_err();
        assert!(err.to_string().contains("email_arrived"), "{err}");
    }

    #[tokio::test]
    async fn test_rejects_zero_effective_concurrency() {
        let mut cfg = test_processor_config("logger", true, &["email_arrived"], 0);
        cfg.concurrency = 0;
        let config = test_config(vec![cfg]);
        let err = run(config, test_backfill_args("logger")).await.unwrap_err();
        assert!(err.to_string().contains("concurrency"), "{err}");
    }

    // ---------- limit ----------

    #[tokio::test]
    async fn test_limit_invokes_only_ascending_prefix() {
        let emails: Vec<EmailRecord> = (1..=10).map(sample_email).collect();
        let mut source = SlicePageSource::new(emails);
        let proc = FakeProcessor::succeed("p", Duration::ZERO);
        let stats = proc.stats.clone();
        let settings = test_settings(2, 0, vec![]);

        // selected=3 acts as min(match_count, --limit).
        let summary = run_backfill(&proc, &mut source, 3, false, &settings)
            .await
            .unwrap();

        assert_eq!(summary.selected, 3);
        assert_eq!(summary.processed, 3);
        assert_eq!(summary.succeeded, 3);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
        assert_eq!(stats.calls(), 3, "only the limit prefix may be invoked");
        assert_eq!(stats.seen_ids(), vec![1, 2, 3]);
    }

    // ---------- retries ----------

    #[tokio::test]
    async fn test_retry_succeeds_after_transient_failures() {
        let proc = FakeProcessor::new(
            "p",
            FakeBehavior::FailFirstN(Arc::new(AtomicUsize::new(2))),
            Duration::ZERO,
        );
        let stats = proc.stats.clone();
        let mut source = SlicePageSource::new(vec![sample_email(1)]);
        let settings = test_settings(1, 2, vec![0, 0]);

        let summary = run_backfill(&proc, &mut source, 1, false, &settings)
            .await
            .unwrap();

        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(stats.calls(), 3, "initial attempt + 2 retries");
    }

    #[tokio::test]
    async fn test_retry_exhaustion_is_permanent_failure() {
        let proc = FakeProcessor::new("p", FakeBehavior::FailAlways, Duration::ZERO);
        let stats = proc.stats.clone();
        let mut source = SlicePageSource::new(vec![sample_email(1)]);
        let settings = test_settings(1, 1, vec![0]);

        let summary = run_backfill(&proc, &mut source, 1, false, &settings)
            .await
            .unwrap();

        assert_eq!(summary.failed, 1);
        assert_eq!(summary.succeeded, 0);
        assert_eq!(stats.calls(), 2, "initial attempt + 1 retry");
    }

    #[tokio::test]
    async fn test_processor_error_is_retried() {
        let proc = FakeProcessor::new(
            "p",
            FakeBehavior::ReturnError("boom".into()),
            Duration::ZERO,
        );
        let stats = proc.stats.clone();
        let mut source = SlicePageSource::new(vec![sample_email(1)]);
        let settings = test_settings(1, 2, vec![0, 0]);

        let summary = run_backfill(&proc, &mut source, 1, false, &settings)
            .await
            .unwrap();

        assert_eq!(summary.failed, 1);
        assert_eq!(stats.calls(), 3, "errors are retryable attempts");
    }

    // ---------- timeouts ----------

    #[tokio::test]
    async fn test_timeouts_are_retried_then_permanently_failed() {
        // Each call sleeps far longer than the 100 ms per-attempt timeout, so
        // every attempt must be cancelled by the timeout.
        let proc = FakeProcessor::new("p", FakeBehavior::Succeed, Duration::from_secs(10));
        let stats = proc.stats.clone();
        let mut source = SlicePageSource::new(vec![sample_email(1)]);
        let mut settings = test_settings(1, 2, vec![0, 0]);
        settings.timeout = Duration::from_millis(100);

        let summary = run_backfill(&proc, &mut source, 1, false, &settings)
            .await
            .unwrap();

        assert_eq!(summary.failed, 1);
        assert_eq!(summary.succeeded, 0);
        assert_eq!(stats.calls(), 3, "every attempt must time out");
    }

    // ---------- continue-on-error vs fail-fast ----------

    #[tokio::test]
    async fn test_continue_on_error_processes_all_emails() {
        let emails: Vec<EmailRecord> = (1..=5).map(sample_email).collect();
        let mut source = SlicePageSource::new(emails);
        let proc = FakeProcessor::new("p", FakeBehavior::FailEmails(vec![3]), Duration::ZERO);
        let stats = proc.stats.clone();
        let settings = test_settings(2, 0, vec![]);

        let summary = run_backfill(&proc, &mut source, 5, false, &settings)
            .await
            .unwrap();

        assert_eq!(summary.processed, 5);
        assert_eq!(summary.succeeded, 4);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 0);
        assert_eq!(stats.calls(), 5, "later emails must still be processed");
        assert!(stats.seen_ids().contains(&5));
    }

    #[tokio::test]
    async fn test_fail_fast_stops_after_failing_chunk() {
        let emails: Vec<EmailRecord> = (1..=5).map(sample_email).collect();
        let mut source = SlicePageSource::new(emails);
        let proc = FakeProcessor::new("p", FakeBehavior::FailEmails(vec![3]), Duration::ZERO);
        let stats = proc.stats.clone();
        let mut settings = test_settings(2, 0, vec![]);
        settings.fail_fast = true;

        let summary = run_backfill(&proc, &mut source, 5, false, &settings)
            .await
            .unwrap();

        // Chunks at concurrency 2: [1,2] ok, [3,4] contains the failure -> stop.
        assert_eq!(summary.processed, 4);
        assert_eq!(summary.succeeded, 3);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1, "skipped = selected - processed");
        assert_eq!(stats.calls(), 4, "email 5 must never be scheduled");
        assert!(!stats.seen_ids().contains(&5));
    }

    // ---------- bounded concurrency ----------

    #[tokio::test]
    async fn test_concurrency_is_bounded() {
        let emails: Vec<EmailRecord> = (1..=8).map(sample_email).collect();
        let mut source = SlicePageSource::new(emails);
        let proc = FakeProcessor::new("p", FakeBehavior::Succeed, Duration::from_millis(100));
        let stats = proc.stats.clone();
        let settings = test_settings(3, 0, vec![]);

        let summary = run_backfill(&proc, &mut source, 8, false, &settings)
            .await
            .unwrap();

        assert_eq!(summary.processed, 8);
        assert_eq!(stats.calls(), 8);
        assert!(
            stats.peak() <= 3,
            "peak {} exceeded the effective concurrency 3",
            stats.peak()
        );
        assert!(
            stats.peak() > 1,
            "expected overlapping calls with 8 emails at concurrency 3"
        );
    }

    // ---------- dry run / no matches ----------

    #[tokio::test]
    async fn test_dry_run_counts_selected_without_invoking_processor() {
        let emails: Vec<EmailRecord> = (1..=10).map(sample_email).collect();
        let mut source = SlicePageSource::new(emails);
        let proc = FakeProcessor::succeed("p", Duration::ZERO);
        let stats = proc.stats.clone();
        let settings = test_settings(2, 0, vec![]);

        // selected is the caller-capped count (min(match_count, --limit)).
        let summary = run_backfill(&proc, &mut source, 4, true, &settings)
            .await
            .unwrap();

        assert_eq!(summary.selected, 4);
        assert_eq!(summary.processed, 0);
        assert_eq!(summary.succeeded, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(stats.calls(), 0, "dry-run must not invoke the processor");
        assert_eq!(source.polls, 0, "dry-run must not resolve email rows");
    }

    #[tokio::test]
    async fn test_zero_selected_short_circuits() {
        let emails: Vec<EmailRecord> = (1..=5).map(sample_email).collect();
        let mut source = SlicePageSource::new(emails);
        let proc = FakeProcessor::succeed("p", Duration::ZERO);
        let stats = proc.stats.clone();
        let settings = test_settings(1, 0, vec![]);

        let summary = run_backfill(&proc, &mut source, 0, false, &settings)
            .await
            .unwrap();

        assert_eq!(summary.selected, 0);
        assert_eq!(summary.processed, 0);
        assert_eq!(stats.calls(), 0);
        assert_eq!(source.polls, 0);
    }

    // ---------- command-processor JSON compatibility ----------

    #[test]
    fn test_command_processor_json_shape_preserved() {
        let email = sample_email(7);
        let event = build_backfill_event(&email);

        // The exact shape CommandProcessor::process writes to stdin.
        let input = serde_json::json!({
            "event": &event,
            "email": Some(&email),
        });

        assert!(input.get("event").is_some(), "top-level event key required");
        assert!(input.get("email").is_some(), "top-level email key required");
        assert_eq!(input["event"]["id"], serde_json::json!(0));
        assert_eq!(
            input["event"]["event_type"],
            serde_json::json!("email_arrived")
        );
        assert_eq!(
            input["event"]["payload"]["backfill"],
            serde_json::json!(true),
            "shell processors detect backfill via .event.payload.backfill"
        );
        assert_eq!(
            input["email"]["raw_message_path"],
            serde_json::json!("/var/lib/mailmux/personal/INBOX/7.eml"),
            "email.raw_message_path must remain present"
        );
        assert_eq!(input["email"]["subject"], serde_json::json!("subject 7"));
        assert_eq!(input["email"]["uid"], serde_json::json!(7000));
    }
}
