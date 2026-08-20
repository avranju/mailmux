mod common;
use common::{config, repository};
use mailindex::{
    index::{
        SearchIndex,
        rebuild::{RebuildHooks, rebuild_index, rebuild_index_with_hooks},
    },
    ingest::NormalizedMessage,
    models::SearchRequest,
    search::SearchService,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// List sibling files or directories of the active index whose name starts
/// with `<index>.<tag>`, in the same naming pattern rebuild uses.
fn siblings(index_path: &Path, tag: &str) -> Vec<PathBuf> {
    let parent = index_path.parent().expect("index path has a parent");
    let index_name = index_path
        .file_name()
        .expect("index path has a file name")
        .to_string_lossy();
    let prefix = format!("{index_name}{tag}");
    std::fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
        })
        .collect()
}

async fn search_snapshot(
    service: &SearchService,
    terms: &[&str],
) -> BTreeMap<String, Vec<(String, f32)>> {
    let mut out = BTreeMap::new();
    for term in terms {
        let found = service
            .search(SearchRequest {
                query: (*term).into(),
                limit: Some(50),
                ..Default::default()
            })
            .await
            .unwrap();
        out.insert(
            (*term).to_owned(),
            found
                .results
                .into_iter()
                .map(|result| (result.source_id, result.score))
                .collect(),
        );
    }
    out
}

fn search_service(
    repo: &Arc<mailindex::storage::Repository>,
    cfg: &mailindex::config::Config,
) -> SearchService {
    let (index, writer) =
        SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    drop(writer);
    SearchService {
        repo: repo.clone(),
        index: Arc::new(index),
        config: Arc::new(cfg.clone()),
    }
}

#[tokio::test]
async fn rebuild_is_bounded_and_marks_error_rows_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    let message = NormalizedMessage {
        source: "s".into(),
        source_id: "i".into(),
        producer_metadata_json: "{}".into(),
        account_id: None,
        mailbox_name: None,
        imap_uid: None,
        message_id: None,
        in_reply_to: None,
        references: vec![],
        sent_at: None,
        subject: Some("rebuild marker".into()),
        sender: None,
        to: vec![],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        body: "rebuild body".into(),
        body_truncated: false,
        raw_sha256: "hash".into(),
        attachments: vec![],
    };
    let id = repo.upsert(&message).await.unwrap().document_id;
    repo.mark_error(id, "hash", "old index error")
        .await
        .unwrap();
    let summary = rebuild_index(&cfg, repo.clone()).await.unwrap();
    assert_eq!(summary.documents, 1);
    assert_eq!(repo.status_counts().await.unwrap().indexed, 1);
    let (index, writer) =
        SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    assert_eq!(index.reader.searcher().num_docs(), 1);
    drop(writer);
}

#[tokio::test]
async fn rebuild_refuses_an_active_writer_without_destroying_index() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    let (index, writer) =
        SearchIndex::open(&cfg.index.path, cfg.index.writer_memory_bytes).unwrap();
    drop(index);
    let result = rebuild_index(&cfg, repo).await;
    assert!(result.is_err());
    assert!(cfg.index.path.join("meta.json").exists());
    drop(writer);
}

