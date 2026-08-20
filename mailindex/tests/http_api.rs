mod common;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{config, fixture, repository};
use mailindex::{
    http::{AppState, router},
    index::{SearchIndex, worker::IndexWorker},
    ingest::{NormalizedAttachment, NormalizedMessage, normalize_message},
    search::SearchService,
};
use std::sync::{Arc, atomic::AtomicBool};
use tokio::{
    sync::Notify,
    time::{Duration, sleep},
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

async fn test_app(dir: &tempfile::TempDir, cfg: mailindex::config::Config) -> Router {
    let repo = repository(dir).await;
    let (idx, _writer) = SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let config = Arc::new(cfg);
    let search = Arc::new(SearchService {
        repo: repo.clone(),
        index: Arc::new(idx),
        config: config.clone(),
    });
    router(AppState {
        repo,
        search,
        config,
        notify: Arc::new(Notify::new()),
        ready: Arc::new(AtomicBool::new(true)),
        token: None,
        cancel: CancellationToken::new(),
    })
}

async fn assert_json_error(response: axum::response::Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()["content-type"], "application/json");
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], code);
}

#[tokio::test]
async fn multipart_ingestion_is_durable_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    let (idx, _writer) = SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let state = AppState {
        repo: repo.clone(),
        search: Arc::new(SearchService {
            repo,
            index: Arc::new(idx),
            config: Arc::new(cfg.clone()),
        }),
        config: Arc::new(cfg),
        notify: Arc::new(Notify::new()),
        ready: Arc::new(AtomicBool::new(true)),
        token: None,
        cancel: CancellationToken::new(),
    };
    let app = router(state);
    let message = fixture("plain.eml");
    let boundary = "mailindex-test";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"metadata\"\r\n\r\n{{}}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"message\"\r\nContent-Type: message/rfc822\r\n\r\n{}\r\n--{boundary}--\r\n",
        String::from_utf8_lossy(message)
    );
    let request = || {
        Request::put("/v1/documents/mailmux/42")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body.clone()))
            .unwrap()
    };
    let first = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let second = app.oneshot(request()).await.unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(second.into_body(), 1_000_000)
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result["changed"], false);
}

