use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub general: GeneralConfig,
    pub database: DatabaseConfig,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub processors: Vec<ProcessorConfig>,
}

#[derive(Debug, Deserialize)]
pub struct GeneralConfig {
    pub data_dir: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_log_format")]
    pub log_format: String,
    #[serde(default = "default_shutdown_grace_period")]
    pub shutdown_grace_period_secs: u64,
    #[serde(default)]
    pub health_port: Option<u16>,
    #[serde(default = "default_event_retention_days")]
    pub event_retention_days: u64,
}

fn default_event_retention_days() -> u64 {
    30
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_format() -> String {
    "pretty".to_string()
}

fn default_shutdown_grace_period() -> u64 {
    10
}

#[derive(Debug, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    10
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccountConfig {
    pub id: String,
    pub imap_host: String,
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    #[serde(default = "default_tls")]
    pub tls: bool,
    pub username: String,
    pub password: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_second: u32,
    #[serde(default = "default_account_max_connections")]
    pub max_connections: u32,
    pub mailboxes: Vec<String>,
    pub initial_sync_max_messages: Option<u64>,
    pub initial_sync_max_age_days: Option<u64>,
    #[serde(default = "default_imap_command_timeout")]
    pub imap_command_timeout_secs: u64,
}

fn default_imap_port() -> u16 {
    993
}

fn default_tls() -> bool {
    true
}

fn default_poll_interval() -> u64 {
    60
}

fn default_rate_limit() -> u32 {
    5
}

fn default_account_max_connections() -> u32 {
    2
}

fn default_imap_command_timeout() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ProcessorConfig {
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default)]
    pub retry_backoff_secs: Vec<u64>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
    #[serde(default)]
    pub config: HashMap<String, toml::Value>,
}

fn default_enabled() -> bool {
    true
}

fn default_timeout() -> u64 {
    30
}

fn default_concurrency() -> u32 {
    1
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading config file: {}", path.display()))?;

        let content = substitute_env_vars(&content);

        let config: Config =
            toml::from_str(&content).with_context(|| format!("parsing config file: {}", path.display()))?;

        config.validate()?;

        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        // Validate accounts
        if self.accounts.is_empty() {
            bail!("at least one account must be configured");
        }

        let mut seen_ids = std::collections::HashSet::new();
        for account in &self.accounts {
            if account.id.is_empty() {
                bail!("account id must not be empty");
            }
            if !seen_ids.insert(&account.id) {
                bail!("duplicate account id: {}", account.id);
            }
            if account.imap_host.is_empty() {
                bail!("account '{}': imap_host must not be empty", account.id);
            }
            if account.username.is_empty() {
                bail!("account '{}': username must not be empty", account.id);
            }
            if account.mailboxes.is_empty() {
                bail!("account '{}': at least one mailbox must be configured", account.id);
            }
            for mailbox in &account.mailboxes {
                if mailbox.is_empty() {
                    bail!("account '{}': mailbox name must not be empty", account.id);
                }
            }
        }

        // Validate processors
        let mut seen_names = std::collections::HashSet::new();
        for processor in &self.processors {
            if processor.name.is_empty() {
                bail!("processor name must not be empty");
            }
            if !seen_names.insert(&processor.name) {
                bail!("duplicate processor name: {}", processor.name);
            }
        }

        // Validate general
        if self.general.data_dir.is_empty() {
            bail!("general.data_dir must not be empty");
        }

        Ok(())
    }
}

/// Substitute `${VAR}` patterns with environment variable values.
/// If the variable is not set, leave the pattern as-is.
fn substitute_env_vars(input: &str) -> String {
    let re = Regex::new(r"\$\{([^}]+)\}").expect("valid regex");
    re.replace_all(input, |caps: &regex::Captures| {
        let var_name = &caps[1];
        std::env::var(var_name).unwrap_or_else(|_| caps[0].to_string())
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn sample_toml() -> &'static str {
        r#"
[general]
data_dir = "/tmp/mailmux"
log_level = "debug"
log_format = "json"
shutdown_grace_period_secs = 5

[database]
url = "postgres://user:pass@localhost:5432/mailmux"
max_connections = 5

[[accounts]]
id = "test"
imap_host = "imap.example.com"
imap_port = 993
tls = true
username = "user@example.com"
password = "secret"
poll_interval_secs = 60
mailboxes = ["INBOX"]

[[processors]]
name = "logger"
enabled = true
events = ["email_arrived"]
timeout_secs = 5
concurrency = 1
"#
    }

    fn write_temp_config(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_load_valid_config() {
        let f = write_temp_config(sample_toml());
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.general.data_dir, "/tmp/mailmux");
        assert_eq!(config.general.log_level, "debug");
        assert_eq!(config.database.max_connections, 5);
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].id, "test");
        assert_eq!(config.processors.len(), 1);
        assert_eq!(config.processors[0].name, "logger");
    }

    #[test]
    fn test_env_var_substitution() {
        // SAFETY: This test runs serially and no other thread reads this env var.
        unsafe { std::env::set_var("TEST_MAILMUX_PASS", "my_secret") };
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://user:pass@localhost:5432/mailmux"

[[accounts]]
id = "test"
imap_host = "imap.example.com"
username = "user@example.com"
password = "${TEST_MAILMUX_PASS}"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.accounts[0].password, "my_secret");
        unsafe { std::env::remove_var("TEST_MAILMUX_PASS") };
    }

    #[test]
    fn test_duplicate_account_ids() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "dup"
imap_host = "imap.example.com"
username = "a"
password = "b"
mailboxes = ["INBOX"]

[[accounts]]
id = "dup"
imap_host = "imap.example.com"
username = "c"
password = "d"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("duplicate account id"));
    }

    #[test]
    fn test_empty_mailboxes() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
imap_host = "imap.example.com"
username = "a"
password = "b"
mailboxes = []
"#;
        let f = write_temp_config(toml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("at least one mailbox"));
    }

    #[test]
    fn test_no_accounts() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"
"#;
        let f = write_temp_config(toml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("at least one account"));
    }
}
