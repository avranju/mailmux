use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IndexState {
    Pending,
    Indexed,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct IngestResponse {
    pub source: String,
    pub source_id: String,
    pub document_id: i64,
    pub changed: bool,
    pub index_state: IndexState,
    pub view_url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct SearchRequest {
    pub query: String,
    pub after: Option<String>,
    pub before: Option<String>,
    #[serde(default)]
    pub account_ids: Vec<String>,
    #[serde(default)]
    pub mailboxes: Vec<String>,
    #[serde(default)]
    pub senders: Vec<String>,
    #[schemars(range(min = 1, max = 50))]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchResult {
    pub source: String,
    pub source_id: String,
    pub score: f32,
    pub sent_at: Option<String>,
    pub sender: Option<String>,
    pub subject: Option<String>,
    pub snippet: String,
    pub attachments: Vec<AttachmentSummary>,
    pub view_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AttachmentSummary {
    pub filename: Option<String>,
    pub media_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub extraction_status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct DocumentResponse {
    pub source: String,
    pub source_id: String,
    pub document_id: i64,
    pub producer_metadata: serde_json::Value,
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
    /// Truncation performed while creating canonical storage.
    pub body_truncated: bool,
    /// Truncation performed while applying this retrieval's max_chars budget.
    pub body_response_truncated: bool,
    pub attachments: Vec<Attachment>,
    /// True only when content was omitted by the aggregate response budget.
    pub response_truncated: bool,
    pub index_state: IndexState,
    pub index_error: Option<String>,
    pub view_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct Attachment {
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
    /// Truncation performed while extracting and storing canonical text.
    pub text_truncated: bool,
    /// Truncation performed while applying this retrieval's max_chars budget.
    pub response_truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReindexResponse {
    pub source: String,
    pub source_id: String,
    pub document_id: i64,
    pub index_state: IndexState,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct IndexStatus {
    pub total: i64,
    pub pending: i64,
    pub indexed: i64,
    pub error: i64,
}