#[tokio::test]
async fn non_multipart_upload_is_a_json_415() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    let (idx, _writer) = SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let state = AppState {
        repo: repo.clone(),
        search: Arc::new(SearchService {
            repo,
            index: Arc::new(idx),
            config: Arc::new(cfg.clone()),
        }),
        config: Arc::new(cfg),
        notify: Arc::new(Notify::new()),
        ready: Arc::new(AtomicBool::new(true)),
        token: None,
        cancel: CancellationToken::new(),
    };
    let response = router(state)
        .oneshot(
            Request::put("/v1/documents/mailmux/42")
                .header("content-type", "application/octet-stream")
                .body(Body::from("not multipart"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(response.headers()["content-type"], "application/json");
}

#[tokio::test]
async fn retrieval_reports_persistent_and_response_truncation_separately() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    let message = |source_id: &str, body: &str, text: Option<&str>| NormalizedMessage {
        source: "s".into(),
        source_id: source_id.into(),
        producer_metadata_json: "{}".into(),
        account_id: None,
        mailbox_name: None,
        imap_uid: None,
        message_id: None,
        in_reply_to: None,
        references: vec![],
        sent_at: None,
        subject: None,
        sender: None,
        to: vec![],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        body: body.into(),
        body_truncated: false,
        raw_sha256: source_id.into(),
        attachments: vec![NormalizedAttachment {
            part_index: 1,
            filename: Some("note.txt".into()),
            media_type: Some("text/plain".into()),
            disposition: Some("attachment".into()),
            content_id: None,
            size_bytes: 3,
            sha256: source_id.into(),
            status: if text.is_some() {
                "extracted"
            } else {
                "unsupported"
            }
            .into(),
            error: None,
            text: text.map(str::to_owned),
            text_truncated: text.is_some_and(|value| value.len() > 3),
        }],
    };
    repo.upsert(&message("exact", "12", Some("abc")))
        .await
        .unwrap();
    repo.upsert(&message("body-long", "123456", Some("abc")))
        .await
        .unwrap();
    repo.upsert(&message("metadata-only", "12345", None))
        .await
        .unwrap();
    let (idx, _writer) = SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let search = SearchService {
        repo,
        index: Arc::new(idx),
        config: Arc::new(cfg),
    };

    let exact = search.get("s", "exact", Some(5)).await.unwrap();
    assert!(!exact.body_response_truncated);
    assert!(!exact.response_truncated);
    assert!(!exact.attachments[0].response_truncated);

    let body_long = search.get("s", "body-long", Some(5)).await.unwrap();
    assert!(body_long.body_response_truncated);
    assert!(body_long.response_truncated);
    assert!(body_long.attachments[0].response_truncated);

    let metadata_only = search.get("s", "metadata-only", Some(5)).await.unwrap();
    assert!(!metadata_only.response_truncated);
    assert!(!metadata_only.attachments[0].response_truncated);
}

#[tokio::test]
async fn health_is_public_and_api_auth_is_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    let normalized = normalize_message(
        "s".into(),
        "id".into(),
        serde_json::json!({}),
        fixture("html-only.eml"),
        &cfg.content,
    )
    .unwrap();
    let mut normalized = normalized;
    normalized.account_id = Some("<script>account</script>".into());
    normalized.mailbox_name = Some("mailbox & <tag>".into());
    normalized.cc = vec!["cc@example.com <script>".into()];
    normalized.attachments.push(NormalizedAttachment {
        part_index: 99,
        filename: Some("</li><script>name".into()),
        media_type: Some("text/plain".into()),
        disposition: Some("attachment".into()),
        content_id: Some("<cid>".into()),
        size_bytes: 123,
        sha256: "hash".into(),
        status: "extracted".into(),
        error: None,
        text: Some("attachment <script>text</script>".into()),
        text_truncated: false,
    });
    repo.upsert(&normalized).await.unwrap();
    let (idx, _writer) = SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let state = AppState {
        repo: repo.clone(),
        search: Arc::new(SearchService {
            repo,
            index: Arc::new(idx),
            config: Arc::new(cfg.clone()),
        }),
        config: Arc::new(config(&dir)),
        notify: Arc::new(Notify::new()),
        ready: Arc::new(AtomicBool::new(true)),
        token: Some("secret".into()),
        cancel: CancellationToken::new(),
    };
    let app = router(state);
    let health = app
        .clone()
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let denied = app
        .clone()
        .oneshot(
            Request::post("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let allowed = app
        .clone()
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(allowed.status(), StatusCode::UNAUTHORIZED);
    let invalid_query = app
        .clone()
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"\""}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_query.status(), StatusCode::BAD_REQUEST);
    assert_eq!(invalid_query.headers()["content-type"], "application/json");
    let invalid_query_body = axum::body::to_bytes(invalid_query.into_body(), 1_000_000)
        .await
        .unwrap();
    let invalid_query_json: serde_json::Value =
        serde_json::from_slice(&invalid_query_body).unwrap();
    assert_eq!(invalid_query_json["error"]["code"], "invalid_request");

    let malformed_json = app
        .clone()
        .oneshot(
            Request::post("/v1/search")
                .header("authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"unterminated}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(malformed_json.status(), StatusCode::BAD_REQUEST);
    assert_eq!(malformed_json.headers()["content-type"], "application/json");

    let view = app
        .oneshot(
            Request::get("/view/s/id")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(view.status(), StatusCode::OK);
    let body = axum::body::to_bytes(view.into_body(), 1_000_000)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("Source ID"));
    assert!(html.contains("&lt;script&gt;account&lt;/script&gt;"));
    assert!(html.contains("cc@example.com"));
    assert!(html.contains("123"));
}

#[tokio::test]
async fn extractor_query_and_body_limit_rejections_use_json_envelopes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&dir);
    cfg.server.max_request_bytes = 32;
    cfg.validate().unwrap();
    let app = test_app(&dir, cfg).await;

    let malformed_multipart = app
        .clone()
        .oneshot(
            Request::put("/v1/documents/s/id")
                .header("content-type", "multipart/form-data")
                .body(Body::from("not multipart"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_json_error(
        malformed_multipart,
        StatusCode::BAD_REQUEST,
        "invalid_request",
    )
    .await;

    let invalid_query = app
        .clone()
        .oneshot(
            Request::get("/v1/documents/s/id?max_chars=nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_json_error(invalid_query, StatusCode::BAD_REQUEST, "invalid_request").await;

    let oversized = app
        .oneshot(
            Request::post("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"this request is too large"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_json_error(
        oversized,
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
    )
    .await;
}

#[tokio::test]
async fn search_and_reindex_routes_use_shared_services() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    let normalized = normalize_message(
        "s".into(),
        "searchable".into(),
        serde_json::json!({}),
        fixture("plain.eml"),
        &cfg.content,
    )
    .unwrap();
    repo.upsert(&normalized).await.unwrap();
    let (idx, writer) = SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let idx = Arc::new(idx);
    let notify = Arc::new(Notify::new());
    let cancel = CancellationToken::new();
    let worker = IndexWorker::new(
        repo.clone(),
        idx.clone(),
        writer,
        notify.clone(),
        10,
        10,
        cancel.clone(),
    );
    let handle = tokio::spawn(worker.run());
    let config = Arc::new(cfg);
    let app = router(AppState {
        repo: repo.clone(),
        search: Arc::new(SearchService {
            repo: repo.clone(),
            index: idx,
            config: config.clone(),
        }),
        config,
        notify,
        ready: Arc::new(AtomicBool::new(true)),
        token: None,
        cancel: cancel.clone(),
    });
    sleep(Duration::from_millis(100)).await;

    let response = app
        .clone()
        .oneshot(
            Request::post("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"Distinctive"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(result["results"][0]["source_id"], "searchable");

    let reindex = app
        .oneshot(
            Request::post("/v1/documents/s/searchable/reindex")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reindex.status(), StatusCode::ACCEPTED);
    cancel.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn changed_upload_replaces_canonical_document() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let app = test_app(&dir, cfg).await;
    let boundary = "replace-test";
    let upload = |message: &'static [u8]| {
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"metadata\"\r\n\r\n{{}}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"message\"\r\n\r\n{}\r\n--{boundary}--\r\n",
            String::from_utf8_lossy(message)
        );
        Request::put("/v1/documents/s/replaced")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .unwrap()
    };

    let first = app
        .clone()
        .oneshot(upload(fixture("plain.eml")))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let second = app
        .clone()
        .oneshot(upload(fixture("html-only.eml")))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    let body = axum::body::to_bytes(
        app.oneshot(
            Request::get("/v1/documents/s/replaced")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .into_body(),
        1_000_000,
    )
    .await
    .unwrap();
    let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(document["subject"], "HTML & subject");
    assert!(document["body"].as_str().unwrap().contains("HTML marker"));
}
