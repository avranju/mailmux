use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Parser;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

mod backfill;
mod cli;
mod config;
mod db;
mod events;
mod health;
mod housekeeping;
mod imap;
mod logging;
mod metrics;
mod processor;
mod shutdown;
mod store;

#[tokio::main]
async fn main() -> Result<()> {
    // Explicitly install ring as the rustls crypto provider. Without this,
    // rustls 0.23 may fail to determine a provider automatically when multiple
    // providers are present in the dependency graph.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls crypto provider"))?;

    let cli = cli::Cli::parse();

    // Load configuration
    let config = config::Config::load(&cli.config)?;

    // Initialize logging (CLI override takes precedence)
    let log_level = cli
        .log_level
        .as_deref()
        .unwrap_or(&config.general.log_level);
    logging::init(log_level, &config.general.log_format)?;
    config.warn_unmonitored_processor_sources();

    match cli.command {
        Some(cli::Command::Replay {
            event_id,
            processor: processor_filter,
        }) => cmd_replay(config, event_id, processor_filter).await,
        Some(cli::Command::DryRun {
            event_id,
            processor: processor_name,
        }) => cmd_dry_run(config, event_id, processor_name).await,
        Some(cli::Command::Backfill(args)) => {
            // The backfill summary is logged inside backfill::run before any
            // partial-failure error is returned.
            backfill::run(config, args).await?;
            Ok(())
        }
        None => cmd_run(config).await,
    }
}

/// Main daemon run loop.
async fn cmd_run(config: config::Config) -> Result<()> {
    let enabled_accounts = config.accounts.iter().filter(|a| a.enabled).count();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        accounts = enabled_accounts,
        configured_accounts = config.accounts.len(),
        processors = config.processors.len(),
        "mailmux starting"
    );

    // Connect to database
    let pool = db::connect(&config.database).await?;
    info!("connected to database");

    // Run migrations
    db::run_migrations(&pool).await?;
    info!("database migrations complete");

    // Initialize metrics
    let metrics_handle = metrics::init();
    if metrics_handle.is_some() {
        info!("prometheus metrics initialized");
    }

    // Setup shutdown handling
    let token = CancellationToken::new();
    shutdown::spawn_signal_handler(token.clone());

    // Create message store
    let message_store = Arc::new(store::MessageStore::new(&config.general.data_dir));

    // Build processor registry
    let registry = Arc::new(processor::registry::ProcessorRegistry::from_config(
        &config.processors,
    ));

    // Setup event dispatch channel
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);

    // Spawn system tasks
    let mut system_tasks = JoinSet::new();

    // Health check server
    let health_state = health::HealthState::new(pool.clone(), metrics_handle);
    if let Some(port) = config.general.health_port {
        let hs = health_state.clone();
        let t = token.clone();
        system_tasks.spawn(async move {
            health::serve(port, hs, t).await;
        });
    }

    // Event loop (LISTEN/NOTIFY + polling)
    {
        let event_loop = events::EventLoop::new(pool.clone(), token.clone(), event_tx);
        system_tasks.spawn(async move {
            if let Err(e) = event_loop.run().await {
                error!(error = %e, "event loop exited with error");
            }
        });
    }

    // Job scheduler
    {
        let scheduler = processor::scheduler::JobScheduler::new(
            pool.clone(),
            registry,
            event_rx,
            token.clone(),
            config.processors.clone(),
        );
        system_tasks.spawn(async move {
            if let Err(e) = scheduler.run().await {
                error!(error = %e, "job scheduler exited with error");
            }
        });
    }

    // Event cleanup (housekeeping)
    {
        let p = pool.clone();
        let t = token.clone();
        let retention = config.general.event_retention_days;
        system_tasks.spawn(async move {
            if let Err(e) = housekeeping::run_event_cleanup(p, retention, t).await {
                error!(error = %e, "event cleanup exited with error");
            }
        });
    }

    // Spawn account managers
    let mut account_tasks = JoinSet::new();
    for account_config in config.accounts {
        if !account_config.enabled {
            info!(
                account = account_config.id,
                "account disabled in config; skipping"
            );
            continue;
        }
        let account_id = account_config.id.clone();
        let manager = imap::AccountManager::new(
            account_config,
            pool.clone(),
            message_store.clone(),
            token.clone(),
        );

        account_tasks.spawn(async move {
            if let Err(e) = manager.run().await {
                error!(account = account_id, error = %e, "account manager exited with error");
            }
        });
    }

    // Mark as ready after initial setup
    health_state.set_ready();

    // Notify systemd that we're ready (no-op if not running under systemd)
    let _ = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]);

    info!("mailmux is running, press Ctrl+C to stop");

    // Wait for shutdown signal
    token.cancelled().await;
    let _ = sd_notify::notify(true, &[sd_notify::NotifyState::Stopping]);
    info!("shutting down");

    // Grace period for in-flight work
    let grace = std::time::Duration::from_secs(config.general.shutdown_grace_period_secs);
    info!(grace_secs = grace.as_secs(), "waiting for grace period");
    tokio::time::sleep(grace).await;

    // Abort remaining tasks
    account_tasks.abort_all();
    system_tasks.abort_all();

    while let Some(result) = account_tasks.join_next().await {
        match result {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => warn!(error = %e, "account task error during shutdown"),
        }
    }
    while let Some(result) = system_tasks.join_next().await {
        match result {
            Ok(()) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => warn!(error = %e, "system task error during shutdown"),
        }
    }

    // Close database pool
    pool.close().await;
    info!("database connections closed");

    info!("mailmux stopped");
    Ok(())
}

