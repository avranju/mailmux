#![allow(dead_code)]
use mailindex::{
    config::{Config, ContentConfig, IndexConfig, SearchConfig, ServerConfig, StorageConfig},
    storage::Repository,
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};
use tempfile::TempDir;

pub fn config(dir: &TempDir) -> Config {
    Config {
        server: ServerConfig {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            public_base_url: "http://127.0.0.1:8090".into(),
            max_request_bytes: 2_000_000,
            api_token_env: None,
            protect_view: true,
        },
        storage: StorageConfig {
            database_path: dir.path().join("mail.db"),
        },
        index: IndexConfig {
            path: dir.path().join("index"),
            writer_memory_bytes: 16 * 1024 * 1024,
            batch_size: 10,
            commit_interval_ms: 50,
        },
        content: ContentConfig {
            max_body_chars: 10_000,
            max_attachment_bytes: 10_000,
            max_attachment_text_chars: 10_000,
            pdf_enabled: true,
        },
        search: SearchConfig {
            default_limit: 10,
            max_limit: 50,
            max_get_chars: 1_000,
        },
    }
}

pub async fn repository(dir: &TempDir) -> Arc<Repository> {
    let repo = Arc::new(Repository::open(&dir.path().join("mail.db")).await.unwrap());
    repo.migrate().await.unwrap();
    repo
}

pub fn fixture(name: &str) -> &'static [u8] {
    match name {
        "plain.eml" => include_bytes!("../fixtures/plain.eml"),
        "multipart-alternative.eml" => include_bytes!("../fixtures/multipart-alternative.eml"),
        "blank-plain-html.eml" => include_bytes!("../fixtures/blank-plain-html.eml"),
        "html-only.eml" => include_bytes!("../fixtures/html-only.eml"),
        "unicode.eml" => include_bytes!("../fixtures/unicode.eml"),
        "text-attachment.eml" => include_bytes!("../fixtures/text-attachment.eml"),
        "pdf-attachment.eml" => include_bytes!("../fixtures/pdf-attachment.eml"),
        "unsupported-binary.eml" => include_bytes!("../fixtures/unsupported-binary.eml"),
        "malformed-pdf.eml" => include_bytes!("../fixtures/malformed-pdf.eml"),
        "duplicate-message-id-a.eml" => include_bytes!("../fixtures/duplicate-message-id-a.eml"),
        "duplicate-message-id-b.eml" => include_bytes!("../fixtures/duplicate-message-id-b.eml"),
        "missing-headers.eml" => include_bytes!("../fixtures/missing-headers.eml"),
        _ => panic!("unknown fixture"),
    }
}

pub fn metadata() -> serde_json::Value {
    serde_json::json!({"account_id":"acct","mailbox_name":"INBOX","uid":7,"producer":"test"})
}

pub fn path(dir: &TempDir, name: &str) -> PathBuf {
    dir.path().join(name)
}
