use std::path::PathBuf;

use clap::{Parser, Subcommand};

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

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Re-run processors for a specific event
    Replay {
        /// The event ID to replay
        #[arg(long)]
        event_id: i64,

        /// Only run a specific processor (by name)
        #[arg(long)]
        processor: Option<String>,
    },

    /// Run a processor against an event without persisting results
    DryRun {
        /// The event ID to process
        #[arg(long)]
        event_id: i64,

        /// The processor name to run
        #[arg(long)]
        processor: String,
    },
}
