use std::sync::Arc;

use anyhow::Result;
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
mod processor;
mod shutdown;
mod store;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    // Load configuration
    let config = config::Config::load(&cli.config)?;

    // Initialize logging (CLI override takes precedence)
    let log_level = cli.log_level.as_deref().unwrap_or(&config.general.log_level);
    logging::init(log_level, &config.general.log_format)?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config_path = %cli.config.display(),
        accounts = config.accounts.len(),
        processors = config.processors.len(),
        "mailmux starting"
    );

    // Connect to database
    let pool = db::connect(&config.database).await?;
    info!("connected to database");

    // Run migrations
    db::run_migrations(&pool).await?;
    info!("database migrations complete");

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
    let health_state = health::HealthState::new(pool.clone());
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
    info!("mailmux is running, press Ctrl+C to stop");

    // Wait for shutdown signal
    token.cancelled().await;
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
