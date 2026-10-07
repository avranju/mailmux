use std::collections::HashMap;
use std::path::Path;

use crate::db::events::Event;
use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use regex::Regex;
use serde::Deserialize;
use tracing::warn;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

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
    #[serde(default = "default_enabled")]
    pub enabled: bool,
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
    pub initial_sync_start_date: Option<NaiveDate>,
    #[serde(default = "default_imap_command_timeout")]
    pub imap_command_timeout_secs: u64,
    pub tls_ca_file: Option<String>,
    #[serde(default)]
    pub tls_accept_invalid_certs: bool,
    /// If set, a sync is forced after this many seconds even when IDLE is
    /// active and no server notification has arrived.  Useful as a safety
    /// net to catch messages that IDLE silently missed.  Should be larger
    /// than `poll_interval_secs`.  Omit or set to null to disable.
    pub idle_heartbeat_interval_secs: Option<u64>,
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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessorSource {
    pub account: String,
    #[serde(default)]
    pub mailboxes: Option<Vec<String>>,
}

impl ProcessorSource {
    pub fn matches(&self, account_id: &str, mailbox_name: &str) -> bool {
        self.account == account_id
            && self
                .mailboxes
                .as_ref()
                .is_none_or(|names| names.iter().any(|name| name == mailbox_name))
    }
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
    pub sources: Option<Vec<ProcessorSource>>,
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

impl ProcessorConfig {
    pub fn matches_source(&self, account_id: &str, mailbox_name: &str) -> bool {
        self.sources
            .as_ref()
            .is_none_or(|sources| sources.iter().any(|s| s.matches(account_id, mailbox_name)))
    }

    pub fn matches_event(&self, event: &Event) -> bool {
        self.enabled
            && self.events.iter().any(|e| e == &event.event_type)
            && self.matches_source(&event.account_id, &event.mailbox_name)
    }
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
        Self::check_file_permissions(path);

        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file: {}", path.display()))?;

        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("parsing config file: {}", path.display()))?;

        config.validate_password_references()?;
        config.resolve_env_vars();
        config.validate()?;

