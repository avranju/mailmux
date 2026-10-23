use anyhow::{Context, Result};
use mail_parser::MessageParser;

/// Read and parse an `.eml` file, returning a plain-text representation of
/// the body. Prefers an explicit text/plain part; falls back to converting HTML.
pub struct AuthenticatedEmail {
    pub subject: String,
    pub body: String,
}

pub fn extract_authenticated(
    path: &str,
    sender: &str,
    trusted: &[String],
) -> Result<Option<AuthenticatedEmail>> {
    let raw = std::fs::read(path).with_context(|| format!("reading email file: {path}"))?;

    let message = MessageParser::default()
        .parse(&raw)
        .ok_or_else(|| anyhow::anyhow!("failed to parse email at {path}"))?;

    if !crate::auth::authenticated(&message, sender, trusted) {
        return Ok(None);
    }
    let body = if let Some(text) = message.body_text(0) {
        text.trim().to_string()
    } else if let Some(html) = message.body_html(0) {
        html_to_text(&html)?
    } else {
        String::new()
    };
    Ok(Some(AuthenticatedEmail {
        subject: message.subject().unwrap_or_default().to_owned(),
        body,
    }))
}

fn html_to_text(html: &str) -> Result<String> {
    html2text::from_read(html.as_bytes(), 120)
        .map_err(|e| anyhow::anyhow!("HTML to text conversion failed: {e}"))
}
