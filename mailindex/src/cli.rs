use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "mailindex", version)]
pub struct Cli {
    #[arg(short = 'c', long, default_value = "config.toml")]
    pub config: PathBuf,
    #[arg(long)]
    pub log_level: Option<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    RebuildIndex,
    IndexStatus,
}
