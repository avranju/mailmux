use tantivy::schema::{FAST, Field, INDEXED, STORED, STRING, Schema, TEXT};

#[derive(Clone, Copy)]
pub struct IndexFields {
    pub document_key: Field,
    pub source: Field,
    pub account_id: Field,
    pub mailbox_name: Field,
    pub sent_timestamp: Field,
    pub sender_exact: Field,
    pub sender_text: Field,
    pub recipients_text: Field,
    pub subject: Field,
    pub body: Field,
    pub attachment_text: Field,
}

pub fn build() -> (Schema, IndexFields) {
    let mut b = Schema::builder();
    let key = b.add_text_field("document_key", STRING | STORED);
    let source = b.add_text_field("source", STRING);
    let account = b.add_text_field("account_id", STRING);
    let mailbox = b.add_text_field("mailbox_name", STRING);
    let sent = b.add_i64_field("sent_timestamp", INDEXED | FAST);
    let se = b.add_text_field("sender_exact", STRING);
    let st = b.add_text_field("sender_text", TEXT);
    let rec = b.add_text_field("recipients_text", TEXT);
    let sub = b.add_text_field("subject", TEXT);
    let body = b.add_text_field("body", TEXT);
    let att = b.add_text_field("attachment_text", TEXT);
    (
        b.build(),
        IndexFields {
            document_key: key,
            source,
            account_id: account,
            mailbox_name: mailbox,
            sent_timestamp: sent,
            sender_exact: se,
            sender_text: st,
            recipients_text: rec,
            subject: sub,
            body,
            attachment_text: att,
        },
    )
}

pub fn validate(schema: &Schema) -> anyhow::Result<()> {
    let (expected, _) = build();
    let actual: Vec<_> = schema.fields().collect();
    let wanted: Vec<_> = expected.fields().collect();
    if actual.len() != wanted.len()
        || wanted.iter().any(|(_, entry)| {
            schema
                .get_field(entry.name())
                .ok()
                .map(|field| schema.get_field_entry(field) != *entry)
                .unwrap_or(true)
        })
    {
        anyhow::bail!("incompatible Tantivy schema; run rebuild-index")
    }
    Ok(())
}

pub fn fields(schema: &Schema) -> anyhow::Result<IndexFields> {
    let f = |n| {
        schema
            .get_field(n)
            .map_err(|e| anyhow::anyhow!("missing field {n}: {e}"))
    };
    Ok(IndexFields {
        document_key: f("document_key")?,
        source: f("source")?,
        account_id: f("account_id")?,
        mailbox_name: f("mailbox_name")?,
        sent_timestamp: f("sent_timestamp")?,
        sender_exact: f("sender_exact")?,
        sender_text: f("sender_text")?,
        recipients_text: f("recipients_text")?,
        subject: f("subject")?,
        body: f("body")?,
        attachment_text: f("attachment_text")?,
    })
}
