mod common;
use common::*;
use mailindex::ingest::NormalizedMessage;
use mailindex::models::IndexState;

fn message(id: &str, body: &str, hash: &str) -> NormalizedMessage {
    NormalizedMessage {
        source: "test".into(),
        source_id: id.into(),
        producer_metadata_json: r#"{"producer":"fixture"}"#.into(),
        account_id: Some("acct".into()),
        mailbox_name: Some("INBOX".into()),
        imap_uid: Some(1),
        message_id: Some("<m>".into()),
        in_reply_to: None,
        references: vec![],
        sent_at: Some("2024-01-01T00:00:00Z".into()),
        subject: Some("subject".into()),
        sender: Some("Name <sender@example.com>".into()),
        to: vec!["to@example.com".into()],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        body: body.into(),
        body_truncated: false,
        raw_sha256: hash.into(),
        attachments: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_uploads_and_index_updates_share_the_writer_gate() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repository(&dir).await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(51));
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..50 {
        // Exercise Repository::clone as well as sharing one Arc<Repository>.
        let repo = repo.as_ref().clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            let id = n.to_string();
            barrier.wait().await;
            let result = repo.upsert(&message(&id, "body", &id)).await.unwrap();
            repo.mark_error(result.document_id, &id, "retry")
                .await
                .unwrap();
            repo.requeue("test", &id).await.unwrap().unwrap();
            repo.mark_indexed_any(result.document_id, &id)
                .await
                .unwrap();
            repo.mark_indexed_versions(&[(result.document_id, id.clone())])
                .await
                .unwrap();
            repo.requeue("test", &id).await.unwrap().unwrap();
            repo.mark_indexed(result.document_id, &id).await.unwrap();
        });
    }
    barrier.wait().await;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("concurrent writes should complete");
    let counts = repo.status_counts().await.unwrap();
    assert_eq!(counts.total, 50);
    assert_eq!(counts.indexed, 50);
    assert_eq!(counts.pending, 0);
    assert_eq!(counts.error, 0);
}

#[tokio::test]
async fn migrations_idempotency_replacement_and_cas() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repository(&dir).await;
    repo.migrate().await.unwrap();
    let first = repo.upsert(&message("id", "old", "h1")).await.unwrap();
    assert!(first.changed);
    let same = repo.upsert(&message("id", "ignored", "h1")).await.unwrap();
    assert!(!same.changed);
    assert_eq!(same.document_id, first.document_id);
    let changed = repo.upsert(&message("id", "new", "h2")).await.unwrap();
    assert!(changed.changed);
    let stored = repo.get_document("test", "id").await.unwrap().unwrap();
    assert_eq!(stored.body_text, "new");
    assert_eq!(stored.producer_metadata_json, r#"{"producer":"fixture"}"#);
    repo.mark_indexed_any(first.document_id, "h1")
        .await
        .unwrap();
    assert_eq!(
        repo.get_document("test", "id")
            .await
            .unwrap()
            .unwrap()
            .index_state,
        IndexState::Pending
    );
    repo.mark_error(first.document_id, "h1", "stale")
        .await
        .unwrap();
    assert_eq!(
        repo.get_document("test", "id")
            .await
            .unwrap()
            .unwrap()
            .index_state,
        IndexState::Pending
    );
    repo.mark_error(first.document_id, "h2", "broken")
        .await
        .unwrap();
    assert_eq!(
        repo.get_document("test", "id")
            .await
            .unwrap()
            .unwrap()
            .index_state,
        IndexState::Error
    );
    repo.requeue("test", "id").await.unwrap();
    repo.mark_indexed(first.document_id, "h2").await.unwrap();
    assert_eq!(
        repo.get_document("test", "id")
            .await
            .unwrap()
            .unwrap()
            .index_state,
        IndexState::Indexed
    );
}

#[tokio::test]
async fn duplicate_rfc_message_ids_coexist_as_distinct_documents() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repository(&dir).await;
    let mut first = message("dup-a", "first body", "h-a");
    first.message_id = Some("<same@example.com>".into());
    let mut second = message("dup-b", "second body", "h-b");
    second.message_id = Some("<same@example.com>".into());
    let a = repo.upsert(&first).await.unwrap();
    let b = repo.upsert(&second).await.unwrap();
    assert_ne!(a.document_id, b.document_id);
    let a = repo.get_document("test", "dup-a").await.unwrap().unwrap();
    let b = repo.get_document("test", "dup-b").await.unwrap().unwrap();
    assert_eq!(a.message_id.as_deref(), Some("<same@example.com>"));
    assert_eq!(b.message_id.as_deref(), Some("<same@example.com>"));
    assert_eq!(a.body_text, "first body");
    assert_eq!(b.body_text, "second body");
}

#[tokio::test]
async fn foreign_key_attachment_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let repo = repository(&dir).await;
    let mut m = message("id", "body", "h");
    m.attachments.push(mailindex::ingest::NormalizedAttachment {
        part_index: 0,
        filename: Some("a.txt".into()),
        media_type: Some("text/plain".into()),
        disposition: None,
        content_id: None,
        size_bytes: 1,
        sha256: "a".into(),
        status: "extracted".into(),
        error: None,
        text: Some("text".into()),
        text_truncated: false,
    });
    repo.upsert(&m).await.unwrap();
    m.raw_sha256 = "h2".into();
    m.attachments.clear();
    repo.upsert(&m).await.unwrap();
    assert!(
        repo.get_document("test", "id")
            .await
            .unwrap()
            .unwrap()
            .attachments
            .is_empty()
    );
}
