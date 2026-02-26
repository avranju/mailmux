use anyhow::{Context, Result};

pub struct Config {
    /// Lowercase email addresses (or substrings) that are accepted as bank senders.
    pub allowed_senders: Vec<String>,
    /// Model name passed to genai, e.g. "claude-haiku-4-5-20251001" or "gpt-4o-mini".
    /// genai infers the provider from the model name and reads the corresponding
    /// API key from the environment automatically (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.).
    pub llm_model: String,
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

        let llm_model = std::env::var("LLM_MODEL")
            .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());

        let endpoint_url =
            std::env::var("ENDPOINT_URL").context("ENDPOINT_URL env var required")?;

        let endpoint_auth =
            std::env::var("ENDPOINT_AUTH").context("ENDPOINT_AUTH env var required")?;

        Ok(Self {
            allowed_senders,
            llm_model,
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
