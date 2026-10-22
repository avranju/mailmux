use std::fmt;

#[derive(Debug)]
pub enum ImapFailure {
    Authentication,
    Timeout(&'static str),
    Transport(anyhow::Error),
    Protocol(anyhow::Error),
    IdleUnsupported,
}

impl ImapFailure {
    pub fn transport(error: impl Into<anyhow::Error>) -> Self {
        Self::Transport(error.into())
    }

    pub fn from_stream(error: imap_next::stream::Error<imap_next::client::Error>) -> Self {
        match error {
            imap_next::stream::Error::State(error) => Self::Protocol(error.into()),
            error => Self::transport(error),
        }
    }
}

impl fmt::Display for ImapFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication => f.write_str("IMAP authentication failed"),
            Self::Timeout(operation) => write!(f, "IMAP {operation} timed out"),
            Self::Transport(_) => f.write_str("IMAP transport failed"),
            Self::Protocol(_) => f.write_str("IMAP protocol failed"),
            Self::IdleUnsupported => f.write_str("IMAP IDLE is unsupported"),
        }
    }
}

impl std::error::Error for ImapFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) | Self::Protocol(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FailureKind {
    Authentication,
    Timeout,
    Network,
    Protocol,
    IdleUnsupported,
    Other,
}

pub fn kind(error: &anyhow::Error) -> FailureKind {
    for source in error.chain() {
        if let Some(error) = source.downcast_ref::<ImapFailure>() {
            return match error {
                ImapFailure::Authentication => FailureKind::Authentication,
                ImapFailure::Timeout(_) => FailureKind::Timeout,
                ImapFailure::Transport(source) => {
                    if source.chain().any(|e| {
                        e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::TimedOut)
                    }) {
                        FailureKind::Timeout
                    } else {
                        FailureKind::Network
                    }
                }
                ImapFailure::Protocol(_) => FailureKind::Protocol,
                ImapFailure::IdleUnsupported => FailureKind::IdleUnsupported,
            };
        }
    }
    FailureKind::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_survives_context_and_never_infers_types_from_words() {
        let timeout = anyhow::Error::new(ImapFailure::Timeout("test"))
            .context("authentication not available");
        assert_eq!(kind(&timeout), FailureKind::Timeout);
        let network = anyhow::Error::new(ImapFailure::transport(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "timed out authentication",
        )))
        .context("wrapper");
        assert_eq!(kind(&network), FailureKind::Network);
        assert_eq!(
            kind(&anyhow::anyhow!(
                "LOGIN rejected: authentication TLS connection timed out"
            )),
            FailureKind::Other
        );
        let disk = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "connection refused",
        ));
        assert_eq!(kind(&disk), FailureKind::Other);
        assert_eq!(
            kind(&anyhow::Error::new(ImapFailure::Authentication).context("another wrapper")),
            FailureKind::Authentication
        );
        let timeout = anyhow::Error::new(ImapFailure::transport(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "opaque",
        )));
        assert_eq!(kind(&timeout), FailureKind::Timeout);
    }
}