fn rebuild_message(id: &str, hash: &str) -> NormalizedMessage {
    NormalizedMessage {
        source: "rebuild".into(),
        source_id: id.into(),
        producer_metadata_json: "{}".into(),
        account_id: None,
        mailbox_name: None,
        imap_uid: None,
        message_id: None,
        in_reply_to: None,
        references: vec![],
        sent_at: None,
        subject: Some(id.into()),
        sender: None,
        to: vec![],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        body: format!("body {id}"),
        body_truncated: false,
        raw_sha256: hash.into(),
        attachments: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_cas_leaves_a_document_pending_when_it_changes_mid_build() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&dir);
    cfg.index.batch_size = 1;
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    repo.upsert(&rebuild_message("one", "old-hash"))
        .await
        .unwrap();
    repo.upsert(&rebuild_message("two", "two-hash"))
        .await
        .unwrap();

    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let changing_repo = repo.clone();
    let mutator = tokio::spawn(async move {
        tokio::task::spawn_blocking(move || start_rx.recv().unwrap())
            .await
            .unwrap();
        changing_repo
            .upsert(&rebuild_message("one", "new-hash"))
            .await
            .unwrap();
        done_tx.send(()).unwrap();
    });
    let done_rx = Arc::new(std::sync::Mutex::new(done_rx));
    let first = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hooks = RebuildHooks {
        after_batch: Some(Arc::new(move |number| {
            if number == 1 && !first.swap(true, std::sync::atomic::Ordering::SeqCst) {
                start_tx.send(()).unwrap();
                done_rx.lock().unwrap().recv().unwrap();
            }
            Ok(())
        })),
        before_install: None,
    };
    rebuild_index_with_hooks(&cfg, repo.clone(), hooks)
        .await
        .unwrap();
    mutator.await.unwrap();
    let changed = repo.get_document("rebuild", "one").await.unwrap().unwrap();
    assert_eq!(changed.raw_sha256, "new-hash");
    assert_eq!(changed.index_state, mailindex::models::IndexState::Pending);
}

#[tokio::test]
async fn rebuild_failure_before_swap_cleans_only_the_temporary_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&dir);
    cfg.index.batch_size = 1;
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    repo.upsert(&rebuild_message("one", "hash")).await.unwrap();
    rebuild_index(&cfg, repo.clone()).await.unwrap();
    let active_meta = std::fs::read(cfg.index.path.join("meta.json")).unwrap();
    let result = rebuild_index_with_hooks(
        &cfg,
        repo,
        RebuildHooks {
            after_batch: Some(Arc::new(|_| anyhow::bail!("injected build failure"))),
            before_install: None,
        },
    )
    .await;
    assert!(result.is_err());
    // The temporary index and its version manifest are cleaned up, and no
    // backup was created before the swap.
    assert!(siblings(&cfg.index.path, ".rebuild-").is_empty());
    assert!(siblings(&cfg.index.path, ".backup-").is_empty());
    assert_eq!(
        std::fs::read(cfg.index.path.join("meta.json")).unwrap(),
        active_meta
    );
}

#[tokio::test]
async fn rebuild_produces_an_equivalent_searchable_corpus() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    for id in ["alpha", "beta", "gamma"] {
        let mut document = rebuild_message(id, &format!("hash-{id}"));
        document.body = format!("body {id} {id}-token");
        repo.upsert(&document).await.unwrap();
    }
    repo.mark_error(
        repo.get_document("rebuild", "beta")
            .await
            .unwrap()
            .unwrap()
            .id,
        "hash-beta",
        "old index error",
    )
    .await
    .unwrap();

    rebuild_index(&cfg, repo.clone()).await.unwrap();
    let before = search_snapshot(
        &search_service(&repo, &cfg),
        &["alpha-token", "beta-token", "gamma-token"],
    )
    .await;
    for (term, hits) in &before {
        assert_eq!(hits.len(), 1, "search for {term} hit {hits:?}");
    }

    // A second rebuild exercises the backup-and-swap installation path.
    let summary = rebuild_index(&cfg, repo.clone()).await.unwrap();
    assert_eq!(summary.documents, 3);
    let counts = repo.status_counts().await.unwrap();
    assert_eq!(counts.indexed, 3);
    assert_eq!(counts.pending + counts.error, 0);

    let after = search_snapshot(
        &search_service(&repo, &cfg),
        &["alpha-token", "beta-token", "gamma-token"],
    )
    .await;
    assert_eq!(before, after);
    // The successful swap removed this run's own backup and temporary paths.
    assert!(siblings(&cfg.index.path, ".backup-").is_empty());
    assert!(siblings(&cfg.index.path, ".rebuild-").is_empty());
}

