use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub struct CategoryCache {
    path: PathBuf,
    identity: String,
    ttl: i64,
}

impl CategoryCache {
    pub fn new(path: Option<&str>, base_url: &str, token: &str, ttl: u32) -> Option<Self> {
        if ttl == 0 {
            return None;
        }
        let path = match path {
            Some(path) => PathBuf::from(path),
            None => default_directory()?.join("categories.sqlite"),
        };
        let mut digest = Sha256::new();
        digest.update(base_url.as_bytes());
        digest.update([0]);
        digest.update(token.as_bytes());
        Some(Self {
            path,
            identity: format!("{:x}", digest.finalize()),
            ttl: i64::from(ttl.min(3600)),
        })
    }

    fn connection(&self, write: bool) -> Result<Option<Connection>> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        ensure_private_directory(parent)?;
        if !self.path.exists() && !write {
            return Ok(None);
        }
        let flags = if write {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let connection =
            Connection::open_with_flags(&self.path, flags | OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
        connection.busy_timeout(std::time::Duration::from_millis(100))?;
        Ok(Some(connection))
    }

    pub fn read(&self) -> Result<Option<Vec<String>>> {
        self.read_at(chrono::Utc::now().timestamp())
    }
    fn read_at(&self, now: i64) -> Result<Option<Vec<String>>> {
        let Some(connection) = self.connection(false)? else {
            return Ok(None);
        };
        let value: Result<String, _> = connection.query_row(
            "SELECT payload FROM category_cache WHERE identity=?1 AND stored_at BETWEEN ?2 AND ?3 AND length(CAST(payload AS BLOB))<=1048576",
            params![self.identity, now.saturating_sub(self.ttl), now], |row| row.get(0));
        match value {
            Ok(value) => Ok(Some(
                serde_json::from_str(&value).context("invalid cached categories")?,
            )),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn write(&self, categories: &[String]) -> Result<()> {
        self.write_at(categories, chrono::Utc::now().timestamp())
    }
    fn write_at(&self, categories: &[String], now: i64) -> Result<()> {
        let payload = serde_json::to_string(categories)?;
        if payload.len() > 1024 * 1024 {
            anyhow::bail!("category cache entry exceeds size limit");
        }
        let mut connection = self.connection(true)?.unwrap();
        let transaction = connection.transaction()?;
        transaction.execute_batch("CREATE TABLE IF NOT EXISTS category_cache(identity TEXT PRIMARY KEY, stored_at INTEGER NOT NULL, payload TEXT NOT NULL)")?;
        transaction.execute("INSERT INTO category_cache(identity,stored_at,payload) VALUES (?1,?2,?3) ON CONFLICT(identity) DO UPDATE SET stored_at=excluded.stored_at,payload=excluded.payload", params![self.identity,now,payload])?;
        transaction.execute(
            "DELETE FROM category_cache WHERE stored_at < ?1",
            params![now.saturating_sub(3600)],
        )?;
        transaction.execute_batch("DELETE FROM category_cache WHERE rowid NOT IN (SELECT rowid FROM category_cache ORDER BY stored_at DESC, rowid DESC LIMIT 64)")?;
        transaction.commit()?;
        Ok(())
    }
}

fn default_directory() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no arguments and cannot mutate process state.
        Some(std::env::temp_dir().join(format!("mailtx-cache-{}", unsafe { libc::geteuid() })))
    }
    #[cfg(not(unix))]
    {
        std::env::var_os("LOCALAPPDATA").map(|path| PathBuf::from(path).join("mailtx"))
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    if !path.exists() {
        builder.create(path)?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!("category cache directory is not a regular directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid is a side-effect-free process identity query.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            anyhow::bail!("category cache directory must be owned by this user and private");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persisted_cache_expires_and_is_isolated_by_endpoint_credentials() {
        let root = std::env::temp_dir().join(format!(
            "mailtx-cache-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("categories.sqlite");
        let cache = CategoryCache::new(
            Some(path.to_str().unwrap()),
            "https://test/api",
            "private-test-token",
            300,
        )
        .unwrap();
        let categories = vec!["Food".into()];
        cache.write_at(&categories, 1000).unwrap();
        let restarted = CategoryCache::new(
            Some(path.to_str().unwrap()),
            "https://test/api",
            "private-test-token",
            300,
        )
        .unwrap();
        assert_eq!(restarted.read_at(1001).unwrap(), Some(categories));
        assert!(restarted.read_at(1301).unwrap().is_none());
        assert!(restarted.read_at(999).unwrap().is_none());
        let other = CategoryCache::new(
            Some(path.to_str().unwrap()),
            "https://test/api",
            "another-token",
            300,
        )
        .unwrap();
        assert!(other.read_at(1001).unwrap().is_none());
        let bytes = std::fs::read(path).unwrap();
        assert!(
            !bytes
                .windows(b"private-test-token".len())
                .any(|window| window == b"private-test-token")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