/// Replay the selected eligible processors, including job reset/claim, invocation,
/// and output persistence. The command wrapper owns the database pool lifecycle.
async fn replay_event(
    pool: &sqlx::PgPool,
    configs: &[config::ProcessorConfig],
    event_id: i64,
    processor_filter: Option<&str>,
) -> Result<()> {
    let event = db::events::get_event_by_id(pool, event_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("event {} not found", event_id))?;
    let email = if let Some(email_id) = event.email_id {
        db::emails::get_email_by_id(pool, email_id).await?
    } else {
        None
    };
    let registry = processor::registry::ProcessorRegistry::from_config(configs);
    let processors = if let Some(name) = processor_filter {
        vec![registry.require_processor_for_event(name, &event)?]
    } else {
        let eligible = registry.processors_for_event(&event);
        if eligible.is_empty() {
            bail!(
                "no eligible processors for event type '{}' from account '{}' mailbox '{}'",
                event.event_type,
                event.account_id,
                event.mailbox_name
            );
        }
        eligible
    };

    for proc in processors {
        let proc_name = proc.name().to_string();
        info!(processor = proc_name, event_id, "replaying processor");
        let job_id = match db::jobs::get_job_by_event_and_processor(pool, event_id, &proc_name).await? {
            Some(existing) => existing.id,
            None => match db::jobs::create_job(pool, event_id, &proc_name).await? {
                Some(id) => id,
                None => db::jobs::get_job_by_event_and_processor(pool, event_id, &proc_name).await?
                    .ok_or_else(|| anyhow::anyhow!("job for event {} / processor '{}' vanished after create_job returned None", event_id, proc_name))?.id,
            },
        };
        let timeout_secs = configs
            .iter()
            .find(|c| c.name == proc_name)
            .map(|c| c.timeout_secs)
            .unwrap_or(30);
        let timeout = std::time::Duration::from_secs(timeout_secs);
        match db::jobs::reset_and_claim_job_for_replay(pool, job_id).await {
            Ok(true) => {}
            Ok(false) => {
                warn!(
                    job_id,
                    processor = proc_name,
                    "job is already in progress; skipping replay"
                );
                continue;
            }
            Err(e) => {
                error!(job_id, error = %e, "failed to reset and claim replay job");
                continue;
            }
        }
        match tokio::time::timeout(timeout, proc.process(&event, email.as_ref())).await {
            Ok(Ok(output)) => {
                let status = if output.success {
                    "completed"
                } else {
                    "failed"
                };
                let serialized = serde_json::to_value(&output).ok();
                let message = output.message.as_deref();
                if let Err(e) = db::jobs::update_job_status(
                    pool,
                    job_id,
                    status,
                    message,
                    None,
                    serialized.as_ref(),
                    db::jobs::AttemptsUpdate::None,
                )
                .await
                {
                    error!(job_id, error = %e, "failed to persist replay output");
                }
            }
            Ok(Err(e)) => {
                let _ = db::jobs::update_job_status(
                    pool,
                    job_id,
                    "failed",
                    Some(&e.to_string()),
                    None,
                    None,
                    db::jobs::AttemptsUpdate::None,
                )
                .await;
            }
            Err(_) => {
                let _ = db::jobs::update_job_status(
                    pool,
                    job_id,
                    "failed",
                    Some("timed out"),
                    None,
                    None,
                    db::jobs::AttemptsUpdate::None,
                )
                .await;
            }
        }
    }
    info!("replay complete");
    Ok(())
}

