use anyhow::{Context, Result};

pub struct Config {
    /// Lowercase email addresses (or substrings) that are accepted as bank senders.
    pub allowed_senders: Vec<String>,
    pub anthropic_api_key: String,
    pub anthropic_model: String,
    pub endpoint_url: String,
    /// Full value for the Authorization header, e.g. "Bearer <token>".
    pub endpoint_auth: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let allowed_senders = std::env::var("ALLOWED_SENDERS")
            .context("ALLOWED_SENDERS env var required (comma-separated email addresses)")?
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let anthropic_api_key =
            std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY env var required")?;

        let anthropic_model = std::env::var("ANTHROPIC_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());

        let endpoint_url =
            std::env::var("ENDPOINT_URL").context("ENDPOINT_URL env var required")?;

        let endpoint_auth =
            std::env::var("ENDPOINT_AUTH").context("ENDPOINT_AUTH env var required")?;

        Ok(Self {
            allowed_senders,
            anthropic_api_key,
            anthropic_model,
            endpoint_url,
            endpoint_auth,
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
