use std::path::PathBuf;

use clap::Parser;

/// Mailmux — event-driven IMAP email processing daemon
#[derive(Debug, Parser)]
#[command(name = "mailmux", version, about)]
pub struct Cli {
    /// Path to the configuration file
    #[arg(short, long, default_value = "config.toml")]
    pub config: PathBuf,

    /// Override log level (trace, debug, info, warn, error)
    #[arg(long)]
    pub log_level: Option<String>,
}