fn dry_run_output_details(
    output: &processor::ProcessorOutput,
) -> (&Option<String>, &Option<serde_json::Value>) {
    (&output.message, &output.metadata)
}

/// Execute a processor dry-run after named eligibility validation.
async fn dry_run_event(
    pool: &sqlx::PgPool,
    configs: &[config::ProcessorConfig],
    event_id: i64,
    processor_name: &str,
) -> Result<()> {
    let event = db::events::get_event_by_id(pool, event_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("event {} not found", event_id))?;
    let email = if let Some(email_id) = event.email_id {
        db::emails::get_email_by_id(pool, email_id).await?
    } else {
        None
    };
    let registry = processor::registry::ProcessorRegistry::from_config(configs);
    let proc = registry.require_processor_for_event(processor_name, &event)?;
    let timeout_secs = configs
        .iter()
        .find(|c| c.name == processor_name)
        .map(|c| c.timeout_secs)
        .unwrap_or(30);
    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        proc.process(&event, email.as_ref()),
    )
    .await
    {
        Ok(Ok(output)) => {
            let (message, metadata) = dry_run_output_details(&output);
            if output.success {
                info!(
                    processor = processor_name,
                    message = ?message,
                    metadata = ?metadata,
                    "dry-run processor completed"
                );
            } else {
                warn!(
                    processor = processor_name,
                    message = ?message,
                    metadata = ?metadata,
                    "dry-run processor reported failure"
                );
            }
        }
        Ok(Err(e)) => {
            error!(processor = processor_name, error = %e, "dry-run processor error");
        }
        Err(_) => {
            error!(processor = processor_name, "dry-run processor timed out");
        }
    }
    Ok(())
}

/// Replay command: re-run processors for a specific event.
async fn cmd_replay(
    config: config::Config,
    event_id: i64,
    processor_filter: Option<String>,
) -> Result<()> {
    info!(event_id, "replaying event");
    let pool = db::connect(&config.database).await?;
    db::run_migrations(&pool).await?;
    let result = replay_event(
        &pool,
        &config.processors,
        event_id,
        processor_filter.as_deref(),
    )
    .await;
    pool.close().await;
    result
}

/// Dry-run command wrapper owns pool setup and shutdown.
async fn cmd_dry_run(config: config::Config, event_id: i64, processor_name: String) -> Result<()> {
    info!(event_id, processor = processor_name, "dry-run starting");
    let pool = db::connect(&config.database).await?;
    db::run_migrations(&pool).await?;
    let result = dry_run_event(&pool, &config.processors, event_id, &processor_name).await;
    pool.close().await;
    result
}

#[cfg(test)]
mod command_eligibility_tests {
    use super::*;

