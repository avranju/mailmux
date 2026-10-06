use anyhow::{Result, bail};
use turso::Connection;
use turso::transaction::TransactionBehavior;

pub const CURRENT_VERSION: i64 = 1;

/// Apply ordered, transactional schema versions. The database intentionally has
/// no implicit migration magic: a newer schema is refused rather than being
/// opened with a partially understood model.
pub async fn migrate(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
        (),
    )
    .await?;
    let mut rows = conn
        .query(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            (),
        )
        .await?;
    let version: i64 = rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing migration version"))?
        .get(0)?;
    if version > CURRENT_VERSION {
        bail!("database schema version {version} is newer than this binary (max {CURRENT_VERSION})")
    }
    if version == CURRENT_VERSION {
        return Ok(());
    }

    let tx = turso::transaction::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .await?;
    for statement in MIGRATION_1 {
        tx.prepare(statement).await?.execute(()).await?;
    }
    tx.prepare("INSERT INTO schema_migrations(version, applied_at) VALUES (1, datetime('now'))")
        .await?
        .execute(())
        .await?;
    tx.commit().await?;
    Ok(())
}

const MIGRATION_1: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS documents(id INTEGER PRIMARY KEY,source TEXT NOT NULL,source_id TEXT NOT NULL,account_id TEXT,mailbox_name TEXT,imap_uid INTEGER,message_id TEXT,in_reply_to TEXT,references_json TEXT,sent_at TEXT,subject TEXT,sender TEXT,to_json TEXT,cc_json TEXT,bcc_json TEXT,reply_to_json TEXT,body_text TEXT NOT NULL DEFAULT '',body_truncated INTEGER NOT NULL DEFAULT 0,raw_sha256 TEXT NOT NULL,index_state TEXT NOT NULL DEFAULT 'pending',index_error TEXT,index_attempts INTEGER NOT NULL DEFAULT 0,indexed_at TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL,producer_metadata_json TEXT NOT NULL DEFAULT '{}',UNIQUE(source,source_id))",
    "CREATE INDEX IF NOT EXISTS idx_documents_sent_at ON documents(sent_at)",
    "CREATE INDEX IF NOT EXISTS idx_documents_account ON documents(account_id)",
    "CREATE INDEX IF NOT EXISTS idx_documents_mailbox ON documents(mailbox_name)",
    "CREATE TABLE IF NOT EXISTS attachments(id INTEGER PRIMARY KEY,document_id INTEGER NOT NULL,part_index INTEGER NOT NULL,filename TEXT,media_type TEXT,content_disposition TEXT,content_id TEXT,size_bytes INTEGER,sha256 TEXT,extraction_status TEXT NOT NULL,extraction_error TEXT,extracted_text TEXT,text_truncated INTEGER NOT NULL DEFAULT 0,FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE,UNIQUE(document_id,part_index))",
    "CREATE INDEX IF NOT EXISTS idx_attachments_document ON attachments(document_id)",
];
