use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    env,
    net::SocketAddr,
    path::{Path, PathBuf},
};

// Tantivy 0.26.1 keeps these writer-memory limits internal, but they are part
// of the IndexWriter contract. Keep the values here in sync with Tantivy's
// MEMORY_BUDGET_NUM_BYTES_MIN/MAX constants so invalid configurations fail
// before opening the index.
pub const TANTIVY_WRITER_MEMORY_MIN: usize = 15_000_000;

pub const TANTIVY_WRITER_MEMORY_MAX: usize = u32::MAX as usize - 1_000_000;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub index: IndexConfig,
    pub content: ContentConfig,
    pub search: SearchConfig,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub public_base_url: String,
    pub max_request_bytes: usize,
    pub api_token_env: Option<String>,
    #[serde(default = "default_true")]
    pub protect_view: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StorageConfig {
    pub database_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct IndexConfig {
    pub path: PathBuf,
    #[serde(default = "default_writer_memory")]
    pub writer_memory_bytes: usize,
    #[serde(default = "default_batch")]
    pub batch_size: usize,
    #[serde(default = "default_commit_ms")]
    pub commit_interval_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ContentConfig {
    #[serde(default = "default_body")]
    pub max_body_chars: usize,
    #[serde(default = "default_attachment_bytes")]
    pub max_attachment_bytes: usize,
    #[serde(default = "default_attachment_text")]
    pub max_attachment_text_chars: usize,
    #[serde(default = "default_true")]
    pub pdf_enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "default_limit")]
    pub default_limit: usize,
    #[serde(default = "default_max_limit")]
    pub max_limit: usize,
    #[serde(default = "default_get")]
    pub max_get_chars: usize,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let c: Self = toml::from_str(&text).context("parse TOML config")?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        if self.server.max_request_bytes == 0 {
            bail!("server.max_request_bytes must be positive")
        };
        if self.search.default_limit == 0
            || self.search.max_limit == 0
            || self.search.default_limit > self.search.max_limit
            || self.search.max_limit > 50
            || self.search.max_get_chars == 0
        {
            bail!("invalid search limits")
        };
        if self.content.max_body_chars == 0
            || self.content.max_attachment_bytes == 0
            || self.content.max_attachment_text_chars == 0
            || self.index.batch_size == 0
            || self.index.commit_interval_ms == 0
        {
            bail!("operational limits must be positive")
        };
        if self.index.writer_memory_bytes < TANTIVY_WRITER_MEMORY_MIN
            || self.index.writer_memory_bytes >= TANTIVY_WRITER_MEMORY_MAX
        {
            bail!(
                "index.writer_memory_bytes must be at least {TANTIVY_WRITER_MEMORY_MIN} and less than {TANTIVY_WRITER_MEMORY_MAX}"
            )
        };
        let base =
            url::Url::parse(&self.server.public_base_url).context("invalid public_base_url")?;
        if base.scheme() != "http" && base.scheme() != "https" {
            bail!("public_base_url must use http or https")
        }
        if !self.storage.database_path.is_absolute() || !self.index.path.is_absolute() {
            bail!("database_path and index.path must be absolute")
        }

        if let Some(p) = self
            .storage
            .database_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            std::fs::create_dir_all(p)?
        };
        std::fs::create_dir_all(&self.index.path)?;
        Ok(())
    }

    pub fn validate_server_auth(&self) -> Result<()> {
        if let Some(name) = &self.server.api_token_env
            && env::var(name)
                .ok()
                .filter(|value| !value.is_empty())
                .is_none()
        {
            bail!("configured api_token_env is missing or empty")
        }
        if !self.server.bind.ip().is_loopback() && self.token().is_none() {
            bail!("non-loopback bind requires api_token_env")
        }
        Ok(())
    }

    pub fn token(&self) -> Option<String> {
        self.server
            .api_token_env
            .as_ref()
            .and_then(|n| env::var(n).ok())
            .filter(|s| !s.is_empty())
    }

    pub fn mcp_allowed_hosts(&self) -> Vec<String> {
        let Some(host) = url::Url::parse(&self.server.public_base_url).ok() else {
            return vec![];
        };
        let Some(name) = host.host_str() else {
            return vec![];
        };
        let authority = host.port().map(|port| format!("{name}:{port}"));
        authority
            .into_iter()
            .chain(std::iter::once(name.to_owned()))
            .collect()
    }

    pub fn view_url(&self, source: &str, id: &str) -> String {
        format!(
            "{}/view/{}/{}",
            self.server.public_base_url.trim_end_matches('/'),
            urlencoding(source),
            urlencoding(id)
        )
    }
}

fn urlencoding(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn default_true() -> bool {
    true
}

fn default_writer_memory() -> usize {
    128 * 1024 * 1024
}

fn default_batch() -> usize {
    100
}

fn default_commit_ms() -> u64 {
    1000
}

fn default_body() -> usize {
    500_000
}

fn default_attachment_bytes() -> usize {
    25 * 1024 * 1024
}

fn default_attachment_text() -> usize {
    500_000
}

fn default_limit() -> usize {
    10
}

fn default_max_limit() -> usize {
    50
}

fn default_get() -> usize {
    100_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_config(path: &Path, writer_memory_bytes: usize) -> Config {
        Config {
            server: ServerConfig {
                bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                public_base_url: "http://127.0.0.1".into(),
                max_request_bytes: 1,
                api_token_env: None,
                protect_view: true,
            },
            storage: StorageConfig {
                database_path: path.join("mail.db"),
            },
            index: IndexConfig {
                path: path.join("index"),
                writer_memory_bytes,
                batch_size: 1,
                commit_interval_ms: 1,
            },
            content: ContentConfig {
                max_body_chars: 1,
                max_attachment_bytes: 1,
                max_attachment_text_chars: 1,
                pdf_enabled: false,
            },
            search: SearchConfig {
                default_limit: 1,
                max_limit: 1,
                max_get_chars: 1,
            },
        }
    }

    #[test]
    fn tantivy_memory_bounds_are_validated_at_configuration_time() {
        assert_eq!(TANTIVY_WRITER_MEMORY_MIN, 15_000_000);
        let dir = tempfile::tempdir().unwrap();

        let too_small = test_config(dir.path(), TANTIVY_WRITER_MEMORY_MIN - 1)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(too_small.contains("at least"));

        test_config(dir.path(), TANTIVY_WRITER_MEMORY_MIN)
            .validate()
            .unwrap();

        let too_large = test_config(dir.path(), TANTIVY_WRITER_MEMORY_MAX)
            .validate()
            .unwrap_err()
            .to_string();
        assert!(too_large.contains("less than"));
    }
}