    #[test]
    fn dry_run_retains_structured_result_details_for_reporting() {
        let output = processor::ProcessorOutput {
            success: false,
            message: Some("diagnostic details".into()),
            metadata: Some(serde_json::json!({"request_id": "abc-123"})),
            metrics: vec![],
        };
        let (message, metadata) = dry_run_output_details(&output);
        assert_eq!(message.as_deref(), Some("diagnostic details"));
        assert_eq!(
            metadata.as_ref(),
            Some(&serde_json::json!({"request_id": "abc-123"}))
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn unnamed_replay_runs_only_eligible_command_processors(
        pool: sqlx::PgPool,
    ) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let counter = dir.path().join("invocations");
        let command_config = |name: &str, account: &str| {
            let mut config = crate::config::ProcessorConfig {
                name: name.into(),
                enabled: true,
                events: vec!["email_arrived".into()],
                sources: Some(vec![crate::config::ProcessorSource {
                    account: account.into(),
                    mailboxes: None,
                }]),
                max_retries: 0,
                retry_backoff_secs: vec![],
                timeout_secs: 10,
                concurrency: 1,
                config: Default::default(),
            };
            config
                .config
                .insert("command".into(), toml::Value::String("sh".into()));
            config.config.insert(
                "args".into(),
                toml::Value::Array(vec![
                    toml::Value::String("-c".into()),
                    toml::Value::String(format!(
                        "echo {} >> '{}' ; echo '{{\\\"success\\\":true}}'",
                        name,
                        counter.display()
                    )),
                ]),
            );
            config
        };
        let event_id: i64 = sqlx::query_scalar("INSERT INTO events (event_type, account_id, mailbox_name, payload) VALUES ('email_arrived','work','Archive','{}') RETURNING id")
            .fetch_one(&pool).await?;
        let ineligible = command_config("excluded", "personal");
        assert!(
            replay_event(&pool, &[ineligible], event_id, Some("excluded"))
                .await
                .is_err()
        );
        assert!(
            !counter.exists(),
            "rejected replay must not invoke the command"
        );

        let eligible = command_config("eligible", "work");
        replay_event(
            &pool,
            &[eligible, command_config("excluded", "personal")],
            event_id,
            None,
        )
        .await?;
        assert_eq!(
            std::fs::read_to_string(&counter)?
                .lines()
                .collect::<Vec<_>>(),
            vec!["eligible"]
        );
        let excluded_jobs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM processor_jobs WHERE event_id=$1 AND processor_name='excluded'",
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await?;
        assert_eq!(excluded_jobs, 0);
        let eligible_job =
            crate::db::jobs::get_job_by_event_and_processor(&pool, event_id, "eligible")
                .await?
                .unwrap();
        assert_eq!(eligible_job.attempts, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn replay_contested_with_sweep_invokes_command_once(pool: sqlx::PgPool) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let counter = dir.path().join("invocations");
        let started = dir.path().join("started");
        let release = dir.path().join("release");
        let mut config = crate::config::ProcessorConfig {
            name: "probe".into(),
            enabled: true,
            events: vec!["email_arrived".into()],
            sources: None,
            max_retries: 0,
            retry_backoff_secs: vec![],
            timeout_secs: 10,
            concurrency: 1,
            config: Default::default(),
        };
        config
            .config
            .insert("command".into(), toml::Value::String("sh".into()));
        config.config.insert("args".into(), toml::Value::Array(vec![
            toml::Value::String("-c".into()),
            toml::Value::String(format!("echo invoked >> '{}'; touch '{}'; while [ ! -f '{}' ]; do sleep 0.01; done; printf '{{\\\"success\\\":true}}'", counter.display(), started.display(), release.display())),
        ]));
        let configs = vec![config.clone()];
        let event_id: i64 = sqlx::query_scalar("INSERT INTO events (event_type, account_id, mailbox_name, payload) VALUES ('email_arrived','work','INBOX','{}') RETURNING id")
            .fetch_one(&pool).await?;
        crate::db::jobs::create_job(&pool, event_id, "probe").await?;
        let scheduler = crate::processor::scheduler::JobScheduler::new(
            pool.clone(),
            Arc::new(crate::processor::registry::ProcessorRegistry::from_config(
                &configs,
            )),
            tokio::sync::mpsc::channel(1).1,
            CancellationToken::new(),
            configs.clone(),
        );
        let replay_pool = pool.clone();
        let replay_configs = configs.clone();
        let replay = tokio::spawn(async move {
            replay_event(&replay_pool, &replay_configs, event_id, Some("probe")).await
        });
        let sweep = tokio::spawn(async move { scheduler.retry_sweep().await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !started.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        std::fs::write(release, "go")?;
        replay.await??;
        sweep.await?;
        assert_eq!(std::fs::read_to_string(counter)?.lines().count(), 1);
        let job = crate::db::jobs::get_job_by_event_and_processor(&pool, event_id, "probe")
            .await?
            .unwrap();
        assert_eq!(job.status, "completed");
        assert_eq!(job.attempts, 1);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "requires DATABASE_URL and PostgreSQL"]
    async fn rejected_replay_and_dry_run_have_no_side_effects(pool: sqlx::PgPool) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let counter = dir.path().join("rejected-invocations");
        let command_config = |enabled: bool, events: Vec<String>, account: &str| {
            let mut config = crate::config::ProcessorConfig {
                name: "probe".into(),
                enabled,
                events,
                sources: Some(vec![crate::config::ProcessorSource {
                    account: account.into(),
                    mailboxes: None,
                }]),
                max_retries: 0,
                retry_backoff_secs: vec![],
                timeout_secs: 10,
                concurrency: 1,
                config: Default::default(),
            };
            config
                .config
                .insert("command".into(), toml::Value::String("sh".into()));
            config.config.insert(
                "args".into(),
                toml::Value::Array(vec![
                    toml::Value::String("-c".into()),
                    toml::Value::String(format!(
                        "echo invoked >> '{}' ; echo '{{\\\"success\\\":true}}'",
                        counter.display()
                    )),
                ]),
            );
            config
        };
        let eligible = command_config(true, vec!["email_arrived".into()], "work");
        let rejected = vec![
            (
                "source",
                vec![command_config(true, vec!["email_arrived".into()], "other")],
                "email_arrived",
            ),
            (
                "subscription",
                vec![command_config(true, vec!["email_removed".into()], "work")],
                "email_arrived",
            ),
            (
                "disabled",
                vec![command_config(false, vec!["email_arrived".into()], "work")],
                "email_arrived",
            ),
            ("missing", vec![], "email_arrived"),
            ("event type", vec![eligible.clone()], "email_removed"),
        ];

        for (label, configs, event_type) in rejected {
            for existing_job in [false, true] {
                let event_id: i64 = sqlx::query_scalar(
                    "INSERT INTO events (event_type, account_id, mailbox_name, payload, dispatched_at) VALUES ($1,'work','Archive','{}',now()) RETURNING id",
                )
                .bind(event_type)
                .fetch_one(&pool)
                .await?;
                let job_id: Option<i64> = if existing_job {
                    Some(sqlx::query_scalar("INSERT INTO processor_jobs (event_id, processor_name, status, attempts, last_error, next_retry_at, output) VALUES ($1,'probe','failed',4,'preserve',now()+interval '1 hour','{\"saved\":true}') RETURNING id")
                        .bind(event_id).fetch_one(&pool).await?)
                } else {
                    None
                };
                let before_job = if let Some(id) = job_id {
                    Some(serde_json::to_value(
                        crate::db::jobs::get_job_by_id(&pool, id).await?.unwrap(),
                    )?)
                } else {
                    None
                };
                let before_marker: Option<chrono::DateTime<chrono::Utc>> =
                    sqlx::query_scalar("SELECT dispatched_at FROM events WHERE id=$1")
                        .bind(event_id)
                        .fetch_one(&pool)
                        .await?;

                assert!(
                    replay_event(&pool, &configs, event_id, Some("probe"))
                        .await
                        .is_err(),
                    "{label} replay should reject"
                );
                assert!(
                    dry_run_event(&pool, &configs, event_id, "probe")
                        .await
                        .is_err(),
                    "{label} dry-run should reject"
                );
                assert!(!counter.exists(), "{label} rejection invoked command");

                let after_jobs: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM processor_jobs WHERE event_id=$1")
                        .bind(event_id)
                        .fetch_one(&pool)
                        .await?;
                assert_eq!(
                    after_jobs,
                    i64::from(existing_job),
                    "{label} changed job existence"
                );
                if let (Some(id), Some(before)) = (job_id, before_job) {
                    let after = serde_json::to_value(
                        crate::db::jobs::get_job_by_id(&pool, id).await?.unwrap(),
                    )?;
                    assert_eq!(before, after, "{label} mutated existing job");
                }
                let after_marker: Option<chrono::DateTime<chrono::Utc>> =
                    sqlx::query_scalar("SELECT dispatched_at FROM events WHERE id=$1")
                        .bind(event_id)
                        .fetch_one(&pool)
                        .await?;
                assert_eq!(
                    before_marker, after_marker,
                    "{label} changed dispatch marker"
                );
            }
        }
        Ok(())
    }
}
