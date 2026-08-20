mod common;
use common::fixture;
use mailindex::{
    config::ContentConfig,
    ingest::{normalize_message, parser},
};

fn limits() -> ContentConfig {
    ContentConfig {
        max_body_chars: 1000,
        max_attachment_bytes: 1000,
        max_attachment_text_chars: 1000,
        pdf_enabled: true,
    }
}

#[test]
fn fixtures_parse_authoritative_metadata_and_safe_html_fallback() {
    let p = parser::parse(fixture("plain.eml"), &limits()).unwrap();
    assert_eq!(p.message_id.as_deref(), Some("plain@example.com"));
    assert!(p.body.contains("Distinctive plain"));
    assert_eq!(p.to, vec!["Bob <bob@example.com>"]);
    let html = parser::parse(fixture("html-only.eml"), &limits()).unwrap();
    assert!(html.body.contains("HTML marker"));
    assert!(!html.body.contains("<script>"));
    assert!(!html.body.contains("alert('bad')"));
    let fallback = parser::parse(fixture("blank-plain-html.eml"), &limits()).unwrap();
    assert!(fallback.body.contains("Useful HTML fallback marker"));
}

#[test]
fn multipart_alternative_selects_plain_and_skips_html() {
    let p = parser::parse(fixture("multipart-alternative.eml"), &limits()).unwrap();
    // The plain alternative is the canonical body: the HTML-only marker is
    // absent, no markup survives, and the shared marker is not duplicated.
    assert_eq!(p.body, "Plain marker only.");
    assert!(!p.body.contains("Html marker"));
    assert!(!p.body.contains('<'));
    assert_eq!(p.body.matches("Plain marker only.").count(), 1);
}

#[test]
fn duplicate_rfc_message_ids_parse_as_distinct_messages() {
    let a = parser::parse(fixture("duplicate-message-id-a.eml"), &limits()).unwrap();
    let b = parser::parse(fixture("duplicate-message-id-b.eml"), &limits()).unwrap();
    assert_eq!(a.message_id, b.message_id);
    assert!(
        a.message_id
            .as_deref()
            .is_some_and(|id| id.contains("same@example.com"))
    );
    assert_eq!(a.subject.as_deref(), Some("One"));
    assert_eq!(b.subject.as_deref(), Some("Two"));
    assert_ne!(a.body, b.body);
    // Canonical identity is the (source, source_id) pair, so duplicate RFC
    // Message-IDs normalize into distinct documents with distinct hashes.
    let a = normalize_message(
        "dup".into(),
        "a".into(),
        serde_json::json!({}),
        fixture("duplicate-message-id-a.eml"),
        &limits(),
    )
    .unwrap();
    let b = normalize_message(
        "dup".into(),
        "b".into(),
        serde_json::json!({}),
        fixture("duplicate-message-id-b.eml"),
        &limits(),
    )
    .unwrap();
    assert_eq!(a.message_id, b.message_id);
    assert_ne!(a.raw_sha256, b.raw_sha256);
    assert_ne!(a.body, b.body);
}

#[test]
fn missing_message_id_and_date_are_supported() {
    let p = parser::parse(fixture("missing-headers.eml"), &limits()).unwrap();
    assert!(p.message_id.is_none());
    assert!(p.sent_at.is_none());
    assert_eq!(p.subject.as_deref(), Some("Minimal"));
    assert!(p.body.contains("Minimal body."));
}

#[test]
fn attachments_keep_local_errors_and_extract_text() {
    let text = parser::parse(fixture("text-attachment.eml"), &limits()).unwrap();
    assert!(text.attachments.iter().any(|a| {
        a.status == "extracted"
            && a.text
                .as_deref()
                .unwrap_or("")
                .contains("Unique attachment")
    }));
    let pdf = parser::parse(fixture("pdf-attachment.eml"), &limits()).unwrap();
    assert!(pdf.attachments.iter().any(|a| {
        a.status == "extracted"
            && a.text
                .as_deref()
                .unwrap_or("")
                .contains("PDF unique marker")
    }));
    let binary = parser::parse(fixture("unsupported-binary.eml"), &limits()).unwrap();
    assert!(binary.attachments.iter().any(|a| a.status == "unsupported"));
    let broken = parser::parse(fixture("malformed-pdf.eml"), &limits()).unwrap();
    assert!(broken.attachments.iter().any(|a| a.status == "error"));
    let mut disabled = limits();
    disabled.pdf_enabled = false;
    let disabled_pdf = parser::parse(fixture("malformed-pdf.eml"), &disabled).unwrap();
    assert!(
        disabled_pdf
            .attachments
            .iter()
            .any(|a| a.status == "disabled")
    );
}

#[test]
fn normalization_is_unicode_safe_and_retains_metadata() {
    let m = normalize_message(
        "mailbox/name".into(),
        "id".into(),
        serde_json::json!({}),
        fixture("unicode.eml"),
        &limits(),
    );
    assert!(m.is_err());
    let m = normalize_message(
        "mailbox".into(),
        "id".into(),
        serde_json::json!({}),
        fixture("unicode.eml"),
        &limits(),
    )
    .unwrap();
    assert!(m.body.contains("Привет"));
    assert!(m.subject.unwrap_or_default().contains("日本"));

    let mut short = limits();
    short.max_body_chars = 10;
    let truncated = normalize_message(
        "mailbox".into(),
        "short".into(),
        serde_json::json!({}),
        fixture("plain.eml"),
        &short,
    )
    .unwrap();
    assert!(truncated.body_truncated);
    assert!(truncated.body.chars().count() <= 10);
}