#[tokio::test]
async fn rebuild_install_failure_restores_the_previous_index() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    // Old corpus: after this rebuild the installed index contains only "old".
    repo.upsert(&rebuild_message("old", "hash-old"))
        .await
        .unwrap();
    rebuild_index(&cfg, repo.clone()).await.unwrap();
    let active_meta = std::fs::read(cfg.index.path.join("meta.json")).unwrap();

    // Prospective corpus is distinguishable: "new" exists only in Turso, so a
    // successful rebuild would make it searchable, but the restored index
    // must not contain it.
    repo.upsert(&rebuild_message("new", "hash-new"))
        .await
        .unwrap();
    let result = rebuild_index_with_hooks(
        &cfg,
        repo.clone(),
        RebuildHooks {
            after_batch: None,
            before_install: Some(Arc::new(|_, _| anyhow::bail!("injected install failure"))),
        },
    )
    .await;
    assert!(result.is_err());

    // The previous corpus remains searchable with its original contents.
    let service = search_service(&repo, &cfg);
    assert_eq!(
        service
            .search(SearchRequest {
                query: "old".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .results
            .len(),
        1
    );
    assert!(
        service
            .search(SearchRequest {
                query: "new".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .results
            .is_empty()
    );
    // The restored directory is byte-identical to the pre-rebuild index and
    // no backup or temporary siblings are left behind.
    assert_eq!(
        std::fs::read(cfg.index.path.join("meta.json")).unwrap(),
        active_meta
    );
    assert!(siblings(&cfg.index.path, ".backup-").is_empty());
    assert!(siblings(&cfg.index.path, ".rebuild-").is_empty());
}

#[tokio::test]
async fn rebuild_preserves_preexisting_backup_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir);
    cfg.validate().unwrap();
    let repo = repository(&dir).await;
    // Simulate a backup retained by a previous failed restoration and a
    // temporary index left by a crashed build, both following the naming
    // pattern. They must survive untouched: deleting them could destroy the
    // only usable index.
    let retained_backup = PathBuf::from(format!(
        "{}.backup-00000000-1111-4222-8333-444444444444",
        cfg.index.path.display()
    ));
    let crashed_temp = PathBuf::from(format!(
        "{}.rebuild-00000000-1111-4222-8333-444444444444",
        cfg.index.path.display()
    ));
    std::fs::create_dir_all(&retained_backup).unwrap();
    std::fs::write(retained_backup.join("retained.txt"), b"retained").unwrap();
    std::fs::create_dir_all(&crashed_temp).unwrap();
    std::fs::write(crashed_temp.join("crashed.txt"), b"crashed").unwrap();

    repo.upsert(&rebuild_message("one", "hash")).await.unwrap();
    rebuild_index(&cfg, repo.clone()).await.unwrap();
    assert_eq!(
        std::fs::read(retained_backup.join("retained.txt")).unwrap(),
        b"retained"
    );
    assert_eq!(
        std::fs::read(crashed_temp.join("crashed.txt")).unwrap(),
        b"crashed"
    );
    assert_eq!(
        siblings(&cfg.index.path, ".backup-"),
        vec![retained_backup.clone()]
    );
    assert_eq!(
        siblings(&cfg.index.path, ".rebuild-"),
        vec![crashed_temp.clone()]
    );

    // A later failed build must also leave the retained backup untouched;
    // the previous implementation deleted it before the rebuild started.
    let result = rebuild_index_with_hooks(
        &cfg,
        repo,
        RebuildHooks {
            after_batch: Some(Arc::new(|_| anyhow::bail!("injected build failure"))),
            before_install: None,
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        std::fs::read(retained_backup.join("retained.txt")).unwrap(),
        b"retained"
    );
    assert_eq!(siblings(&cfg.index.path, ".backup-"), vec![retained_backup]);
    assert_eq!(siblings(&cfg.index.path, ".rebuild-"), vec![crashed_temp]);
}
