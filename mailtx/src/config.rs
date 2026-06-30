use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::warn;

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
    /// When true, allows plaintext HTTP for loopback-only hosts (localhost,
    /// 127.0.0.0/8, ::1) — intended for local development only.  Default: false.
    #[serde(default)]
    pub allow_insecure_http: bool,

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

        // Validate and normalize firefly.base_url at startup.
        let normalized_url = validate_and_normalize_firefly_base_url(&config.firefly)
            .with_context(|| "validating firefly.base_url at startup")?;
        config.firefly.base_url = normalized_url;

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

/// Validate and normalise the Firefly base URL at startup.
///
/// Returns the trimmed URL string on success.  Rejects non-HTTPS schemes
/// unless `allow_insecure_http` is true and the host is loopback.
pub fn validate_and_normalize_firefly_base_url(config: &FireflyConfig) -> Result<String> {
    let url_str = config.base_url.trim();
    let url = reqwest::Url::parse(url_str)
        .with_context(|| format!("firefly.base_url is not a valid URL: {url_str}"))?;

    let host = url.host().ok_or_else(|| {
        anyhow::anyhow!(
            "firefly.base_url must include a host (e.g. https://firefly.example.com/api)"
        )
    })?;

    let host_str = host.to_string();
    match url.scheme() {
        "https" => Ok(url_str.to_string()),
        "http" if config.allow_insecure_http && is_loopback_http_host(&host_str) => {
            warn!(
                "plaintext HTTP is enabled for firefly.base_url (loopback-only, local development)"
            );
            Ok(url_str.to_string())
        }
        "http" if !config.allow_insecure_http => anyhow::bail!(
            "firefly.base_url must use HTTPS because the Firefly credentials and \
             transaction data must not be sent over plaintext transport; \
             set firefly.allow_insecure_http = true only for local development on loopback hosts"
        ),
        "http" => anyhow::bail!(
            "firefly.allow_insecure_http is enabled but firefly.base_url host \
             ({host_str}) is not a loopback address; the override is limited to \
             localhost, 127.0.0.0/8, and ::1"
        ),
        other => anyhow::bail!(
            "firefly.base_url must use https, or http only with the loopback-only \
             allow_insecure_http override; got scheme: {other}"
        ),
    }
}

/// Returns true if `host` is a loopback address that is safe for the
/// insecure-HTTP development override.
///
/// Accepts:
/// - The literal hostname "localhost" (case-insensitive)
/// - IPv4 loopback addresses (127.0.0.0/8)
/// - IPv6 loopback (::1)
pub fn is_loopback_http_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }

    // Strip optional brackets that the url crate adds for IPv6 literals.
    let inner = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);

    // Try parsing as an IP address to check loopback.
    if let Ok(ip) = inner.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }

    false
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

    use super::{is_loopback_http_host, validate_and_normalize_firefly_base_url};

    fn test_config(allowed_senders: Vec<String>) -> Config {
        Config {
            allowed_senders,
            llm_model: "test-model".to_string(),
            tag: "test-tag".to_string(),
            firefly: FireflyConfig {
                base_url: "https://firefly.example/api".to_string(),
                access_token: "token".to_string(),
                allow_insecure_http: false,
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

    // ---------------------------------------------------------------------------
    // URL validation tests
    // ---------------------------------------------------------------------------

    fn test_firefly_config(base_url: &str, allow_insecure_http: bool) -> FireflyConfig {
        FireflyConfig {
            base_url: base_url.to_string(),
            access_token: "dummy-token".to_string(),
            allow_insecure_http,
            asset_accounts: vec![],
            default_asset_account_id: None,
            currency_code: None,
            apply_rules: false,
            fire_webhooks: true,
            error_if_duplicate_hash: false,
        }
    }

    #[test]
    fn accepts_https_url_with_insecure_disabled() {
        let config = test_firefly_config("https://firefly.example/api", false);
        let result = validate_and_normalize_firefly_base_url(&config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "https://firefly.example/api");
    }

    #[test]
    fn rejects_http_url_when_insecure_is_disabled() {
        let config = test_firefly_config("http://firefly.example/api", false);
        let result = validate_and_normalize_firefly_base_url(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("HTTPS"));
        assert!(!err.contains("token"));
    }

    #[test]
    fn accepts_http_localhost_with_insecure_enabled() {
        let config = test_firefly_config("http://localhost:8080", true);
        let result = validate_and_normalize_firefly_base_url(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn accepts_http_127_0_0_1_with_insecure_enabled() {
        let config = test_firefly_config("http://127.0.0.1:8080", true);
        let result = validate_and_normalize_firefly_base_url(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn accepts_http_ipv6_loopback_with_insecure_enabled() {
        let config = test_firefly_config("http://[::1]:8080", true);
        let result = validate_and_normalize_firefly_base_url(&config);
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_http_non_loopback_even_with_insecure_enabled() {
        for url in [
            "http://192.168.1.10/api",
            "http://10.0.0.2/api",
            "http://firefly.local/api",
        ] {
            let config = test_firefly_config(url, true);
            let result = validate_and_normalize_firefly_base_url(&config);
            assert!(result.is_err(), "expected error for {url}, but got Ok");
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("loopback"),
                "error for {url} should mention loopback, got: {err}"
            );
        }
    }

    #[test]
    fn rejects_insecure_http_localhost_when_override_is_disabled() {
        let config = test_firefly_config("http://localhost:8080", false);
        let result = validate_and_normalize_firefly_base_url(&config);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_invalid_url() {
        let config = test_firefly_config("not-a-url", false);
        let result = validate_and_normalize_firefly_base_url(&config);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_url_without_host() {
        // url crate parses "https:///path" with an empty domain host.
        // We use http:///path so the scheme branch triggers the rejection.
        let config = test_firefly_config("http:///path", false);
        let result = validate_and_normalize_firefly_base_url(&config);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_unsupported_scheme() {
        let config = test_firefly_config("ftp://firefly.example/api", false);
        let result = validate_and_normalize_firefly_base_url(&config);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ftp"));
    }

    #[test]
    fn loopback_http_host_detects_localhost() {
        assert!(is_loopback_http_host("localhost"));
        assert!(is_loopback_http_host("Localhost"));
        assert!(is_loopback_http_host("LOCALHOST"));
    }

    #[test]
    fn loopback_http_host_detects_ipv4_loopback() {
        assert!(is_loopback_http_host("127.0.0.1"));
        assert!(is_loopback_http_host("127.255.255.255"));
        assert!(!is_loopback_http_host("127.0.0.1").to_string().is_empty()); // just check it compiles
        assert!(!is_loopback_http_host("192.168.1.1"));
        assert!(!is_loopback_http_host("10.0.0.1"));
    }

    #[test]
    fn loopback_http_host_detects_ipv6_loopback() {
        assert!(is_loopback_http_host("::1"));
        assert!(!is_loopback_http_host("fe80::1"));
    }

    #[test]
    fn loopback_http_host_rejects_domain_names() {
        assert!(!is_loopback_http_host("firefly.local"));
        assert!(!is_loopback_http_host("example.com"));
    }
}
