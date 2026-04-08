use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::debug;

/// Filesystem-backed store for raw RFC 5322 email messages.
pub struct MessageStore {
    data_dir: PathBuf,
}

impl MessageStore {
    pub fn new(data_dir: &str) -> Self {
        Self {
            data_dir: PathBuf::from(data_dir),
        }
    }

    /// Save a raw email message to disk.
    /// Returns the path where the message was stored.
    pub async fn save(
        &self,
        account_id: &str,
        mailbox: &str,
        uid: u32,
        raw_bytes: &[u8],
    ) -> Result<PathBuf> {
        let safe_account = sanitize_filename::sanitize(account_id);
        let safe_mailbox = sanitize_filename::sanitize(mailbox);
        let dir = self.data_dir.join(&safe_account).join(&safe_mailbox);

        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating directory: {}", dir.display()))?;

        // Verify the resolved path is still under data_dir. This catches
        // symlink-based escapes that sanitization alone cannot prevent.
        let canonical_dir = dir.canonicalize()
            .with_context(|| format!("canonicalizing directory: {}", dir.display()))?;
        let canonical_data_dir = self.data_dir.canonicalize()
            .with_context(|| format!("canonicalizing data_dir: {}", self.data_dir.display()))?;
        if !canonical_dir.starts_with(&canonical_data_dir) {
            bail!(
                "directory {} escapes data_dir {}",
                canonical_dir.display(),
                canonical_data_dir.display()
            );
        }

        let path = dir.join(format!("{uid}.eml"));
        tokio::fs::write(&path, raw_bytes)
            .await
            .with_context(|| format!("writing message to: {}", path.display()))?;

        debug!(path = %path.display(), size = raw_bytes.len(), "saved raw message");
        Ok(path)
    }

    /// Load a raw email message from disk.
    #[allow(dead_code)]
    pub async fn load(&self, path: &Path) -> Result<Vec<u8>> {
        tokio::fs::read(path)
            .await
            .with_context(|| format!("reading message from: {}", path.display()))
    }

    /// Delete a raw email message from disk.
    #[allow(dead_code)]
    pub async fn delete(&self, path: &Path) -> Result<()> {
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("deleting message: {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_save_and_load() {
        let dir = TempDir::new().unwrap();
        let store = MessageStore::new(dir.path().to_str().unwrap());

        let raw = b"From: test@example.com\r\nSubject: Test\r\n\r\nHello";
        let path = store.save("acct1", "INBOX", 42, raw).await.unwrap();

        assert!(path.exists());
        assert!(path.to_str().unwrap().ends_with("42.eml"));

        let loaded = store.load(&path).await.unwrap();
        assert_eq!(loaded, raw);
    }

    #[tokio::test]
    async fn test_delete() {
        let dir = TempDir::new().unwrap();
        let store = MessageStore::new(dir.path().to_str().unwrap());

        let raw = b"test message";
        let path = store.save("acct1", "INBOX", 1, raw).await.unwrap();
        assert!(path.exists());

        store.delete(&path).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn test_malicious_mailbox_name_sanitized() {
        let dir = TempDir::new().unwrap();
        let store = MessageStore::new(dir.path().to_str().unwrap());

        let raw = b"test message";
        let path = store.save("acct1", "../../etc", 1, raw).await.unwrap();

        // sanitize-filename replaces ".." so the path stays under data_dir
        assert!(path.starts_with(dir.path()));
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_slash_in_mailbox_name_sanitized() {
        let dir = TempDir::new().unwrap();
        let store = MessageStore::new(dir.path().to_str().unwrap());

        let raw = b"test message";
        let path = store.save("acct1", "Folder/Sub", 1, raw).await.unwrap();

        assert!(path.starts_with(dir.path()));
        assert!(path.exists());
    }
}
