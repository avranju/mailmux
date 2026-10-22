use std::fmt;
use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials([redacted])")
    }
}

impl Credentials {
    pub fn read(path: &str) -> Result<Self> {
        let file = std::fs::File::open(path).context("opening IMAP credential file")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = file
                .metadata()
                .context("checking IMAP credential file permissions")?
                .permissions()
                .mode();
            if mode & 0o026 != 0 {
                warn!("IMAP credential file permissions should be 0600 or 0640");
            }
        }
        let mut content = String::new();
        file.take(16 * 1024 + 1)
            .read_to_string(&mut content)
            .context("reading IMAP credential file")?;
        if content.len() > 16 * 1024 {
            anyhow::bail!("IMAP credential file exceeds 16 KiB");
        }
        let value: Self = toml::from_str(&content)
            .map_err(|_| anyhow::anyhow!("invalid IMAP credential file format"))?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if self.username.trim().is_empty() || self.password.is_empty() {
            anyhow::bail!("IMAP username and password must not be empty");
        }
        Ok(())
    }
}

/// Keep the current session if a secret-manager update is temporarily absent
/// or invalid. Compare complete contents, including same-size replacements.
pub async fn wait_for_change(
    path: &str,
    previous: &Credentials,
    interval: Duration,
    token: &CancellationToken,
) -> bool {
    loop {
        tokio::select! {
            _ = token.cancelled() => return false,
            _ = tokio::time::sleep(interval) => {},
        }
        match Credentials::read(path) {
            Ok(current) if current != *previous => return true,
            Ok(_) => {}
            Err(_) => {
                warn!("IMAP credential file unavailable or invalid; waiting for a valid update")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn unchanged_and_invalid_updates_wait_then_same_size_rotation_is_detected() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(file, "username='user1'\npassword='pass1'\n").unwrap();
        let path = file.path().to_str().unwrap().to_owned();
        let previous = Credentials::read(&path).unwrap();
        assert_eq!(format!("{previous:?}"), "Credentials([redacted])");
        let token = CancellationToken::new();
        let future = wait_for_change(&path, &previous, Duration::from_millis(5), &token);
        tokio::pin!(future);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut future)
                .await
                .is_err()
        );
        std::fs::write(&path, "password='private-invalid-secret'").unwrap();
        let error = Credentials::read(&path).unwrap_err();
        assert!(!format!("{error:#}").contains("private-invalid-secret"));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut future)
                .await
                .is_err()
        );
        std::fs::write(&path, "username='user2'\npassword='pass2'\n").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut future)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn cancellation_interrupts_rotation_wait() {
        let previous = Credentials {
            username: "test".into(),
            password: "test".into(),
        };
        let token = CancellationToken::new();
        token.cancel();
        assert!(!wait_for_change("/unused", &previous, Duration::from_secs(60), &token).await);
    }
}
