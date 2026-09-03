use std::path::PathBuf;

use anyhow::{Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
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

    /// Run one configured processor against durable historical emails
    Backfill(BackfillArgs),
}

/// Arguments for the `mailmux backfill` command.
///
/// Invokes exactly one configured processor against historical `emails` rows
/// using transient in-memory events. No events or processor jobs are
/// persisted.
#[derive(Debug, Clone, clap::Args)]
pub struct BackfillArgs {
    /// Configured processor to invoke
    #[arg(long)]
    pub processor: String,

    /// Include emails whose message date is >= this value
    /// (YYYY-MM-DD or RFC 3339; date-only means midnight UTC)
    #[arg(long, value_parser = parse_backfill_datetime)]
    pub after: Option<DateTime<Utc>>,

    /// Include emails whose message date is < this value
    /// (YYYY-MM-DD or RFC 3339; date-only means midnight UTC)
    #[arg(long, value_parser = parse_backfill_datetime)]
    pub before: Option<DateTime<Utc>>,

    /// Include an account; repeatable (values are ORed)
    #[arg(long = "account")]
    pub accounts: Vec<String>,

    /// Include a mailbox; repeatable (values are ORed)
    #[arg(long = "mailbox")]
    pub mailboxes: Vec<String>,

    /// Include a specific emails.id; repeatable (values are ORed)
    #[arg(long = "email-id")]
    pub email_ids: Vec<i64>,

    /// Explicitly allow selecting all stored emails
    #[arg(long)]
    pub all: bool,

    /// Stop after N selected emails
    #[arg(long)]
    pub limit: Option<u64>,

    /// Override the processor's concurrency for this run
    #[arg(long)]
    pub concurrency: Option<usize>,

    /// Stop scheduling new work after the first permanently failed email
    #[arg(long)]
    pub fail_fast: bool,

    /// Count selected emails without invoking the processor
    #[arg(long)]
    pub dry_run: bool,
}

impl BackfillArgs {
    /// Validate safety rules that depend only on the CLI arguments:
    /// at least one selection filter unless `--all` is present, and a
    /// strictly increasing `--after` / `--before` range.
    pub fn validate(&self) -> Result<()> {
        let has_filter = self.after.is_some()
            || self.before.is_some()
            || !self.accounts.is_empty()
            || !self.mailboxes.is_empty()
            || !self.email_ids.is_empty();

        if !has_filter && !self.all {
            bail!(
                "backfill requires at least one selection filter \
                 (--after, --before, --account, --mailbox, --email-id) \
                 or an explicit --all"
            );
        }

        if let (Some(after), Some(before)) = (self.after, self.before)
            && after >= before
        {
            bail!(
                "invalid date range: --after ({after}) must be strictly \
                 earlier than --before ({before})"
            );
        }

        Ok(())
    }
}

/// Parse a backfill date filter. Accepts `YYYY-MM-DD` (interpreted as
/// midnight UTC) or an RFC 3339 timestamp (normalized to UTC).
pub fn parse_backfill_datetime(value: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        // Midnight is always a valid time for any valid date.
        return Ok(date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is valid")
            .and_utc());
    }

    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("invalid date or RFC 3339 timestamp '{value}': {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    fn utc_dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_time(NaiveTime::from_hms_opt(h, mi, s).unwrap())
            .and_utc()
    }

    fn backfill(args: &[&str]) -> Result<BackfillArgs, clap::Error> {
        let cli = Cli::try_parse_from(args)?;
        let Some(Command::Backfill(args)) = cli.command else {
            return Err(clap::Error::new(
                clap::error::ErrorKind::MissingRequiredArgument,
            ));
        };
        Ok(args)
    }

    #[test]
    fn test_backfill_parses_all_options() {
        let args = backfill(&[
            "mailmux",
            "backfill",
            "--processor",
            "mail-indexer",
            "--after",
            "2021-01-01",
            "--before",
            "2022-01-01",
            "--account",
            "personal",
            "--account",
            "work",
            "--mailbox",
            "INBOX",
            "--mailbox",
            "Sent",
            "--email-id",
            "1",
            "--email-id",
            "2",
            "--all",
            "--limit",
            "10",
            "--concurrency",
            "4",
            "--fail-fast",
            "--dry-run",
        ])
        .unwrap();

        assert_eq!(args.processor, "mail-indexer");
        assert_eq!(args.after, Some(utc_dt(2021, 1, 1, 0, 0, 0)));
        assert_eq!(args.before, Some(utc_dt(2022, 1, 1, 0, 0, 0)));
        assert_eq!(
            args.accounts,
            vec!["personal".to_string(), "work".to_string()]
        );
        assert_eq!(
            args.mailboxes,
            vec!["INBOX".to_string(), "Sent".to_string()]
        );
        assert_eq!(args.email_ids, vec![1, 2]);
        assert!(args.all);
        assert_eq!(args.limit, Some(10));
        assert_eq!(args.concurrency, Some(4));
        assert!(args.fail_fast);
        assert!(args.dry_run);

        args.validate().unwrap();
    }

    #[test]
    fn test_backfill_requires_filter_or_all() {
        // Parsing succeeds; the safety rule is enforced by validate().
        let args = backfill(&["mailmux", "backfill", "--processor", "p"]).unwrap();
        assert!(matches!(
            args.validate(),
            Err(e) if e.to_string().contains("selection filter")
        ));

        let args = backfill(&["mailmux", "backfill", "--processor", "p", "--all"]).unwrap();
        args.validate().unwrap();
    }

    #[test]
    fn test_backfill_datetime_date_only_is_midnight_utc() {
        assert_eq!(
            parse_backfill_datetime("2021-06-15").unwrap(),
            utc_dt(2021, 6, 15, 0, 0, 0)
        );
    }

    #[test]
    fn test_backfill_datetime_rfc3339_offset_normalized_to_utc() {
        assert_eq!(
            parse_backfill_datetime("2021-06-15T10:30:00+02:00").unwrap(),
            utc_dt(2021, 6, 15, 8, 30, 0)
        );
        assert_eq!(
            parse_backfill_datetime("2021-06-15T10:30:00Z").unwrap(),
            utc_dt(2021, 6, 15, 10, 30, 0)
        );
    }

    #[test]
    fn test_backfill_datetime_rejects_malformed_values() {
        assert!(parse_backfill_datetime("not-a-date").is_err());
        assert!(parse_backfill_datetime("2021-13-45").is_err());
        assert!(parse_backfill_datetime("2021-06-15T25:00:00Z").is_err());
    }

    #[test]
    fn test_backfill_rejects_invalid_date_range() {
        let args = backfill(&[
            "mailmux",
            "backfill",
            "--processor",
            "p",
            "--after",
            "2022-01-01",
            "--before",
            "2021-01-01",
        ])
        .unwrap();
        assert!(matches!(
            args.validate(),
            Err(e) if e.to_string().contains("invalid date range")
        ));

        // Equal bounds are also rejected (empty range).
        let args = backfill(&[
            "mailmux",
            "backfill",
            "--processor",
            "p",
            "--after",
            "2021-01-01",
            "--before",
            "2021-01-01",
        ])
        .unwrap();
        assert!(args.validate().is_err());
    }
}
