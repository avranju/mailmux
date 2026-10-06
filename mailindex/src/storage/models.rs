use crate::models::IndexState;

#[derive(Clone, Debug)]
pub struct StoredAttachment {
    pub part_index: i64,
    pub filename: Option<String>,
    pub media_type: Option<String>,
    pub content_disposition: Option<String>,
    pub content_id: Option<String>,
    pub size_bytes: Option<i64>,
    pub sha256: Option<String>,
    pub extraction_status: String,
    pub extraction_error: Option<String>,
    pub extracted_text: Option<String>,
    pub text_truncated: bool,
}

#[derive(Clone, Debug)]
pub struct StoredDocument {
    pub id: i64,
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
    pub body_text: String,
    pub body_truncated: bool,
    pub raw_sha256: String,
    pub index_state: IndexState,
    pub index_error: Option<String>,
    pub attachments: Vec<StoredAttachment>,
}

pub type PendingDocument = StoredDocument;

/// The bounded projection used to hydrate search hits.  Unlike
/// `StoredDocument`, this deliberately contains no complete body or
/// attachment text.
#[derive(Clone, Debug)]
pub struct SearchProjection {
    pub id: i64,
    pub source: String,
    pub source_id: String,
    pub sent_at: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub body_text: String,
    pub attachments: Vec<SearchAttachmentProjection>,
}

#[derive(Clone, Debug)]
pub struct SearchAttachmentProjection {
    pub filename: Option<String>,
    pub media_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub extraction_status: String,
    pub extracted_text: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ExistingHashState {
    pub id: i64,
    pub hash: String,
    pub state: IndexState,
}

#[derive(Clone, Debug)]
pub struct UpsertOutcome {
    pub document_id: i64,
    pub changed: bool,
    pub index_state: IndexState,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct IndexStatusCounts {
    pub total: i64,
    pub pending: i64,
    pub indexed: i64,
    pub error: i64,
}
