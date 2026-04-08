use std::sync::Arc;

use anyhow::{Result, bail};
use clap::Parser;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

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

    match cli.command {
        Some(cli::Command::Replay {
            event_id,
            processor: processor_filter,
        }) => cmd_replay(config, event_id, processor_filter).await,
        Some(cli::Command::DryRun {
            event_id,
            processor: processor_name,
        }) => cmd_dry_run(config, event_id, processor_name).await,
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

/// Replay command: re-run processors for a specific event.
async fn cmd_replay(
    config: config::Config,
    event_id: i64,
    processor_filter: Option<String>,
) -> Result<()> {
    info!(event_id, "replaying event");

    let pool = db::connect(&config.database).await?;
    db::run_migrations(&pool).await?;

    let event = db::events::get_event_by_id(&pool, event_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("event {} not found", event_id))?;

    let email = if let Some(email_id) = event.email_id {
        db::emails::get_email_by_id(&pool, email_id).await?
    } else {
        None
    };

    let registry = processor::registry::ProcessorRegistry::from_config(&config.processors);
    let processors = registry.processors_for_event(&event.event_type);

    if processors.is_empty() {
        bail!(
            "no processors configured for event type '{}'",
            event.event_type
        );
    }

    for proc in processors {
        if let Some(ref filter) = processor_filter
            && proc.name() != filter
        {
            continue;
        }

        let proc_name = proc.name().to_string();
        info!(processor = proc_name, event_id, "running processor");

        // Create a new job for the replay
        let job_id = db::jobs::create_job(&pool, event_id, &proc_name)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "a processor job already exists for event {} / processor '{}'",
                    event_id,
                    proc_name
                )
            })?;

        let timeout_secs = config
            .processors
            .iter()
            .find(|c| c.name == proc_name)
            .map(|c| c.timeout_secs)
            .unwrap_or(30);

        let timeout = std::time::Duration::from_secs(timeout_secs);

        if let Err(e) =
            db::jobs::update_job_status(&pool, job_id, "in_progress", None, None, true).await
        {
            error!(job_id, error = %e, "failed to update job status");
            continue;
        }

        match tokio::time::timeout(timeout, proc.process(&event, email.as_ref())).await {
            Ok(Ok(output)) if output.success => {
                info!(processor = proc_name, "replay completed successfully");
                let msg = output.message.as_deref();
                let _ =
                    db::jobs::update_job_status(&pool, job_id, "completed", msg, None, false).await;
            }
            Ok(Ok(output)) => {
                let msg = output.message.unwrap_or_default();
                warn!(
                    processor = proc_name,
                    message = msg,
                    "replay completed with failure"
                );
                let _ =
                    db::jobs::update_job_status(&pool, job_id, "failed", Some(&msg), None, false)
                        .await;
            }
            Ok(Err(e)) => {
                error!(processor = proc_name, error = %e, "replay failed with error");
                let _ = db::jobs::update_job_status(
                    &pool,
                    job_id,
                    "failed",
                    Some(&e.to_string()),
                    None,
                    false,
                )
                .await;
            }
            Err(_) => {
                error!(processor = proc_name, "replay timed out");
                let _ = db::jobs::update_job_status(
                    &pool,
                    job_id,
                    "failed",
                    Some("timed out"),
                    None,
                    false,
                )
                .await;
            }
        }
    }

    pool.close().await;
    info!("replay complete");
    Ok(())
}

/// Dry-run command: run a processor without persisting results.
async fn cmd_dry_run(config: config::Config, event_id: i64, processor_name: String) -> Result<()> {
    info!(event_id, processor = processor_name, "dry-run starting");

    let pool = db::connect(&config.database).await?;
    db::run_migrations(&pool).await?;

    let event = db::events::get_event_by_id(&pool, event_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("event {} not found", event_id))?;

    let email = if let Some(email_id) = event.email_id {
        db::emails::get_email_by_id(&pool, email_id).await?
    } else {
        None
    };

    let registry = processor::registry::ProcessorRegistry::from_config(&config.processors);
    let processor = registry
        .processors_for_event(&event.event_type)
        .into_iter()
        .find(|p| p.name() == processor_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "processor '{}' not found or not subscribed to event type '{}'",
                processor_name,
                event.event_type
            )
        })?;

    let timeout_secs = config
        .processors
        .iter()
        .find(|c| c.name == processor_name)
        .map(|c| c.timeout_secs)
        .unwrap_or(30);

    let timeout = std::time::Duration::from_secs(timeout_secs);

    info!("executing processor (results will NOT be persisted)");
    match tokio::time::timeout(timeout, processor.process(&event, email.as_ref())).await {
        Ok(Ok(output)) => {
            if output.success {
                info!(
                    processor = processor_name,
                    message = output.message.as_deref().unwrap_or("(none)"),
                    "dry-run: processor succeeded"
                );
            } else {
                warn!(
                    processor = processor_name,
                    message = output.message.as_deref().unwrap_or("(none)"),
                    "dry-run: processor reported failure"
                );
            }
            if let Some(metadata) = output.metadata {
                info!(metadata = %metadata, "dry-run: processor metadata");
            }
        }
        Ok(Err(e)) => {
            error!(processor = processor_name, error = %e, "dry-run: processor error");
        }
        Err(_) => {
            error!(processor = processor_name, "dry-run: processor timed out");
        }
    }

    pool.close().await;
    info!("dry-run complete");
    Ok(())
}
