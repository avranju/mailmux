pub mod migrations;
pub mod models;

use crate::{
    ingest::{NormalizedAttachment, NormalizedMessage},
    models::IndexState,
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use models::*;
use std::path::Path;
use turso::transaction::TransactionBehavior;
use turso::{Builder, Connection, Database, Value};

#[derive(Clone)]
pub struct Repository {
    db: Database,
}

fn s(v: impl Into<String>) -> Value {
    Value::Text(v.into())
}

fn i(v: i64) -> Value {
    Value::Integer(v)
}

fn opt(v: &Option<String>) -> Value {
    v.clone().map(Value::Text).unwrap_or(Value::Null)
}

fn opt_i(v: Option<i64>) -> Value {
    v.map(Value::Integer).unwrap_or(Value::Null)
}

fn json<T: serde::Serialize>(v: &T) -> Value {
    s(serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
}

fn state(v: &str) -> IndexState {
    match v {
        "indexed" => IndexState::Indexed,
        "error" => IndexState::Error,
        _ => IndexState::Pending,
    }
}

fn text(row: &turso::Row, n: usize) -> Option<String> {
    match row.get_value(n).ok()? {
        Value::Text(v) => Some(v),
        _ => None,
    }
}

fn number(row: &turso::Row, n: usize) -> Option<i64> {
    match row.get_value(n).ok()? {
        Value::Integer(v) => Some(v),
        _ => None,
    }
}

fn array(row: &turso::Row, n: usize) -> Vec<String> {
    text(row, n)
        .and_then(|x| serde_json::from_str(&x).ok())
        .unwrap_or_default()
}

impl Repository {
    pub async fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            db: Builder::new_local(path.to_string_lossy().as_ref())
                .build()
                .await?,
        })
    }

    async fn connection(&self) -> Result<Connection> {
        let c = self.db.connect()?;
        c.execute("PRAGMA foreign_keys = ON", ()).await?;
        Ok(c)
    }

    pub async fn migrate(&self) -> Result<()> {
        migrations::migrate(&self.connection().await?).await
    }

    pub async fn health(&self) -> Result<()> {
        let c = self.connection().await?;
        let mut r = c.query("SELECT 1", ()).await?;
        r.next().await?;
        Ok(())
    }

    pub async fn existing_hash(
        &self,
        source: &str,
        source_id: &str,
    ) -> Result<Option<ExistingHashState>> {
        let c = self.connection().await?;
        let mut r = c
            .query(
                "SELECT id, raw_sha256, index_state
                 FROM documents
                 WHERE source = ?1
                   AND source_id = ?2",
                vec![s(source), s(source_id)],
            )
            .await?;
        Ok(match r.next().await? {
            Some(row) => Some(ExistingHashState {
                id: row.get(0)?,
                hash: row.get(1)?,
                state: state(&row.get::<String>(2)?),
            }),
            None => None,
        })
    }

    pub async fn upsert(&self, m: &NormalizedMessage) -> Result<UpsertOutcome> {
        let mut c = self.connection().await?;
        let tx =
            turso::transaction::Transaction::new(&mut c, TransactionBehavior::Immediate).await?;
        let now = Utc::now().to_rfc3339();
        let mut q = tx
            .query(
                "SELECT id, raw_sha256, index_state
                 FROM documents
                 WHERE source = ?1
                   AND source_id = ?2",
                vec![s(&m.source), s(&m.source_id)],
            )
            .await?;
        if let Some(row) = q.next().await? {
            let id: i64 = row.get(0)?;
            let old: String = row.get(1)?;
            let old_state: String = row.get(2)?;
            if old == m.raw_sha256 {
                tx.commit().await?;
                return Ok(UpsertOutcome {
                    document_id: id,
                    changed: false,
                    index_state: state(&old_state),
                });
            }
            exec_tx(
                &tx,
                "UPDATE documents
                 SET account_id = ?1,
                     mailbox_name = ?2,
                     imap_uid = ?3,
                     message_id = ?4,
                     in_reply_to = ?5,
                     references_json = ?6,
                     sent_at = ?7,
                     subject = ?8,
                     sender = ?9,
                     to_json = ?10,
                     cc_json = ?11,
                     bcc_json = ?12,
                     reply_to_json = ?13,
                     body_text = ?14,
                     body_truncated = ?15,
                     raw_sha256 = ?16,
                     producer_metadata_json = ?17,
                     index_state = 'pending',
                     index_error = NULL,
                     updated_at = ?18
                 WHERE id = ?19",
                vec![
                    opt(&m.account_id),
                    opt(&m.mailbox_name),
                    opt_i(m.imap_uid),
                    opt(&m.message_id),
                    opt(&m.in_reply_to),
                    json(&m.references),
                    opt(&m.sent_at),
                    opt(&m.subject),
                    opt(&m.sender),
                    json(&m.to),
                    json(&m.cc),
                    json(&m.bcc),
                    json(&m.reply_to),
                    s(&m.body),
                    i(m.body_truncated as i64),
                    s(&m.raw_sha256),
                    s(&m.producer_metadata_json),
                    s(&now),
                    i(id),
                ],
            )
            .await?;
            exec_tx(
                &tx,
                "DELETE FROM attachments WHERE document_id = ?1",
                vec![i(id)],
            )
            .await?;
            insert_attachments(&tx, id, &m.attachments).await?;
            tx.commit().await?;
            return Ok(UpsertOutcome {
                document_id: id,
                changed: true,
                index_state: IndexState::Pending,
            });
        }
        exec_tx(
            &tx,
            "INSERT INTO documents (
                source,
                source_id,
                account_id,
                mailbox_name,
                imap_uid,
                message_id,
                in_reply_to,
                references_json,
                sent_at,
                subject,
                sender,
                to_json,
                cc_json,
                bcc_json,
                reply_to_json,
                body_text,
                body_truncated,
                raw_sha256,
                index_state,
                created_at,
                updated_at,
                producer_metadata_json
            )
            VALUES (
                ?1,
                ?2,
                ?3,
                ?4,
                ?5,
                ?6,
                ?7,
                ?8,
                ?9,
                ?10,
                ?11,
                ?12,
                ?13,
                ?14,
                ?15,
                ?16,
                ?17,
                ?18,
                'pending',
                ?19,
                ?19,
                ?20
            )",
            vec![
                s(&m.source),
                s(&m.source_id),
                opt(&m.account_id),
                opt(&m.mailbox_name),
                opt_i(m.imap_uid),
                opt(&m.message_id),
                opt(&m.in_reply_to),
                json(&m.references),
                opt(&m.sent_at),
                opt(&m.subject),
                opt(&m.sender),
                json(&m.to),
                json(&m.cc),
                json(&m.bcc),
                json(&m.reply_to),
                s(&m.body),
                i(m.body_truncated as i64),
                s(&m.raw_sha256),
                s(&now),
                s(&m.producer_metadata_json),
            ],
        )
        .await?;
        let mut r = tx
            .query(
                "SELECT id FROM documents WHERE source = ?1 AND source_id = ?2",
                vec![s(&m.source), s(&m.source_id)],
            )
            .await?;
        let id: i64 = r
            .next()
            .await?
            .ok_or_else(|| anyhow!("insert did not produce id"))?
            .get(0)?;
        insert_attachments(&tx, id, &m.attachments).await?;
        tx.commit().await?;
        Ok(UpsertOutcome {
            document_id: id,
            changed: true,
            index_state: IndexState::Pending,
        })
    }

    pub async fn get_document(
        &self,
        source: &str,
        source_id: &str,
    ) -> Result<Option<StoredDocument>> {
        let c = self.connection().await?;
        let mut r = c
            .query(
                "SELECT id,
                        source,
                        source_id,
                        account_id,
                        mailbox_name,
                        imap_uid,
                        message_id,
                        in_reply_to,
                        references_json,
                        sent_at,
                        subject,
                        sender,
                        to_json,
                        cc_json,
                        bcc_json,
                        reply_to_json,
                        body_text,
                        body_truncated,
                        raw_sha256,
                        index_state,
                        index_error,
                        producer_metadata_json
                 FROM documents
                 WHERE source = ?1
                   AND source_id = ?2",
                vec![s(source), s(source_id)],
            )
            .await?;
        let Some(row) = r.next().await? else {
            return Ok(None);
        };
        let id: i64 = row.get(0)?;
        let mut a = c
            .query(
                "SELECT part_index,
                        filename,
                        media_type,
                        content_disposition,
                        content_id,
                        size_bytes,
                        sha256,
                        extraction_status,
                        extraction_error,
                        extracted_text,
                        text_truncated
                 FROM attachments
                 WHERE document_id = ?1
                 ORDER BY part_index",
                vec![i(id)],
            )
            .await?;
        let mut attachments = Vec::new();
        while let Some(x) = a.next().await? {
            attachments.push(StoredAttachment {
                part_index: x.get(0)?,
                filename: text(&x, 1),
                media_type: text(&x, 2),
                content_disposition: text(&x, 3),
                content_id: text(&x, 4),
                size_bytes: number(&x, 5),
                sha256: text(&x, 6),
                extraction_status: x.get(7)?,
                extraction_error: text(&x, 8),
                extracted_text: text(&x, 9),
                text_truncated: x.get::<i64>(10)? != 0,
            })
        }
        Ok(Some(decode(row, id, attachments)?))
    }

    /// Load only the metadata needed to render a search hit and a bounded,
    /// aggregate text projection for its snippet.
    pub async fn search_projection(
        &self,
        source: &str,
        source_id: &str,
        text_limit: usize,
    ) -> Result<Option<SearchProjection>> {
        let c = self.connection().await?;
        // Fetch attachment metadata first so a long body cannot consume the
        // entire bounded projection. When attachments exist, reserve half of
        // the projection for their extracted text; this keeps attachment-only
        // hits useful without loading canonical unbounded content.
        let mut r = c
            .query(
                "SELECT id, source, source_id, sent_at, subject, sender
                 FROM documents
                 WHERE source = ?1
                   AND source_id = ?2",
                vec![s(source), s(source_id)],
            )
            .await?;
        let Some(row) = r.next().await? else {
            return Ok(None);
        };
        let id: i64 = row.get(0)?;
        let mut a = c
            .query(
                "SELECT id, filename, media_type, size_bytes, extraction_status
                 FROM attachments
                 WHERE document_id = ?1
                 ORDER BY part_index",
                vec![i(id)],
            )
            .await?;
        let mut attachment_rows = Vec::new();
        while let Some(x) = a.next().await? {
            attachment_rows.push(x);
        }
        let body_limit = if attachment_rows.is_empty() {
            text_limit
        } else {
            text_limit / 2
        };
        let mut body_rows = c
            .query(
                "SELECT substr(body_text, 1, ?1) FROM documents WHERE id = ?2",
                vec![i(body_limit as i64), i(id)],
            )
            .await?;
        let body_text = body_rows
            .next()
            .await?
            .and_then(|body_row| text(&body_row, 0))
            .unwrap_or_default();
        let mut remaining = text_limit.saturating_sub(body_text.chars().count());
        let mut attachments = Vec::with_capacity(attachment_rows.len());
        for x in attachment_rows {
            let attachment_id: i64 = x.get(0)?;
            let extracted_text = if remaining > 0 {
                let mut text_rows = c
                    .query(
                        "SELECT substr(extracted_text, 1, ?1) FROM attachments WHERE id = ?2",
                        vec![i(remaining as i64), i(attachment_id)],
                    )
                    .await?;
                text_rows
                    .next()
                    .await?
                    .and_then(|text_row| text(&text_row, 0))
                    .inspect(|value| {
                        remaining = remaining.saturating_sub(value.chars().count());
                    })
            } else {
                None
            };
            attachments.push(SearchAttachmentProjection {
                filename: text(&x, 1),
                media_type: text(&x, 2),
                size_bytes: number(&x, 3),
                extraction_status: x.get(4)?,
                extracted_text,
            });
        }
        Ok(Some(SearchProjection {
            id,
            source: row.get(1)?,
            source_id: row.get(2)?,
            sent_at: text(&row, 3),
            subject: text(&row, 4),
            sender: text(&row, 5),
            body_text,
            attachments,
        }))
    }

    pub async fn documents_after(
        &self,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<StoredDocument>> {
        let c = self.connection().await?;
        let mut r = c
            .query(
                "SELECT id, source, source_id FROM documents WHERE id > ?1 ORDER BY id LIMIT ?2",
                vec![i(after_id), i(limit as i64)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(x) = r.next().await? {
            let id: i64 = x.get(0)?;
            let source: String = x.get(1)?;
            let source_id: String = x.get(2)?;
            if let Some(d) = self.get_document(&source, &source_id).await? {
                debug_assert_eq!(id, d.id);
                out.push(d);
            }
        }
        Ok(out)
    }

    pub async fn document_count(&self) -> Result<i64> {
        let c = self.connection().await?;
        let mut r = c.query("SELECT COUNT(*) FROM documents", ()).await?;
        Ok(r.next()
            .await?
            .ok_or_else(|| anyhow!("missing count"))?
            .get(0)?)
    }

    pub async fn all_documents(&self) -> Result<Vec<StoredDocument>> {
        let c = self.connection().await?;
        let mut r = c
            .query("SELECT source, source_id FROM documents ORDER BY id", ())
            .await?;
        let mut out = Vec::new();
        while let Some(x) = r.next().await? {
            let source: String = x.get(0)?;
            let id: String = x.get(1)?;
            if let Some(d) = self.get_document(&source, &id).await? {
                out.push(d)
            }
        }
        Ok(out)
    }

    pub async fn pending(&self, limit: usize) -> Result<Vec<PendingDocument>> {
        let c = self.connection().await?;
        let mut r = c
            .query(
                "SELECT source, source_id
                 FROM documents
                 WHERE index_state = 'pending'
                 ORDER BY id
                 LIMIT ?1",
                vec![i(limit as i64)],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(x) = r.next().await? {
            let source: String = x.get(0)?;
            let id: String = x.get(1)?;
            if let Some(d) = self.get_document(&source, &id).await? {
                out.push(d)
            }
        }
        Ok(out)
    }

    pub async fn mark_indexed(&self, id: i64, hash: &str) -> Result<()> {
        let c = self.connection().await?;
        c.execute(
            "UPDATE documents
             SET index_state = 'indexed',
                 index_error = NULL,
                 indexed_at = datetime('now'),
                 updated_at = datetime('now')
             WHERE id = ?1
               AND raw_sha256 = ?2
               AND index_state = 'pending'",
            vec![i(id), s(hash)],
        )
        .await?;
        Ok(())
    }

    pub async fn mark_error(&self, id: i64, hash: &str, error: &str) -> Result<()> {
        let c = self.connection().await?;
        c.execute(
            "UPDATE documents
             SET index_state = 'error',
                 index_error = ?3,
                 index_attempts = index_attempts + 1,
                 updated_at = datetime('now')
             WHERE id = ?1
               AND raw_sha256 = ?2
               AND index_state = 'pending'",
            vec![i(id), s(hash), s(error)],
        )
        .await?;
        Ok(())
    }

    pub async fn mark_indexed_any(&self, id: i64, hash: &str) -> Result<()> {
        let c = self.connection().await?;
        c.execute(
            "UPDATE documents
             SET index_state = 'indexed',
                 index_error = NULL,
                 indexed_at = datetime('now'),
                 updated_at = datetime('now')
             WHERE id = ?1
               AND raw_sha256 = ?2",
            vec![i(id), s(hash)],
        )
        .await?;
        Ok(())
    }

    /// Apply rebuild CAS updates in bounded transactions. The caller supplies
    /// only the versions from one manifest batch; newer uploads are left
    /// untouched because the hash is part of the predicate.
    pub async fn mark_indexed_versions(&self, versions: &[(i64, String)]) -> Result<()> {
        if versions.is_empty() {
            return Ok(());
        }
        let mut c = self.connection().await?;
        let tx =
            turso::transaction::Transaction::new(&mut c, TransactionBehavior::Immediate).await?;
        for (id, hash) in versions {
            exec_tx(
                &tx,
                "UPDATE documents
                 SET index_state = 'indexed',
                     index_error = NULL,
                     indexed_at = datetime('now'),
                     updated_at = datetime('now')
                 WHERE id = ?1
                   AND raw_sha256 = ?2",
                vec![i(*id), s(hash)],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn requeue(&self, source: &str, id: &str) -> Result<Option<(i64, IndexState)>> {
        let c = self.connection().await?;
        let n = c
            .execute(
                "UPDATE documents
                 SET index_state = 'pending',
                     index_error = NULL,
                     updated_at = datetime('now')
                 WHERE source = ?1
                   AND source_id = ?2",
                vec![s(source), s(id)],
            )
            .await?;
        if n == 0 {
            Ok(None)
        } else {
            let d = self
                .get_document(source, id)
                .await?
                .ok_or_else(|| anyhow!("document disappeared"))?;
            Ok(Some((d.id, IndexState::Pending)))
        }
    }

    pub async fn status_counts(&self) -> Result<IndexStatusCounts> {
        let c = self.connection().await?;
        let mut r = c
            .query(
                "SELECT index_state, COUNT(*) FROM documents GROUP BY index_state",
                (),
            )
            .await?;
        let mut out = IndexStatusCounts::default();
        while let Some(x) = r.next().await? {
            let st: String = x.get(0)?;
            let n: i64 = x.get(1)?;
            out.total += n;
            match st.as_str() {
                "pending" => out.pending = n,
                "indexed" => out.indexed = n,
                "error" => out.error = n,
                _ => {}
            }
        }
        Ok(out)
    }
}

async fn exec_tx(
    tx: &turso::transaction::Transaction<'_>,
    sql: &str,
    p: Vec<Value>,
) -> Result<u64> {
    Ok(tx.prepare(sql).await?.execute(p).await?)
}

async fn insert_attachments(
    tx: &turso::transaction::Transaction<'_>,
    id: i64,
    as_: &[NormalizedAttachment],
) -> Result<()> {
    for a in as_ {
        exec_tx(
            tx,
            "INSERT INTO attachments (
                document_id,
                part_index,
                filename,
                media_type,
                content_disposition,
                content_id,
                size_bytes,
                sha256,
                extraction_status,
                extraction_error,
                extracted_text,
                text_truncated
            )
            VALUES (
                ?1,
                ?2,
                ?3,
                ?4,
                ?5,
                ?6,
                ?7,
                ?8,
                ?9,
                ?10,
                ?11,
                ?12
            )",
            vec![
                i(id),
                i(a.part_index),
                opt(&a.filename),
                opt(&a.media_type),
                opt(&a.disposition),
                opt(&a.content_id),
                i(a.size_bytes),
                s(&a.sha256),
                s(&a.status),
                opt(&a.error),
                opt(&a.text),
                i(a.text_truncated as i64),
            ],
        )
        .await?;
    }
    Ok(())
}

fn decode(r: turso::Row, id: i64, a: Vec<StoredAttachment>) -> Result<StoredDocument> {
    let st: String = r.get(19)?;
    Ok(StoredDocument {
        id,
        source: r.get(1)?,
        source_id: r.get(2)?,
        producer_metadata_json: text(&r, 21).unwrap_or_else(|| "{}".into()),
        account_id: text(&r, 3),
        mailbox_name: text(&r, 4),
        imap_uid: number(&r, 5),
        message_id: text(&r, 6),
        in_reply_to: text(&r, 7),
        references: array(&r, 8),
        sent_at: text(&r, 9),
        subject: text(&r, 10),
        sender: text(&r, 11),
        to: array(&r, 12),
        cc: array(&r, 13),
        bcc: array(&r, 14),
        reply_to: array(&r, 15),
        body_text: r.get(16)?,
        body_truncated: r.get::<i64>(17)? != 0,
        raw_sha256: r.get(18)?,
        index_state: state(&st),
        index_error: text(&r, 20),
        attachments: a,
    })
}
