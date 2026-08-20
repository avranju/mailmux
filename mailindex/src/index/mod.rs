pub mod rebuild;
pub mod schema;
pub mod worker;

use crate::storage::models::StoredDocument;
use anyhow::{Context, Result};
use std::path::Path;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy};

#[derive(Clone)]
pub struct SearchIndex {
    pub index: Index,
    pub reader: IndexReader,
    pub fields: schema::IndexFields,
}

pub fn document_key(source: &str, id: &str) -> String {
    format!("{}:{}{}", source.len(), source, id)
}

pub fn decode_key(k: &str) -> Option<(String, String)> {
    let (n, rest) = k.split_once(':')?;
    let n: usize = n.parse().ok()?;
    if rest.len() < n || !rest.is_char_boundary(n) {
        return None;
    }
    let (s, id) = rest.split_at(n);
    if s.is_empty() || id.is_empty() {
        return None;
    }
    Some((s.into(), id.into()))
}

impl SearchIndex {
    pub fn open(path: &Path, memory: usize) -> Result<(Self, IndexWriter)> {
        let (schema, _fields) = schema::build();
        std::fs::create_dir_all(path)?;
        let index = if path.join("meta.json").exists() {
            Index::open_in_dir(path)
                .context("open Tantivy index; run rebuild-index if schema is incompatible")?
        } else {
            Index::create_in_dir(path, schema)?
        };
        schema::validate(&index.schema())?;
        let actual = schema::fields(&index.schema())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let writer = index.writer(memory)?;
        Ok((
            Self {
                index,
                reader,
                fields: actual,
            },
            writer,
        ))
    }

    pub fn reload(&self) -> Result<()> {
        self.reader.reload()?;
        Ok(())
    }
}

pub fn tantivy_doc(
    d: &StoredDocument,
    f: &schema::IndexFields,
) -> tantivy::schema::TantivyDocument {
    let mut x = tantivy::schema::TantivyDocument::default();
    x.add_text(f.document_key, document_key(&d.source, &d.source_id));
    x.add_text(f.source, &d.source);
    if let Some(v) = &d.account_id {
        x.add_text(f.account_id, v)
    }
    if let Some(v) = &d.mailbox_name {
        x.add_text(f.mailbox_name, v)
    }
    if let Some(v) = &d.sent_at
        && let Ok(t) = chrono::DateTime::parse_from_rfc3339(v)
        && let Some(timestamp) = t.timestamp_nanos_opt()
    {
        x.add_i64(f.sent_timestamp, timestamp)
    }
    if let Some(v) = &d.sender {
        x.add_text(f.sender_exact, normalize_sender(v));
        x.add_text(f.sender_text, v)
    }
    x.add_text(
        f.recipients_text,
        d.to.iter()
            .chain(d.cc.iter())
            .chain(d.bcc.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
    );
    if let Some(v) = &d.subject {
        x.add_text(f.subject, v)
    }
    x.add_text(f.body, &d.body_text);
    for a in &d.attachments {
        if let Some(v) = &a.extracted_text {
            x.add_text(f.attachment_text, v)
        }
    }
    x
}

pub fn normalize_sender(value: &str) -> String {
    value
        .rsplit_once('<')
        .and_then(|(_, rest)| rest.strip_suffix('>'))
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_reject_malformed_lengths() {
        assert!(decode_key("x:value").is_none());
        assert!(decode_key("99:value").is_none());
        assert!(decode_key("1:").is_none());
        assert!(decode_key("1:é").is_none());
    }

    #[test]
    fn schema_validation_checks_options() {
        let (schema, _) = schema::build();
        assert!(schema::validate(&schema).is_ok());
        let mut changed = tantivy::schema::Schema::builder();
        changed.add_text_field("document_key", tantivy::schema::TEXT);
        assert!(schema::validate(&changed.build()).is_err());
    }
}
