use anyhow::{Context, Result};

pub struct Config {
    /// Lowercase email addresses (or substrings) that are accepted as bank senders.
    pub allowed_senders: Vec<String>,
    /// Model name passed to genai, e.g. "claude-haiku-4-5-20251001" or "gpt-4o-mini".
    /// genai infers the provider from the model name and reads the corresponding
    /// API key from the environment automatically (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.).
    pub llm_model: String,
    pub firefly: FireflyConfig,
}

pub struct FireflyConfig {
    /// Firefly API base URL, usually "https://<host>/api".
    pub base_url: String,
    /// Personal access token.
    pub access_token: String,
    /// The asset account ID in Firefly where transactions are booked.
    pub asset_account_id: String,
    /// Optional transaction currency code (e.g. "USD", "EUR").
    pub currency_code: Option<String>,
    /// Whether Firefly should apply rules for the new transaction.
    pub apply_rules: bool,
    /// Whether Firefly should fire webhooks for the new transaction.
    pub fire_webhooks: bool,
    /// Whether Firefly should reject duplicate transaction hashes.
    pub error_if_duplicate_hash: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let allowed_senders = std::env::var("ALLOWED_SENDERS")
            .context("ALLOWED_SENDERS env var required (comma-separated email addresses)")?
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let llm_model = std::env::var("LLM_MODEL").context("LLM_MODEL env var required")?;

        let base_url =
            std::env::var("FIREFLY_BASE_URL").context("FIREFLY_BASE_URL env var required")?;
        let access_token = std::env::var("FIREFLY_ACCESS_TOKEN")
            .context("FIREFLY_ACCESS_TOKEN env var required")?;
        let asset_account_id = std::env::var("FIREFLY_ASSET_ACCOUNT_ID")
            .context("FIREFLY_ASSET_ACCOUNT_ID env var required")?;
        let currency_code = std::env::var("FIREFLY_CURRENCY_CODE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let apply_rules = env_bool("FIREFLY_APPLY_RULES", false)?;
        let fire_webhooks = env_bool("FIREFLY_FIRE_WEBHOOKS", true)?;
        let error_if_duplicate_hash = env_bool("FIREFLY_ERROR_IF_DUPLICATE_HASH", false)?;

        Ok(Self {
            allowed_senders,
            llm_model,
            firefly: FireflyConfig {
                base_url,
                access_token,
                asset_account_id,
                currency_code,
                apply_rules,
                fire_webhooks,
                error_if_duplicate_hash,
            },
        })
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

fn env_bool(name: &str, default: bool) -> Result<bool> {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => {
                anyhow::bail!("{name} must be a boolean (one of: true/false, 1/0, yes/no, on/off)")
            }
        },
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(anyhow::anyhow!("failed to read {name}: {e}")),
    }
}
