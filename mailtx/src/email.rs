use anyhow::{Context, Result};
use mail_parser::MessageParser;

const MAX_BODY_BYTES: usize = 32 * 1024;

fn bounded_body(mut body: String) -> String {
    const MARKER: &str = "\n[body truncated]";
    if body.len() > MAX_BODY_BYTES {
        let mut end = MAX_BODY_BYTES - MARKER.len();
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str(MARKER);
        tracing::warn!(
            limit_bytes = MAX_BODY_BYTES,
            "email body truncated before LLM extraction"
        );
    }
    body
}

/// Read and parse an `.eml` file, returning a plain-text representation of
/// the body. Prefers an explicit text/plain part; falls back to converting HTML.
pub fn extract_body(path: &str) -> Result<String> {
    let raw = std::fs::read(path).with_context(|| format!("reading email file: {path}"))?;

    let message = MessageParser::default()
        .parse(&raw)
        .ok_or_else(|| anyhow::anyhow!("failed to parse email at {path}"))?;

    if let Some(text) = message.body_text(0) {
        return Ok(bounded_body(text.trim().to_string()));
    }

    if let Some(html) = message.body_html(0) {
        return html_to_text(&html).map(bounded_body);
    }

    Ok(String::new())
}

fn html_to_text(html: &str) -> Result<String> {
    html2text::from_read(html.as_bytes(), 120)
        .map_err(|e| anyhow::anyhow!("HTML to text conversion failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn llm_body_limit_applies_to_plain_and_html_email() {
        for content_type in ["text/plain", "text/html"] {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            write!(file, "From: bank@example.com\r\nMIME-Version: 1.0\r\nContent-Type: {content_type}; charset=utf-8\r\n\r\n{}", "x".repeat(MAX_BODY_BYTES * 2)).unwrap();
            let body = extract_body(file.path().to_str().unwrap()).unwrap();
            assert!(body.len() <= MAX_BODY_BYTES);
            assert!(body.ends_with("[body truncated]"));
        }
    }

    #[test]
    fn body_bound_preserves_utf8_and_small_messages() {
        assert_eq!(
            bounded_body("normal transaction".into()),
            "normal transaction"
        );
        let body = bounded_body("€".repeat(MAX_BODY_BYTES));
        assert!(body.len() <= MAX_BODY_BYTES);
        assert!(body.ends_with("[body truncated]"));
    }
}
