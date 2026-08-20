use crate::{error::AppError, http::AppState, search::SearchError};
use axum::{
    extract::{Path, State},
    response::Html,
};

pub async fn view(
    State(st): State<AppState>,
    Path((source, id)): Path<(String, String)>,
) -> Result<Html<String>, AppError> {
    let d = st
        .search
        .get(&source, &id, Some(st.config.search.max_get_chars))
        .await
        .map_err(|error| match error {
            SearchError::Invalid(message) => AppError::Invalid(message),
            SearchError::NotFound => AppError::NotFound,
            SearchError::Internal(error) => AppError::Internal(error),
        })?;
    let esc = |s: &str| html_escape::encode_text(s).to_string();
    let subject = d.subject.as_deref().unwrap_or("(no subject)");
    let mut out = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{}</title><main><h1>{}</h1>",
        esc(subject),
        esc(subject)
    );
    out.push_str("<dl>");
    let row = |out: &mut String, label: &str, value: &str| {
        out.push_str(&format!("<dt>{}</dt><dd>{}</dd>", label, esc(value)));
    };
    row(&mut out, "Source", &d.source);
    row(&mut out, "Source ID", &d.source_id);
    row(&mut out, "Account", d.account_id.as_deref().unwrap_or(""));
    row(&mut out, "Mailbox", d.mailbox_name.as_deref().unwrap_or(""));
    row(
        &mut out,
        "IMAP UID",
        &d.imap_uid
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    row(&mut out, "Date", d.sent_at.as_deref().unwrap_or(""));
    row(&mut out, "From", d.sender.as_deref().unwrap_or(""));
    row(&mut out, "To", &d.to.join(", "));
    row(&mut out, "Cc", &d.cc.join(", "));
    row(&mut out, "Bcc", &d.bcc.join(", "));
    row(&mut out, "Reply-To", &d.reply_to.join(", "));
    row(&mut out, "Subject", subject);
    row(
        &mut out,
        "Message-ID",
        d.message_id.as_deref().unwrap_or(""),
    );
    row(
        &mut out,
        "In-Reply-To",
        d.in_reply_to.as_deref().unwrap_or(""),
    );
    row(&mut out, "References", &d.references.join(" "));
    row(
        &mut out,
        "Producer metadata",
        &d.producer_metadata.to_string(),
    );
    row(&mut out, "Index state", &format!("{:?}", d.index_state));
    out.push_str("</dl><h2>Body</h2><pre>");
    out.push_str(&esc(&d.body));
    if d.body_truncated {
        out.push_str("</pre><p>Body was truncated during normalization.</p>");
    } else {
        out.push_str("</pre>");
    }
    out.push_str("<h2>Attachments</h2><ul>");
    for a in d.attachments {
        let size = a
            .size_bytes
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".into());
        out.push_str("<li><dl>");
        row(
            &mut out,
            "Filename",
            a.filename.as_deref().unwrap_or("unnamed"),
        );
        row(
            &mut out,
            "Media type",
            a.media_type.as_deref().unwrap_or("unknown"),
        );
        row(&mut out, "Size", &size);
        row(
            &mut out,
            "Disposition",
            a.content_disposition.as_deref().unwrap_or(""),
        );
        row(
            &mut out,
            "Content-ID",
            a.content_id.as_deref().unwrap_or(""),
        );
        row(&mut out, "Extraction", &a.extraction_status);
        if let Some(error) = a.extraction_error.as_deref() {
            row(&mut out, "Extraction error", error);
        }
        out.push_str("</dl>");
        if let Some(t) = a.extracted_text {
            out.push_str(&format!(
                "<details><summary>Extracted text</summary><pre>{}</pre></details>",
                esc(&t)
            ));
        }
        out.push_str("</li>");
    }
    out.push_str("</ul></main>");
    Ok(Html(out))
}
