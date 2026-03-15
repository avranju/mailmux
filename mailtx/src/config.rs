use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Lowercase email addresses (or substrings) that are accepted as bank senders.
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
        let mut config: Self = toml::from_str(&content)
            .with_context(|| format!("parsing config file: {path}"))?;

        // Normalise allowed_senders to lowercase and drop empty entries.
        for s in &mut config.allowed_senders {
            *s = s.trim().to_lowercase();
        }
        config.allowed_senders.retain(|s| !s.is_empty());

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

    /// Returns true if the sender (which may be in "Display Name <email>" format)
    /// matches any entry in the allow-list.
    pub fn sender_allowed(&self, sender: &str) -> bool {
        let sender_lower = sender.to_lowercase();
        self.allowed_senders
            .iter()
            .any(|allowed| sender_lower.contains(allowed.as_str()))
    }
}
