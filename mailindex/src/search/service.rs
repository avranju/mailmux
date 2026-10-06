use crate::{
    config::Config,
    index::{SearchIndex, decode_key},
    models::*,
    search::query,
    storage::{Repository, models::SearchProjection},
};
use anyhow::anyhow;
use std::sync::Arc;
use tantivy::{collector::TopDocs, schema::Value};

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("not found")]
    NotFound,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Clone)]
pub struct SearchService {
    pub repo: Arc<Repository>,
    pub index: Arc<SearchIndex>,
    pub config: Arc<Config>,
}

impl SearchService {
    pub async fn search(
        &self,
        req: SearchRequest,
    ) -> std::result::Result<SearchResponse, SearchError> {
        let mut req = req;
        if req.limit.is_none() {
            req.limit = Some(self.config.search.default_limit);
        }
        let (q, limit) = query::build(&req, &self.index, self.config.search.max_limit)
            .map_err(|error| SearchError::Invalid(error.to_string()))?;
        let searcher = self.index.reader.searcher();
        let hits = searcher
            .search(&*q, &TopDocs::with_limit(limit).order_by_score())
            .map_err(|error| SearchError::Internal(anyhow!(error)))?;
        let mut results = Vec::new();
        for (score, addr) in hits {
            let d = searcher
                .doc::<tantivy::schema::TantivyDocument>(addr)
                .map_err(|error| SearchError::Internal(anyhow!(error)))?;
            let Some(v) = d
                .get_first(self.index.fields.document_key)
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            let Some((s, id)) = decode_key(v) else {
                continue;
            };
            // Search hits are hydrated from a bounded projection.  Full
            // canonical content is reserved for `mail_get`/the view endpoint.
            if let Some(doc) = self.repo.search_projection(&s, &id, 600).await? {
                results.push(self.result(doc, score, &req.query));
            }
        }
        Ok(SearchResponse { results })
    }

    fn result(&self, d: SearchProjection, score: f32, q: &str) -> SearchResult {
        let snippet = snippet(&d, q);
        SearchResult {
            source: d.source.clone(),
            source_id: d.source_id.clone(),
            score,
            sent_at: d.sent_at.clone(),
            sender: d.sender.clone(),
            subject: d.subject.clone(),
            snippet,
            attachments: d
                .attachments
                .iter()
                .map(|a| AttachmentSummary {
                    filename: a.filename.clone(),
                    media_type: a.media_type.clone(),
                    size_bytes: a.size_bytes,
                    extraction_status: a.extraction_status.clone(),
                })
                .collect(),
            view_url: self.config.view_url(&d.source, &d.source_id),
        }
    }

    pub async fn get(
        &self,
        s: &str,
        id: &str,
        max: Option<usize>,
    ) -> std::result::Result<DocumentResponse, SearchError> {
        crate::ingest::validate_identity(s)
            .map_err(|error| SearchError::Invalid(error.to_string()))?;
        crate::ingest::validate_identity(id)
            .map_err(|error| SearchError::Invalid(error.to_string()))?;
        if max == Some(0) {
            return Err(SearchError::Invalid("max_chars must be at least 1".into()));
        }
        let d = self
            .repo
            .get_document(s, id)
            .await?
            .ok_or(SearchError::NotFound)?;
        let budget = max
            .unwrap_or(self.config.search.max_get_chars)
            .min(self.config.search.max_get_chars);
        let (body, body_used, body_response_truncated) = take(&d.body_text, budget);
        let mut used = body_used;
        let mut response_truncated = body_response_truncated;
        let mut attachments = Vec::new();
        for a in d.attachments {
            let text = a.extracted_text.as_ref().map(|x| {
                let (y, n, response_was_truncated) = take(x, budget.saturating_sub(used));
                used += n;
                response_truncated |= response_was_truncated;
                (y, response_was_truncated)
            });
            attachments.push(Attachment {
                part_index: a.part_index,
                filename: a.filename,
                media_type: a.media_type,
                content_disposition: a.content_disposition,
                content_id: a.content_id,
                size_bytes: a.size_bytes,
                sha256: a.sha256,
                extraction_status: a.extraction_status,
                extraction_error: a.extraction_error,
                extracted_text: text.as_ref().map(|x| x.0.clone()),
                text_truncated: a.text_truncated,
                response_truncated: text.as_ref().is_some_and(|x| x.1),
            });
        }
        Ok(DocumentResponse {
            source: d.source.clone(),
            source_id: d.source_id.clone(),
            document_id: d.id,
            producer_metadata: serde_json::from_str(&d.producer_metadata_json)
                .unwrap_or(serde_json::Value::Null),
            account_id: d.account_id,
            mailbox_name: d.mailbox_name,
            imap_uid: d.imap_uid,
            message_id: d.message_id,
            in_reply_to: d.in_reply_to,
            references: d.references,
            sent_at: d.sent_at,
            subject: d.subject,
            sender: d.sender,
            to: d.to,
            cc: d.cc,
            bcc: d.bcc,
            reply_to: d.reply_to,
            body,
            body_truncated: d.body_truncated,
            body_response_truncated,
            attachments,
            response_truncated,
            index_state: d.index_state,
            index_error: d.index_error,
            view_url: self.config.view_url(&d.source, &d.source_id),
        })
    }
}

fn take(s: &str, n: usize) -> (String, usize, bool) {
    let x: String = s.chars().take(n).collect();
    let t = x.chars().count() < s.chars().count();
    let used = x.chars().count();
    (x, used, t)
}

fn snippet(d: &SearchProjection, q: &str) -> String {
    let mut sources = Vec::with_capacity(1 + d.attachments.len());
    sources.push(d.body_text.as_str());
    sources.extend(
        d.attachments
            .iter()
            .filter_map(|attachment| attachment.extracted_text.as_deref()),
    );

    for source in &sources {
        let lower = source.to_ascii_lowercase();
        let position = q
            .split_whitespace()
            .map(|term| term.to_ascii_lowercase())
            .filter(|term| !term.is_empty())
            .filter_map(|term| lower.find(&term))
            .min();
        if let Some(byte_position) = position {
            let char_position = source[..byte_position].chars().count();
            let start = char_position.saturating_sub(100);
            return source.chars().skip(start).take(300).collect();
        }
    }

    sources
        .into_iter()
        .find(|source| !source.is_empty())
        .map(|source| source.chars().take(300).collect())
        .unwrap_or_default()
}
