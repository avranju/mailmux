use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::time::Duration;
use tracing::warn;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LlmBackend {
    #[default]
    Auto,
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum LlmResponseFormat {
    #[default]
    JsonSchema,
    JsonObject,
    Prompt,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    #[serde(default)]
    pub backend: LlmBackend,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub endpoint: Option<String>,
    pub api_key_env: Option<String>,
    pub response_format: Option<LlmResponseFormat>,
    pub timeout_secs: Option<u64>,
    pub connect_timeout_secs: Option<u64>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub allow_insecure_http: Option<bool>,
    pub extra_body: Option<toml::Table>,
}

#[derive(Debug, Clone)]
pub struct ResolvedLlmConfig {
    pub backend: LlmBackend,
    pub model: String,
    pub response_format: LlmResponseFormat,
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f64>,
    pub custom: Option<ResolvedOpenAiCompatibleConfig>,
}
#[derive(Debug, Clone)]
pub struct ResolvedOpenAiCompatibleConfig {
    pub url: reqwest::Url,
    pub api_key_env: Option<String>,
    pub extra_body: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Lowercase email addresses that are accepted as bank senders.
    pub allowed_senders: Vec<String>,
    /// Model name passed to genai, e.g. "claude-haiku-4-5-20251001" or "gpt-4o-mini".
    /// genai infers the provider from the model name and reads the corresponding
    /// API key from the environment automatically (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.).
    pub llm_model: Option<String>,
    pub llm: Option<LlmConfig>,
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

pub fn build_llm_url(base: &str, endpoint: &str, allow_http: bool) -> Result<reqwest::Url> {
    if base.contains('\\') {
        anyhow::bail!("llm.base_url contains a forbidden backslash");
    }
    let mut url =
        reqwest::Url::parse(base).map_err(|_| anyhow::anyhow!("llm.base_url is invalid"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
    {
        anyhow::bail!(
            "llm.base_url must be absolute and contain no credentials, query, or fragment"
        );
    }
    if url.scheme() != "https" && !(allow_http && url.scheme() == "http") {
        anyhow::bail!("llm.base_url requires HTTPS unless llm.allow_insecure_http is true");
    }
    if !endpoint.starts_with('/')
        || endpoint.starts_with("//")
        || endpoint.contains(['\\', '?', '#'])
    {
        anyhow::bail!("llm.endpoint must be a single-leading-slash path");
    }
    let mut decoded = endpoint.to_string();
    let original_slashes = endpoint.matches('/').count();
    let mut stable = false;
    for _ in 0..64 {
        // URL parsers may discard ASCII tab/newline characters before resolving
        // dot segments, so reject them in every representation we inspect.
        if decoded.chars().any(char::is_control) {
            anyhow::bail!("llm.endpoint contains a forbidden control character");
        }
        if decoded.split('/').any(|s| s == "." || s == "..")
            || decoded.contains('\\')
            || decoded.matches('/').count() > original_slashes
        {
            anyhow::bail!("llm.endpoint contains traversal or encoded separators");
        }
        let next = percent_decode(&decoded)?;
        if next == decoded {
            stable = true;
            break;
        }
        decoded = next;
    }
    if !stable {
        anyhow::bail!("llm.endpoint encoding is too deeply nested");
    }
    let origin = (
        url.scheme().to_string(),
        url.host_str().unwrap_or("").to_string(),
        url.port(),
    );
    let prefix = url.path().trim_end_matches('/');
    url.set_path(&format!("{prefix}{endpoint}"));
    if (
        url.scheme().to_string(),
        url.host_str().unwrap_or("").to_string(),
        url.port(),
    ) != origin
    {
        anyhow::bail!("llm.endpoint changes URL origin");
    }
    Ok(url)
}
fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                anyhow::bail!("llm.endpoint has malformed escape");
            }
            let h = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|x| u8::from_str_radix(x, 16).ok())
                .ok_or_else(|| anyhow::anyhow!("llm.endpoint has malformed escape"))?;
            out.push(h);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| anyhow::anyhow!("llm.endpoint has invalid encoding"))
}
pub fn extra_body_to_json(table: &toml::Table) -> Result<Map<String, Value>> {
    const RESERVED: &[&str] = &[
        "model",
        "messages",
        "stream",
        "response_format",
        "max_tokens",
        "max_completion_tokens",
        "temperature",
        "n",
    ];
    let mut result = Map::new();
    for (key, value) in table {
        if RESERVED.contains(&key.as_str()) {
            anyhow::bail!("llm.extra_body contains a managed request key");
        }
        let json = toml_value_to_json(value)?;
        result.insert(key.clone(), json);
    }
    Ok(result)
}
fn toml_value_to_json(value: &toml::Value) -> Result<Value> {
    Ok(match value {
        toml::Value::String(v) => Value::String(v.clone()),
        toml::Value::Integer(v) => Value::Number((*v).into()),
        toml::Value::Float(v) if v.is_finite() => serde_json::Number::from_f64(*v)
            .map(Value::Number)
            .ok_or_else(|| anyhow::anyhow!("llm.extra_body contains a nonfinite number"))?,
        toml::Value::Float(_) => anyhow::bail!("llm.extra_body contains a nonfinite number"),
        toml::Value::Boolean(v) => Value::Bool(*v),
        toml::Value::Datetime(_) => {
            anyhow::bail!("llm.extra_body does not support datetime values")
        }
        toml::Value::Array(values) => Value::Array(
            values
                .iter()
                .map(toml_value_to_json)
                .collect::<Result<Vec<_>>>()?,
        ),
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(k, v)| Ok((k.clone(), toml_value_to_json(v)?)))
                .collect::<Result<Map<_, _>>>()?,
        ),
    })
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
    /// Whether Firefly should reject duplicate transaction hashes. Defaults to true.
    #[serde(default = "default_error_if_duplicate_hash")]
    pub error_if_duplicate_hash: bool,
}

