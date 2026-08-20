mod common;
use common::{config, repository};
use mailindex::{
    index::{
        SearchIndex,
        worker::{IndexWorker, WorkerHooks},
    },
    ingest::{NormalizedAttachment, NormalizedMessage},
    models::SearchRequest,
    search::SearchService,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    sync::Notify,
    time::{Duration, sleep},
};
use tokio_util::sync::CancellationToken;

fn msg(id: &str, subject: &str, body: &str, hash: &str) -> NormalizedMessage {
    NormalizedMessage {
        source: "src".into(),
        source_id: id.into(),
        producer_metadata_json: "{}".into(),
        account_id: Some("a".into()),
        mailbox_name: Some("INBOX".into()),
        imap_uid: None,
        message_id: None,
        in_reply_to: None,
        references: vec![],
        sent_at: Some("2024-01-02T00:00:00Z".into()),
        subject: Some(subject.into()),
        sender: Some(
            if id == "one" {
                "Name <sender@example.com>"
            } else {
                "Other <other@example.com>"
            }
            .into(),
        ),
        to: vec![],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        body: body.into(),
        body_truncated: false,
        raw_sha256: hash.into(),
        attachments: vec![],
    }
}

#[tokio::test]
async fn worker_indexes_replacements_and_sender_filters() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    repo.upsert(&msg("one", "Important subject", "old unique text", "one"))
        .await
        .unwrap();
    repo.upsert(&msg("two", "Other", "sender body", "two"))
        .await
        .unwrap();
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
        20,
        cancel.clone(),
    );
    let handle = tokio::spawn(worker.run());
    sleep(Duration::from_millis(150)).await;
    let service = SearchService {
        repo: repo.clone(),
        index: idx,
        config: Arc::new(cfg.clone()),
    };
    let found = service
        .search(SearchRequest {
            query: "unique".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(found.results.len(), 1);
    let sender = service
        .search(SearchRequest {
            query: "sender".into(),
            senders: vec!["sender@example.com".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(sender.results.len(), 1);
    repo.upsert(&msg("one", "Changed", "replacement marker", "three"))
        .await
        .unwrap();
    notify.notify_one();
    sleep(Duration::from_millis(150)).await;
    assert!(
        service
            .search(SearchRequest {
                query: "old".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .results
            .is_empty()
    );
    assert_eq!(
        service
            .search(SearchRequest {
                query: "replacement".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .results
            .len(),
        1
    );
    cancel.cancel();
    notify.notify_one();
    handle.await.unwrap();
}

#[tokio::test]
async fn structured_filters_and_subject_boost_are_applied() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;

    let mut subject_hit = msg(
        "subject",
        "rankterm rankterm rankterm in subject",
        "ordinary",
        "rank-subject",
    );
    subject_hit.account_id = Some("acct-a".into());
    subject_hit.mailbox_name = Some("INBOX".into());
    let mut body_hit = msg("body", "ordinary", "rankterm in body", "rank-body");
    body_hit.account_id = Some("acct-a".into());
    body_hit.mailbox_name = Some("Archive".into());
    let mut other_account = msg("other", "rankterm", "ordinary", "rank-other");
    other_account.account_id = Some("acct-b".into());
    other_account.mailbox_name = Some("INBOX".into());
    for document in [subject_hit, body_hit, other_account] {
        repo.upsert(&document).await.unwrap();
    }

    let (idx, writer) = SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let idx = Arc::new(idx);
    let cancel = CancellationToken::new();
    let worker = IndexWorker::new(
        repo.clone(),
        idx.clone(),
        writer,
        Arc::new(Notify::new()),
        10,
        10,
        cancel.clone(),
    );
    let handle = tokio::spawn(worker.run());
    sleep(Duration::from_millis(100)).await;
    let service = SearchService {
        repo,
        index: idx,
        config: Arc::new(cfg),
    };

    let filtered = service
        .search(SearchRequest {
            query: "rankterm".into(),
            account_ids: vec!["acct-a".into()],
            mailboxes: vec!["INBOX".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(filtered.results.len(), 1);
    assert_eq!(filtered.results[0].source_id, "subject");

    let ranked = service
        .search(SearchRequest {
            query: "rankterm".into(),
            account_ids: vec!["acct-a".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ranked.results[0].source_id, "subject");

    cancel.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn date_bounds_are_midnight_inclusive_then_exclusive() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    for (id, sent_at, hash) in [
        ("before", "2024-01-01T00:00:00Z", "before-hash"),
        ("at", "2024-01-02T00:00:00Z", "at-hash"),
        ("after", "2024-01-03T00:00:00Z", "after-hash"),
        (
            "fraction-before",
            "2024-01-04T00:00:00.499Z",
            "fraction-before-hash",
        ),
        (
            "fraction-at",
            "2024-01-04T00:00:00.500Z",
            "fraction-at-hash",
        ),
        (
            "fraction-after",
            "2024-01-04T00:00:00.501Z",
            "fraction-after-hash",
        ),
    ] {
        let mut document = msg(id, "boundary", "boundary marker", hash);
        document.sent_at = Some(sent_at.into());
        repo.upsert(&document).await.unwrap();
    }
    let (idx, writer) = SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let idx = Arc::new(idx);
    let cancel = CancellationToken::new();
    let worker = IndexWorker::new(
        repo.clone(),
        idx.clone(),
        writer,
        Arc::new(Notify::new()),
        10,
        10,
        cancel.clone(),
    );
    let handle = tokio::spawn(worker.run());
    sleep(Duration::from_millis(100)).await;
    let service = SearchService {
        repo,
        index: idx,
        config: Arc::new(cfg),
    };
    let response = service
        .search(SearchRequest {
            query: "boundary".into(),
            after: Some("2024-01-01".into()),
            before: Some("2024-01-02".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].source_id, "before");
    let fractional = service
        .search(SearchRequest {
            query: "boundary".into(),
            after: Some("2024-01-04T00:00:00.500Z".into()),
            before: Some("2024-01-04T00:00:00.501Z".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        fractional
            .results
            .iter()
            .map(|result| result.source_id.as_str())
            .collect::<Vec<_>>(),
        vec!["fraction-at"]
    );
    cancel.cancel();
    handle.await.unwrap();
}

#[tokio::test]
async fn attachment_only_hits_use_the_bounded_projection() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    let mut document = msg(
        "attachment",
        "ordinary",
        &"ordinary body ".repeat(60),
        "attachment-hash",
    );
    document.attachments = vec![NormalizedAttachment {
        part_index: 1,
        filename: Some("notes.txt".into()),
        media_type: Some("text/plain".into()),
        disposition: Some("attachment".into()),
        content_id: None,
        size_bytes: 32,
        sha256: "attachment-sha".into(),
        status: "extracted".into(),
        error: None,
        text: Some("distinctive attachment-only marker".into()),
        text_truncated: false,
    }];
    repo.upsert(&document).await.unwrap();
    let (idx, writer) = SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let idx = Arc::new(idx);
    let cancel = CancellationToken::new();
    let worker = IndexWorker::new(
        repo.clone(),
        idx.clone(),
        writer,
        Arc::new(Notify::new()),
        10,
        10,
        cancel.clone(),
    );
    let handle = tokio::spawn(worker.run());
    sleep(Duration::from_millis(100)).await;
    let service = SearchService {
        repo,
        index: idx,
        config: Arc::new(cfg),
    };
    let response = service
        .search(SearchRequest {
            query: "distinctive attachment-only marker".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(response.results.len(), 1);
    assert!(response.results[0].snippet.contains("attachment-only"));
    assert_eq!(
        response.results[0].attachments[0].filename.as_deref(),
        Some("notes.txt")
    );
    cancel.cancel();
    handle.await.unwrap();
}

#[test]
fn document_keys_are_collision_free_and_unicode_safe() {
    for (source, id) in [("a:b", "c"), ("é", "x:y"), ("source", "id")] {
        let key = mailindex::index::document_key(source, id);
        assert_eq!(
            mailindex::index::decode_key(&key),
            Some((source.into(), id.into()))
        );
    }
    assert!(mailindex::index::decode_key("1:x").is_none());
    assert!(mailindex::index::decode_key("1:x:").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_cas_rejects_a_stale_batch_and_cancellation_stops_the_next_batch() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    let repo = common::repository(&dir).await;
    repo.upsert(&msg("one", "old", "old body", "old-hash"))
        .await
        .unwrap();
    repo.upsert(&msg("two", "second", "second body", "second-hash"))
        .await
        .unwrap();

    let (index, writer) =
        SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    let index = Arc::new(index);
    let cancel = CancellationToken::new();
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let changing_repo = repo.clone();
    let mutator = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || start_rx.recv().unwrap())
            .await
            .unwrap();
        changing_repo
            .upsert(&msg("one", "new", "new body", "new-hash"))
            .await
            .unwrap();
        done_tx.send(()).unwrap();
    });
    let done_rx = Arc::new(Mutex::new(done_rx));
    let before_called = Arc::new(AtomicBool::new(false));
    let stop_after_first = cancel.clone();
    let hooks = WorkerHooks {
        before_batch: Some(Arc::new(move |_| {
            if !before_called.swap(true, Ordering::SeqCst) {
                start_tx.send(()).unwrap();
                done_rx.lock().unwrap().recv().unwrap();
            }
        })),
        after_batch: Some(Arc::new(move |number| {
            if number == 1 {
                stop_after_first.cancel();
            }
        })),
    };
    let worker = IndexWorker::new_with_hooks(
        repo.clone(),
        index,
        writer,
        Arc::new(Notify::new()),
        1,
        10,
        cancel,
        hooks,
    );
    let worker_task = tokio::spawn(worker.run());
    worker_task.await.unwrap();
    mutator.await.unwrap();
    let one = repo.get_document("src", "one").await.unwrap().unwrap();
    let two = repo.get_document("src", "two").await.unwrap().unwrap();
    assert_eq!(one.raw_sha256, "new-hash");
    assert_eq!(one.index_state, mailindex::models::IndexState::Pending);
    assert_eq!(two.index_state, mailindex::models::IndexState::Pending);
}