        Ok(config)
    }

    /// Warn if the config file is world-readable, since it may reference
    /// secrets via environment variable names.
    fn check_file_permissions(path: &Path) {
        #[cfg(unix)]
        {
            if let Ok(metadata) = std::fs::metadata(path) {
                let mode = metadata.permissions().mode();
                if mode & 0o004 != 0 {
                    warn!(
                        path = %path.display(),
                        mode = format!("{mode:04o}"),
                        "config file is world-readable — consider restricting \
                         permissions to 0600 or 0640"
                    );
                }
            }
        }
    }

    /// Substitute `${VAR}` environment variable references in all string fields.
    fn resolve_env_vars(&mut self) {
        // General
        substitute_env_vars(&mut self.general.data_dir);
        substitute_env_vars(&mut self.general.log_level);
        substitute_env_vars(&mut self.general.log_format);

        // Database
        substitute_env_vars(&mut self.database.url);

        // Accounts
        for account in &mut self.accounts {
            substitute_env_vars(&mut account.id);
            substitute_env_vars(&mut account.imap_host);
            substitute_env_vars(&mut account.username);
            substitute_env_vars(&mut account.password);
            for mailbox in &mut account.mailboxes {
                substitute_env_vars(mailbox);
            }

            if let Some(ca_file) = &mut account.tls_ca_file {
                substitute_env_vars(ca_file);
            }
        }

        // Processors
        for processor in &mut self.processors {
            substitute_env_vars(&mut processor.name);
            if let Some(sources) = &mut processor.sources {
                for source in sources {
                    substitute_env_vars(&mut source.account);
                    if let Some(mailboxes) = &mut source.mailboxes {
                        for mailbox in mailboxes {
                            substitute_env_vars(mailbox);
                        }
                    }
                }
            }
            substitute_toml_value_env_vars(&mut processor.config);
        }
    }

    fn validate_password_references(&self) -> Result<()> {
        for account in &self.accounts {
            if !is_env_var_reference(&account.password) {
                bail!(
                    "account '{}': password must be an environment variable reference (e.g. password = \"${{MY_PASSWORD}}\"). Literal passwords in config files are not allowed.",
                    account.id
                );
            }
        }
        Ok(())
    }

    pub fn warn_unmonitored_processor_sources(&self) {
        for processor in &self.processors {
            for source in processor.sources.iter().flatten() {
                if self
                    .accounts
                    .iter()
                    .find(|a| a.id == source.account)
                    .is_some_and(|a| !a.enabled)
                {
                    warn!(processor = %processor.name, account = %source.account, "processor source account is disabled; historical emails remain selectable");
                }
                if let Some(mailboxes) = &source.mailboxes {
                    for mailbox in mailboxes {
                        if self
                            .accounts
                            .iter()
                            .find(|a| a.id == source.account)
                            .is_none_or(|a| !a.mailboxes.contains(mailbox))
                        {
                            warn!(processor = %processor.name, account = %source.account, mailbox = %mailbox, "processor source mailbox is not currently monitored");
                        }
                    }
                }
            }
        }
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
                bail!(
                    "account '{}': at least one mailbox must be configured",
                    account.id
                );
            }
            for mailbox in &account.mailboxes {
                if mailbox.is_empty() {
                    bail!("account '{}': mailbox name must not be empty", account.id);
                }
            }
            if account.tls_ca_file.is_some() && !account.tls {
                bail!("account '{}': tls_ca_file requires tls = true", account.id);
            }
            if account.tls_accept_invalid_certs && !account.tls {
                bail!(
                    "account '{}': tls_accept_invalid_certs requires tls = true",
                    account.id
                );
            }
            if account.tls_accept_invalid_certs && !is_loopback_host(&account.imap_host) {
                bail!(
                    "account '{}': tls_accept_invalid_certs is only allowed for loopback addresses \
                     (localhost, 127.0.0.1, [::1]). Disabling certificate verification for \
                     remote hosts exposes the connection to man-in-the-middle attacks.",
                    account.id
                );
            }
            if let Some(heartbeat) = account.idle_heartbeat_interval_secs
                && heartbeat <= account.poll_interval_secs
            {
                warn!(
                    account = account.id,
                    heartbeat_secs = heartbeat,
                    poll_interval_secs = account.poll_interval_secs,
                    "idle_heartbeat_interval_secs is <= poll_interval_secs; \
                     the heartbeat will fire more often than the polling fallback, \
                     which is probably not what you want"
                );
            }
        }

        // Validate processors
        const KNOWN_EVENT_TYPES: &[&str] = &["email_arrived"];
        let mut seen_names = std::collections::HashSet::new();
        for processor in &self.processors {
            if processor.name.is_empty() {
                bail!("processor name must not be empty");
            }
            if !seen_names.insert(&processor.name) {
                bail!("duplicate processor name: {}", processor.name);
            }
            if let Some(sources) = &processor.sources {
                if sources.is_empty() {
                    bail!("processor '{}': sources must not be empty", processor.name);
                }
                for source in sources {
                    if source.account.is_empty() {
                        bail!(
                            "processor '{}': source account must not be empty",
                            processor.name
                        );
                    }
                    if !self.accounts.iter().any(|a| a.id == source.account) {
                        bail!(
                            "processor '{}': unknown source account '{}'",
                            processor.name,
                            source.account
                        );
                    }
                    if let Some(mailboxes) = &source.mailboxes {
                        if mailboxes.is_empty() {
                            bail!(
                                "processor '{}': source for account '{}' has an empty mailboxes list",
                                processor.name,
                                source.account
                            );
                        }
                        if mailboxes.iter().any(String::is_empty) {
                            bail!(
                                "processor '{}': source for account '{}' has an empty mailbox name",
                                processor.name,
                                source.account
                            );
                        }
                    }
                }
            }
            for event in &processor.events {
                if !KNOWN_EVENT_TYPES.contains(&event.as_str()) {
                    bail!(
                        "processor '{}': unknown event type '{}' (supported: {})",
                        processor.name,
                        event,
                        KNOWN_EVENT_TYPES.join(", ")
                    );
                }
            }
        }

        // Validate general
        if self.general.data_dir.is_empty() {
            bail!("general.data_dir must not be empty");
        }

        Ok(())
    }
}

/// Returns `true` if the value is a `${VAR}` environment variable reference.
fn is_env_var_reference(value: &str) -> bool {
    let re = Regex::new(r"^\$\{[^}]+\}$").expect("valid regex");
    re.is_match(value)
}

/// Returns `true` if the given host is a loopback address (localhost, 127.0.0.1, or [::1]).
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Substitute `${VAR}` patterns in a string with environment variable values.
/// If the variable is not set, the pattern is left as-is.
fn substitute_env_vars(value: &mut String) {
    let re = Regex::new(r"\$\{([^}]+)\}").expect("valid regex");
    let replaced = re
        .replace_all(value, |caps: &regex::Captures| {
            let var_name = &caps[1];
            std::env::var(var_name).unwrap_or_else(|_| caps[0].to_string())
        })
        .into_owned();
    *value = replaced;
}

/// Recursively substitute `${VAR}` patterns in all string values within a
/// TOML value map (used for processor config).
fn substitute_toml_value_env_vars(map: &mut HashMap<String, toml::Value>) {
    for value in map.values_mut() {
        substitute_toml_value(value);
    }
}

