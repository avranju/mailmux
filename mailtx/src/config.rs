use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Lowercase email addresses that are accepted as bank senders.
    pub allowed_senders: Vec<String>,
    /// Model name passed to genai, e.g. "claude-haiku-4-5-20251001" or "gpt-4o-mini".
    /// genai infers the provider from the model name and reads the corresponding
    /// API key from the environment automatically (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.).
    #[serde(default = "default_llm_model")]
    pub llm_model: String,
    /// Tag applied to every transaction posted to Firefly. Defaults to "mailmux-mailtx".
    #[serde(default = "default_tag")]
    pub tag: String,
    pub firefly: FireflyConfig,

    /// Path to the SQLite database used to hold pending transfer legs.
    /// Required when any transfer_rules are defined.
    pub state_db: Option<String>,
    /// How long to wait (in hours) for the counterpart leg before expiring.
    #[serde(default = "default_transfer_match_window_hours")]
    pub transfer_match_window_hours: u64,
    /// Directional transfer rules used to coalesce two-leg bank transfers into
    /// a single Firefly III "transfer" transaction.
    #[serde(default)]
    pub transfer_rules: Vec<TransferRule>,
}

/// A directional rule describing one transfer route between two asset accounts.
#[derive(Debug, Clone, Deserialize)]
pub struct TransferRule {
    /// Local `id` of the asset account money leaves from.
    pub source_account: String,
    /// Local `id` of the asset account money arrives in.
    pub destination_account: String,
    /// All of these substrings must appear (case-insensitive) in the LLM-extracted
    /// description of the withdrawal email for this rule to match.
    #[serde(default)]
    pub withdrawal_keywords: Vec<String>,
    /// All of these substrings must appear (case-insensitive) in the LLM-extracted
    /// description of the deposit email for this rule to match.
    #[serde(default)]
    pub deposit_keywords: Vec<String>,
}

fn default_llm_model() -> String {
    "claude-haiku-4-5-20251001".to_string()
}

fn default_tag() -> String {
    "mailmux-mailtx".to_string()
}

fn default_transfer_match_window_hours() -> u64 {
    48
}

#[derive(Debug, Deserialize)]
pub struct FireflyConfig {
    /// Firefly API base URL, usually "https://<host>/api".
    pub base_url: String,
    /// Personal access token.
    pub access_token: String,

    /// Candidate asset accounts used by the matcher to resolve which account to book.
    #[serde(default)]
    pub asset_accounts: Vec<FireflyAssetAccountConfig>,
    /// Optional fallback asset account ID used when matcher cannot resolve an account.
    pub default_asset_account_id: Option<String>,
    /// Optional transaction currency code (e.g. "USD", "EUR").
    pub currency_code: Option<String>,
    /// Whether Firefly should apply rules for the new transaction.
    #[serde(default)]
    pub apply_rules: bool,
    /// Whether Firefly should fire webhooks for the new transaction.
    #[serde(default = "default_fire_webhooks")]
    pub fire_webhooks: bool,
    /// Whether Firefly should reject duplicate transaction hashes.
    #[serde(default)]
    pub error_if_duplicate_hash: bool,
}

fn default_fire_webhooks() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct FireflyAssetAccountConfig {
    /// Stable local identifier (for logs/debugging).
    pub id: String,
    /// Firefly asset account ID.
    pub firefly_account_id: String,
    /// Optional account suffix hints (e.g. ["9772", "9558"]).
    #[serde(default)]
    pub account_suffixes: Vec<String>,
    /// Optional debit-card last4 hints mapped to this asset account.
    #[serde(default)]
    pub debit_card_last4: Vec<String>,
    /// Optional free-text aliases for fuzzy-ish deterministic name matching.
    #[serde(default)]
    pub aliases: Vec<String>,
}