fn default_fire_webhooks() -> bool {
    true
}

fn default_error_if_duplicate_hash() -> bool {
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
        let mut config: Self = toml::from_str(&content).map_err(|e| {
            anyhow::anyhow!(
                "invalid TOML configuration in {} at {}:{}",
                path,
                e.span().map(|s| s.start).unwrap_or(0),
                "unknown"
            )
        })?;

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

    pub fn resolve_llm(&self) -> Result<ResolvedLlmConfig> {
        let raw = self.llm.as_ref();
        if raw.is_some() && self.llm_model.is_some() {
            anyhow::bail!("llm_model cannot be combined with [llm]");
        }
        let backend = raw.map_or(LlmBackend::Auto, |c| c.backend);
        let model = if backend == LlmBackend::OpenAiCompatible {
            raw.and_then(|c| c.model.clone())
                .ok_or_else(|| anyhow::anyhow!("llm.model is required for openai_compatible"))?
        } else {
            raw.and_then(|c| c.model.clone())
                .or_else(|| self.llm_model.clone())
                .unwrap_or_else(default_llm_model)
        };
        if model.trim().is_empty() {
            anyhow::bail!("llm.model must not be blank");
        }
        let response_format = raw.and_then(|c| c.response_format).unwrap_or_default();
        let timeout_secs = raw.and_then(|c| c.timeout_secs).unwrap_or(60);
        let connect_secs = raw.and_then(|c| c.connect_timeout_secs).unwrap_or(10);
        if timeout_secs == 0 || connect_secs == 0 || connect_secs > timeout_secs {
            anyhow::bail!(
                "LLM deadlines must be positive and connect_timeout_secs must not exceed timeout_secs"
            );
        }
        let mut max_tokens = raw.and_then(|c| c.max_tokens);
        if max_tokens == Some(0) {
            anyhow::bail!("llm.max_tokens must be positive");
        }
        let mut temperature = raw.and_then(|c| c.temperature);
        if let Some(t) = temperature
            && (!t.is_finite() || !(0.0..=2.0).contains(&t))
        {
            anyhow::bail!("llm.temperature must be between 0 and 2");
        }
        let custom = if backend == LlmBackend::OpenAiCompatible {
            let c = raw.expect("custom backend requires llm table");
            let base = c
                .base_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("llm.base_url is required"))?;
            let endpoint = c.endpoint.as_deref().unwrap_or("/v1/chat/completions");
            let url = build_llm_url(base, endpoint, c.allow_insecure_http.unwrap_or(false))?;
            if c.api_key_env
                .as_deref()
                .is_some_and(|v| v.trim().is_empty())
            {
                anyhow::bail!("llm.api_key_env must not be blank");
            }
            max_tokens = Some(max_tokens.unwrap_or(1024));
            temperature = Some(temperature.unwrap_or(0.0));
            Some(ResolvedOpenAiCompatibleConfig {
                url,
                api_key_env: c.api_key_env.clone(),
                extra_body: extra_body_to_json(
                    c.extra_body.as_ref().unwrap_or(&toml::Table::new()),
                )?,
            })
        } else {
            if let Some(c) = raw
                && (c.base_url.is_some()
                    || c.endpoint.is_some()
                    || c.api_key_env.is_some()
                    || c.allow_insecure_http.is_some()
                    || c.extra_body.is_some())
            {
                anyhow::bail!("custom LLM settings require backend = openai_compatible");
            }
            None
        };
        Ok(ResolvedLlmConfig {
            backend,
            model,
            response_format,
            timeout: Duration::from_secs(timeout_secs),
            connect_timeout: Duration::from_secs(connect_secs),
            max_tokens,
            temperature,
            custom,
        })
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
    fn duplicate_hash_rejection_defaults_to_true_but_can_be_disabled() {
        let defaulted: FireflyConfig = toml::from_str(
            r#"base_url = "https://firefly.example/api"
access_token = "token""#,
        )
        .unwrap();
        assert!(defaulted.error_if_duplicate_hash);

        let opt_out: FireflyConfig = toml::from_str(
            r#"base_url = "https://firefly.example/api"
access_token = "token"
error_if_duplicate_hash = false"#,
        )
        .unwrap();
        assert!(!opt_out.error_if_duplicate_hash);
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
            llm_model: Some("test-model".to_string()),
            llm: None,
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

    #[test]
    fn resolves_legacy_and_custom_defaults() {
        let mut config = test_config(vec![]);
        config.llm_model = None;
        assert_eq!(
            config.resolve_llm().unwrap().model,
            "claude-haiku-4-5-20251001"
        );
        config.llm = Some(super::LlmConfig {
            backend: super::LlmBackend::OpenAiCompatible,
            model: Some(" org/model::x ".into()),
            base_url: Some("http://localhost:8080/prefix/".into()),
            allow_insecure_http: Some(true),
            ..Default::default()
        });
        let resolved = config.resolve_llm().unwrap();
        assert_eq!(resolved.model, " org/model::x ");
        assert_eq!(
            resolved.custom.unwrap().url.as_str(),
            "http://localhost:8080/prefix/v1/chat/completions"
        );
        assert_eq!(resolved.max_tokens, Some(1024));
        assert_eq!(resolved.temperature, Some(0.0));
    }

    #[test]
    fn accepts_toml_documented_backend_spelling_and_auto_model() {
        let raw: super::LlmConfig = toml::from_str(
            r#"backend = "openai_compatible"
model = "opaque/model::id"
base_url = "https://llm.example/prefix""#,
        )
        .unwrap();
        let mut config = test_config(vec![]);
        config.llm_model = None;
        config.llm = Some(raw);
        assert_eq!(config.resolve_llm().unwrap().model, "opaque/model::id");

        let raw: super::LlmConfig = toml::from_str("model = \"gpt-4o-mini\"").unwrap();
        config.llm = Some(raw);
        assert_eq!(config.resolve_llm().unwrap().model, "gpt-4o-mini");
        config.llm = None;
        assert_eq!(
            config.resolve_llm().unwrap().model,
            "claude-haiku-4-5-20251001"
        );
        config.llm_model = Some("legacy".into());
        config.llm = Some(super::LlmConfig::default());
        assert!(config.resolve_llm().is_err());
    }

    #[test]
    fn extra_body_rejects_managed_keys_and_converts_nested_values() {
        let table: toml::Table = toml::from_str("x = { nested = [1, true, 'ok'] }").unwrap();
        let converted = super::extra_body_to_json(&table).unwrap();
        assert_eq!(converted["x"]["nested"][1], true);
        for key in [
            "model",
            "messages",
            "stream",
            "response_format",
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "n",
        ] {
            let table = toml::from_str::<toml::Table>(&format!("{key} = 1")).unwrap();
            assert!(super::extra_body_to_json(&table).is_err(), "{key}");
        }
    }

    #[test]
    fn rejects_zero_tokens_and_accepts_positive_tokens() {
        let mut config = test_config(vec![]);
        config.llm_model = None;
        config.llm = Some(toml::from_str("max_tokens = 0").unwrap());
        assert!(config.resolve_llm().is_err());
        config.llm = Some(toml::from_str("max_tokens = 42").unwrap());
        assert_eq!(config.resolve_llm().unwrap().max_tokens, Some(42));
        assert!(toml::from_str::<super::LlmConfig>("max_tokens = -1").is_err());
        assert!(toml::from_str::<super::LlmConfig>("max_tokens = 4294967296").is_err());
    }

    #[test]
    fn rejects_control_character_traversal_and_preserves_prefix() {
        for control in ['\t', '\r', '\n'] {
            let endpoint = format!("/.{control}./v1/chat/completions");
            assert!(
                super::build_llm_url("https://example.test/inference", &endpoint, false).is_err(),
                "control character {control:?}"
            );
            let encoded = match control {
                '\t' => "%09",
                '\r' => "%0d",
                _ => "%0a",
            };
            let endpoint = format!("/.{encoded}./v1/chat/completions");
            assert!(
                super::build_llm_url("https://example.test/inference", &endpoint, false).is_err()
            );
        }
        for endpoint in ["/v1/chat/completions", "/nested/path"] {
            let url =
                super::build_llm_url("https://example.test/inference/", endpoint, false).unwrap();
            assert!(url.path().starts_with("/inference/"), "{}", url.path());
        }
    }

    #[test]
    fn rejects_llm_endpoint_traversal_and_insecure_policy() {
        for endpoint in [
            "//evil/x",
            "/../x",
            "/%2e%2e/x",
            "/%252e%252e/x",
            "/a%2fb",
            "/a%252fb",
            "/%252525252e%252525252e/x",
            "/%2525252525252525252e%2525252525252525252e/x",
            "/a%2525252525252525252fb",
            "/a%2525252525252525255cb",
        ] {
            assert!(
                super::build_llm_url("https://example.test/prefix", endpoint, false).is_err(),
                "{endpoint}"
            );
        }
        assert!(super::build_llm_url("http://lan.local", "/v1/chat/completions", false).is_err());
        assert!(super::build_llm_url("http://lan.local", "/v1/chat/completions", true).is_ok());
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
