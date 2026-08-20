pub mod extractors;
pub mod normalize;
pub mod parser;

use crate::config::ContentConfig;
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct NormalizedMessage {
    pub source: String,
    pub source_id: String,
    pub producer_metadata_json: String,
    pub account_id: Option<String>,
    pub mailbox_name: Option<String>,
    pub imap_uid: Option<i64>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub sent_at: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Vec<String>,
    pub body: String,
    pub body_truncated: bool,
    pub raw_sha256: String,
    pub attachments: Vec<NormalizedAttachment>,
}

#[derive(Clone, Debug)]
pub struct NormalizedAttachment {
    pub part_index: i64,
    pub filename: Option<String>,
    pub media_type: Option<String>,
    pub disposition: Option<String>,
    pub content_id: Option<String>,
    pub size_bytes: i64,
    pub sha256: String,
    pub status: String,
    pub error: Option<String>,
    pub text: Option<String>,
    pub text_truncated: bool,
}

pub fn validate_identity(s: &str) -> Result<()> {
    if s.is_empty()
        || s.chars().count() > 255
        || s.chars().any(|c| c.is_control() || c == '/' || c == '\\')
    {
        bail!("invalid source identity")
    }
    Ok(())
}

pub fn sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

pub fn normalize_message(
    source: String,
    source_id: String,
    metadata: serde_json::Value,
    raw: &[u8],
    limits: &ContentConfig,
) -> Result<NormalizedMessage> {
    validate_identity(&source)?;
    validate_identity(&source_id)?;
    let parsed = parser::parse(raw, limits)?;
    let get = |k: &str| metadata.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let imap_uid = metadata.get("uid").and_then(|v| v.as_i64());
    Ok(NormalizedMessage {
        source,
        source_id,
        producer_metadata_json: serde_json::to_string(&metadata)?,
        account_id: get("account_id"),
        mailbox_name: get("mailbox_name"),
        imap_uid,
        message_id: parsed.message_id,
        in_reply_to: parsed.in_reply_to,
        references: parsed.references,
        sent_at: parsed.sent_at,
        subject: parsed.subject,
        sender: parsed.sender,
        to: parsed.to,
        cc: parsed.cc,
        bcc: parsed.bcc,
        reply_to: parsed.reply_to,
        body: parsed.body,
        body_truncated: parsed.body_truncated,
        raw_sha256: sha256(raw),
        attachments: parsed.attachments,
    })
}

#[cfg(test)]
mod tests {
    use super::validate_identity;

    #[test]
    fn identity_limit_counts_characters_not_bytes() {
        assert!(validate_identity(&"é".repeat(255)).is_ok());
        assert!(validate_identity(&"é".repeat(256)).is_err());
    }
}