impl Config {
    /// Load configuration from the TOML file pointed to by the `MAILTX_CONFIG` env var.
    pub fn load() -> Result<Self> {
        let path = std::env::var("MAILTX_CONFIG")
            .context("MAILTX_CONFIG env var required (path to TOML config file)")?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config file: {path}"))?;
        let mut config: Self =
            toml::from_str(&content).with_context(|| format!("parsing config file: {path}"))?;

        // Normalise allowed_senders to lowercase mailbox addresses and drop invalid entries.
        config.allowed_senders = config
            .allowed_senders
            .into_iter()
            .filter_map(|s| normalize_sender_address(&s))
            .collect();

        if config.firefly.asset_accounts.is_empty() {
            anyhow::bail!("firefly.asset_accounts must contain at least one entry");
        }

        if !config.transfer_rules.is_empty() && config.state_db.is_none() {
            anyhow::bail!(
                "transfer_rules are configured but state_db is not set; \
                 set state_db to a writable file path for the pending transfer store"
            );
        }

        Ok(config)
    }

    /// Returns true if the sender's parsed mailbox address exactly matches an
    /// entry in the allow-list. Display names and malformed sender strings are
    /// never used for matching.
    pub fn sender_allowed(&self, sender: &str) -> bool {
        let Some(sender_address) = normalize_sender_address(sender) else {
            return false;
        };

        self.allowed_senders
            .iter()
            .any(|allowed| sender_address == allowed.as_str())
    }
}

fn normalize_sender_address(sender: &str) -> Option<String> {
    let sender = sender.trim();
    if sender.is_empty() {
        return None;
    }

    let address = if let Some(start) = sender.find('<') {
        let end = sender[start + 1..].find('>')? + start + 1;
        // Reject malformed strings rather than falling back to display-name text.
        if sender[end + 1..].trim().is_empty() && sender[start + 1..end].find('<').is_none() {
            &sender[start + 1..end]
        } else {
            return None;
        }
    } else {
        sender
    }
    .trim()
    .to_lowercase();

    if is_valid_mailbox_address(&address) {
        Some(address)
    } else {
        None
    }
}

fn is_valid_mailbox_address(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };

    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !address
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '"'))
}

#[cfg(test)]
mod tests {
    use super::{Config, FireflyConfig, normalize_sender_address};

    #[test]
    fn normalizes_exact_mailbox_addresses() {
        assert_eq!(
            normalize_sender_address("Alerts <ALERTS@bank.example>"),
            Some("alerts@bank.example".to_string())
        );
        assert_eq!(
            normalize_sender_address(" alerts@bank.example "),
            Some("alerts@bank.example".to_string())
        );
    }

    #[test]
    fn ignores_display_name_when_extracting_sender() {
        assert_eq!(
            normalize_sender_address("\"alerts@bank.example support\" <evil@attacker.example>"),
            Some("evil@attacker.example".to_string())
        );
    }

    #[test]
    fn rejects_malformed_sender_strings() {
        assert_eq!(
            normalize_sender_address("alerts@bank.example support"),
            None
        );
        assert_eq!(normalize_sender_address("Alerts Only"), None);
        assert_eq!(normalize_sender_address("Alerts <not-an-address>"), None);
        assert_eq!(
            normalize_sender_address("Alerts <a@b.example> trailing"),
            None
        );
    }

    #[test]
    fn sender_allowed_requires_exact_mailbox_match() {
        let config = test_config(vec!["alerts@bank.example".to_string()]);

        assert!(config.sender_allowed("Alerts <alerts@bank.example>"));
        assert!(!config.sender_allowed("\"alerts@bank.example support\" <evil@attacker.example>"));
        assert!(!config.sender_allowed("fraud-alerts@bank.example"));
        assert!(!config.sender_allowed("alerts@bank.example.evil.example"));
    }

    fn test_config(allowed_senders: Vec<String>) -> Config {
        Config {
            allowed_senders,
            llm_model: "test-model".to_string(),
            tag: "test-tag".to_string(),
            firefly: FireflyConfig {
                base_url: "https://firefly.example/api".to_string(),
                access_token: "token".to_string(),
                asset_accounts: vec![],
                default_asset_account_id: None,
                currency_code: None,
                apply_rules: false,
                fire_webhooks: true,
                error_if_duplicate_hash: false,
            },
            state_db: None,
            transfer_match_window_hours: 48,
            transfer_rules: vec![],
        }
    }
}
