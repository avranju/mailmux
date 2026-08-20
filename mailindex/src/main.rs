use anyhow::{Context, Result};
use clap::Parser;
use mailindex::{
    cli::{Cli, Command},
    config::Config,
    storage::Repository,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(cli.log_level.clone().unwrap_or_else(|| "info".into()))
        .init();
    let config = Config::load(&cli.config)?;
    let repo = Repository::open(&config.storage.database_path).await?;
    repo.migrate().await?;
    match cli.command {
        Some(Command::IndexStatus) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&repo.status_counts().await?)?
            );
        }
        Some(Command::RebuildIndex) => {
            mailindex::index::rebuild::rebuild_index(&config, std::sync::Arc::new(repo))
                .await
                .context("rebuild index")?;
        }
        None => {
            mailindex::http::serve(config, repo).await?;
        }
    }
    Ok(())
}