fn substitute_toml_value(value: &mut toml::Value) {
    match value {
        toml::Value::String(s) => substitute_env_vars(s),
        toml::Value::Array(arr) => {
            for item in arr {
                substitute_toml_value(item);
            }
        }
        toml::Value::Table(table) => {
            for (_, v) in table.iter_mut() {
                substitute_toml_value(v);
            }
        }
        _ => {}
    }
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
enabled = true
imap_host = "imap.example.com"
imap_port = 993
tls = true
username = "user@example.com"
password = "${TEST_SECRET}"
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
    fn source_matching_is_exact_and_paired() {
        let source = ProcessorSource {
            account: "personal".into(),
            mailboxes: Some(vec!["INBOX".into(), "Archive".into()]),
        };
        assert!(source.matches("personal", "INBOX"));
        assert!(source.matches("personal", "Archive"));
        assert!(!source.matches("work", "INBOX"));
        assert!(!source.matches("personal", "inbox"));
    }

    #[test]
    fn explicit_empty_sources_are_rejected() {
        let toml = sample_toml().replace(
            "events = [\"email_arrived\"]",
            "events = [\"email_arrived\"]\nsources = []",
        );
        let f = write_temp_config(&toml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("sources must not be empty"));
    }

    #[test]
    fn malformed_sources_are_rejected_even_for_disabled_processors() {
        for (selector, expected) in [
            (r#"{ account = "missing" }"#, "unknown source account"),
            (r#"{ account = "" }"#, "source account must not be empty"),
            (
                r#"{ account = "test", mailboxes = [] }"#,
                "empty mailboxes list",
            ),
            (
                r#"{ account = "test", mailboxes = [""] }"#,
                "empty mailbox name",
            ),
            (r#"{ account = "test", extra = true }"#, "unknown field"),
        ] {
            let toml = sample_toml().replace(
                "enabled = true\nevents = [\"email_arrived\"]",
                &format!("enabled = false\nevents = [\"email_arrived\"]\nsources = [{selector}]"),
            );
            let file = write_temp_config(&toml);
            let error = Config::load(file.path()).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "{selector}: {error:#}"
            );
        }
    }

    #[test]
    fn selector_account_reference_and_optional_mailboxes_validate() {
        let toml = sample_toml().replace(
            "events = [\"email_arrived\"]",
            "events = [\"email_arrived\"]\nsources = [{ account = \"test\" }]",
        );
        let file = write_temp_config(&toml);
        let config = Config::load(file.path()).unwrap();
        assert!(config.processors[0].matches_source("test", "Archive"));
        assert!(!config.processors[0].matches_source("Test", "Archive"));
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
        assert!(config.accounts[0].enabled);
        assert_eq!(config.processors.len(), 1);
        assert_eq!(config.processors[0].name, "logger");
    }

    #[test]
    fn test_account_enabled_defaults_true() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
imap_host = "imap.example.com"
username = "a"
password = "${TEST_PASS}"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        let config = Config::load(f.path()).unwrap();
        assert!(config.accounts[0].enabled);
    }

    #[test]
    fn test_account_enabled_can_be_false() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
enabled = false
imap_host = "imap.example.com"
username = "a"
password = "${TEST_PASS}"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        let config = Config::load(f.path()).unwrap();
        assert!(!config.accounts[0].enabled);
    }

    #[test]
    fn test_env_var_substitution() {
        temp_env::with_var("TEST_MAILMUX_PASS", Some("my_secret"), || {
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
        });
    }

    #[test]
    fn env_substitutions_apply_to_accounts_and_source_selectors_before_validation() {
        temp_env::with_vars(
            [
                ("MM_ACCOUNT", Some("historical")),
                ("MM_SOURCE", Some("historical")),
                ("MM_BOX", Some("Archive")),
                ("MM_SELECTOR_BOX", Some("Archive")),
            ],
            || {
                let toml = sample_toml()
                    .replace("id = \"test\"", "id = \"${MM_ACCOUNT}\"")
                    .replace("mailboxes = [\"INBOX\"]", "mailboxes = [\"${MM_BOX}\"]")
                    .replace("events = [\"email_arrived\"]", "events = [\"email_arrived\"]\nsources = [{ account = \"${MM_SOURCE}\", mailboxes = [\"${MM_SELECTOR_BOX}\"] }]");
                let file = write_temp_config(&toml);
                let config = Config::load(file.path()).unwrap();
                assert_eq!(config.accounts[0].id, "historical");
                assert_eq!(config.accounts[0].mailboxes, ["Archive"]);
                assert!(config.processors[0].matches_source("historical", "Archive"));
            },
        );
    }

    #[test]
    fn resolved_empty_and_duplicate_account_ids_are_rejected() {
        temp_env::with_var("MM_EMPTY", Some(""), || {
            let toml = sample_toml().replace("id = \"test\"", "id = \"${MM_EMPTY}\"");
            let file = write_temp_config(&toml);
            assert!(
                Config::load(file.path())
                    .unwrap_err()
                    .to_string()
                    .contains("account id must not be empty")
            );
        });
        temp_env::with_var("MM_DUP", Some("same"), || {
            let toml = sample_toml()
                .replace("id = \"test\"", "id = \"${MM_DUP}\"")
                .replace("[[processors]]", "[[accounts]]\nid = \"same\"\nimap_host = \"imap.example.com\"\nusername = \"other\"\npassword = \"${TEST_SECRET}\"\nmailboxes = [\"INBOX\"]\n\n[[processors]]");
            let file = write_temp_config(&toml);
            assert!(
                Config::load(file.path())
                    .unwrap_err()
                    .to_string()
                    .contains("duplicate account id")
            );
        });
    }

    #[test]
    fn missing_selector_account_is_rejected_and_disabled_historical_account_is_valid() {
        let missing = sample_toml().replace(
            "events = [\"email_arrived\"]",
            "events = [\"email_arrived\"]\nsources = [{ mailboxes = [\"INBOX\"] }]",
        );
        let file = write_temp_config(&missing);
        assert!(
            format!("{:#}", Config::load(file.path()).unwrap_err())
                .contains("missing field `account`")
        );

        let historical = sample_toml()
            .replace("id = \"test\"", "id = \"old\"")
            .replace("enabled = true", "enabled = false")
            .replace("events = [\"email_arrived\"]", "events = [\"email_arrived\"]\nsources = [{ account = \"old\", mailboxes = [\"FormerBox\"] }]");
        let file = write_temp_config(&historical);
        let config = Config::load(file.path()).unwrap();
        assert!(config.processors[0].matches_source("old", "FormerBox"));
        config.warn_unmonitored_processor_sources();
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
password = "${TEST_PASS}"
mailboxes = ["INBOX"]

[[accounts]]
id = "dup"
imap_host = "imap.example.com"
username = "c"
password = "${TEST_PASS}"
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
password = "${TEST_PASS}"
mailboxes = []
"#;
        let f = write_temp_config(toml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("at least one mailbox"));
    }

    #[test]
    fn test_unknown_event_type() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
imap_host = "imap.example.com"
username = "a"
password = "${TEST_PASS}"
mailboxes = ["INBOX"]

[[processors]]
name = "notify"
events = ["email_deleted"]
"#;
        let f = write_temp_config(toml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(err.to_string().contains("unknown event type"));
        assert!(err.to_string().contains("email_deleted"));
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

    #[test]
    fn test_tls_accept_invalid_certs_rejected_for_remote_host() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
imap_host = "imap.example.com"
tls = true
tls_accept_invalid_certs = true
username = "a"
password = "${TEST_PASS}"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("only allowed for loopback addresses"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_tls_accept_invalid_certs_allowed_for_localhost() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
imap_host = "localhost"
tls = true
tls_accept_invalid_certs = true
username = "a"
password = "${TEST_PASS}"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        let config = Config::load(f.path()).unwrap();
        assert!(config.accounts[0].tls_accept_invalid_certs);
    }

    #[test]
    fn test_tls_accept_invalid_certs_allowed_for_127_0_0_1() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
imap_host = "127.0.0.1"
tls = true
tls_accept_invalid_certs = true
username = "a"
password = "${TEST_PASS}"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        let config = Config::load(f.path()).unwrap();
        assert!(config.accounts[0].tls_accept_invalid_certs);
    }

    #[test]
    fn test_literal_password_rejected() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
imap_host = "imap.example.com"
username = "a"
password = "plaintext_secret"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        let err = Config::load(f.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("must be an environment variable reference"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_env_var_password_accepted() {
        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
imap_host = "imap.example.com"
username = "a"
password = "${SOME_PASSWORD}"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.accounts[0].id, "test");
    }

    #[cfg(unix)]
    #[test]
    fn test_world_readable_config_warns() {
        use std::os::unix::fs::PermissionsExt;

        let toml = r#"
[general]
data_dir = "/tmp/mailmux"

[database]
url = "postgres://localhost/mailmux"

[[accounts]]
id = "test"
imap_host = "imap.example.com"
username = "a"
password = "${TEST_PASS}"
mailboxes = ["INBOX"]
"#;
        let f = write_temp_config(toml);
        // Make the file world-readable
        std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(0o644)).unwrap();

        // Should succeed but emit a warning (we just verify it doesn't error)
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.accounts[0].id, "test");
    }
}
