use anyhow::{Context, Result};
use mail_parser::MessageParser;

/// Read and parse an `.eml` file, returning a plain-text representation of
/// the body. Prefers an explicit text/plain part; falls back to converting HTML.
pub fn extract_body(path: &str) -> Result<String> {
    let raw = std::fs::read(path).with_context(|| format!("reading email file: {path}"))?;

    let message = MessageParser::default()
        .parse(&raw)
        .ok_or_else(|| anyhow::anyhow!("failed to parse email at {path}"))?;

    if let Some(text) = message.body_text(0) {
        return Ok(text.trim().to_string());
    }

    if let Some(html) = message.body_html(0) {
        return html_to_text(&html);
    }

    Ok(String::new())
}

fn html_to_text(html: &str) -> Result<String> {
    html2text::from_read(html.as_bytes(), 120)
        .map_err(|e| anyhow::anyhow!("HTML to text conversion failed: {e}"))
}
