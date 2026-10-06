use crate::{
    config::Config,
    index::{SearchIndex, tantivy_doc},
    storage::Repository,
};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Debug, Default)]
pub struct RebuildSummary {
    pub documents: i64,
}

/// Synchronous seams used by recovery tests. They are also useful to callers
/// embedding maintenance operations: hooks run without message content and
/// are never installed by the normal CLI path.
pub type RebuildBatchHook = Arc<dyn Fn(usize) -> Result<()> + Send + Sync>;

pub type RebuildInstallHook = Arc<dyn Fn(&Path, &Path) -> Result<()> + Send + Sync>;

#[derive(Clone, Default)]
pub struct RebuildHooks {
    pub after_batch: Option<RebuildBatchHook>,
    pub before_install: Option<RebuildInstallHook>,
}

struct TemporaryIndex {
    path: PathBuf,
    manifest: PathBuf,
    armed: bool,
}

impl TemporaryIndex {
    fn new(path: PathBuf, manifest: PathBuf) -> Self {
        Self {
            path,
            manifest,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.path.exists()
            && let Err(error) = fs::remove_dir_all(&self.path)
        {
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "failed to remove temporary rebuild index"
            );
        }
        if self.manifest.exists()
            && let Err(error) = fs::remove_file(&self.manifest)
        {
            tracing::warn!(
                path = %self.manifest.display(),
                %error,
                "failed to remove rebuild version manifest"
            );
        }
    }
}

/// Allocate a new unique sibling path next to the active index.
///
/// Names carry a fresh UUIDv4, so they cannot collide with leftovers from
/// earlier crashed runs. Existing siblings are never deleted or overwritten:
/// a backup retained by a previous failed restoration (or a temporary index
/// left by a crashed build) must survive for operator recovery, so a
/// collision simply allocates another name.
fn unique_sibling(base: &Path, tag: &str) -> Result<PathBuf> {
    let base_name = base.display().to_string();
    for _ in 0..16 {
        let candidate = PathBuf::from(format!("{base_name}.{tag}-{}", uuid::Uuid::new_v4()));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("could not allocate a unique {tag} sibling path next to {base_name}")
}

/// Rebuild into a freshly named sibling directory and only touch the active
/// directory after the complete corpus has been committed and counted.
/// Temporary and backup siblings get collision-resistant UUID names, and
/// pre-existing siblings are never deleted. Version tokens are written to a
/// disk-backed manifest, so memory remains bounded by one index batch even
/// for very large archives.
pub async fn rebuild_index(config: &Config, repo: Arc<Repository>) -> Result<RebuildSummary> {
    rebuild_index_with_hooks(config, repo, RebuildHooks::default()).await
}

pub async fn rebuild_index_with_hooks(
    config: &Config,
    repo: Arc<Repository>,
    hooks: RebuildHooks,
) -> Result<RebuildSummary> {
    let active = &config.index.path;
    let tmp = unique_sibling(active, "rebuild")?;
    let manifest = PathBuf::from(format!("{}.versions", tmp.display()));
    let backup = unique_sibling(active, "backup")?;
    let mut temporary = TemporaryIndex::new(tmp.clone(), manifest.clone());

    // Tantivy's writer lock is the serving/rebuild concurrency guard. Keep the
    // probe writer alive until the swap is complete.
    let active_guard = if active.join("meta.json").exists() {
        let existing = tantivy::Index::open_in_dir(active)
            .context("open active index; stop mailindex before rebuild")?;
        Some(
            existing
                .writer::<tantivy::schema::TantivyDocument>(config.index.writer_memory_bytes)
                .context("active index is in use; stop mailindex before rebuild")?,
        )
    } else {
        None
    };

    let (idx, mut writer) = SearchIndex::open(&tmp, config.index.writer_memory_bytes)?;
    let mut manifest_writer = BufWriter::new(File::create(&manifest)?);
    let mut after_id = 0;
    let mut total = 0i64;
    let batch_size = config.index.batch_size.max(1);
    let mut batch_number = 0;
    loop {
        let rows = repo.documents_after(after_id, batch_size).await?;
        if rows.is_empty() {
            break;
        }
        for d in &rows {
            writer.add_document(tantivy_doc(d, &idx.fields))?;
            // SHA-256 is hex and therefore cannot contain the delimiter or a
            // line ending. This keeps the manifest simple and streamable.
            writeln!(manifest_writer, "{}\t{}", d.id, d.raw_sha256)?;
        }
        writer.commit()?;
        manifest_writer.flush()?;
        total += rows.len() as i64;
        after_id = rows.last().map(|d| d.id).unwrap_or(after_id);
        batch_number += 1;
        if let Some(hook) = &hooks.after_batch {
            hook(batch_number)?;
        }
    }
    manifest_writer.into_inner()?.sync_all()?;
    idx.reload()?;
    let count = idx.reader.searcher().num_docs() as i64;
    if count != total || count != repo.document_count().await? {
        bail!("rebuilt index document count mismatch: expected {total}, got {count}");
    }
    drop(writer);

    if active.exists() {
        fs::rename(active, &backup).context("move active index to backup")?;
    }
    if let Some(hook) = &hooks.before_install
        && let Err(error) = hook(&tmp, active)
    {
        return restore_after_failed_install(active, &backup, error);
    }
    if let Err(error) = fs::rename(&tmp, active) {
        return restore_after_failed_install(active, &backup, error.into());
    }
    drop(active_guard);
    // `backup` is the directory this run created under a UUID name, so it can
    // only hold the replaced index. Pre-existing sibling backups keep their
    // unique names and are never touched here.
    if backup.exists() {
        fs::remove_dir_all(&backup).context("remove confirmed index backup")?;
    }

    // Apply exact ID/hash CAS updates from the manifest in bounded batches.
    // A document changed while the rebuild ran remains pending for the serving
    // worker; no complete-corpus Vec is retained in memory.
    let file = BufReader::new(File::open(&manifest)?);
    let mut versions = Vec::with_capacity(batch_size);
    for line in file.lines() {
        let line = line?;
        let (id, hash) = line
            .split_once('\t')
            .ok_or_else(|| anyhow::anyhow!("invalid rebuild version manifest entry"))?;
        versions.push((
            id.parse::<i64>()
                .context("invalid document ID in rebuild manifest")?,
            hash.to_owned(),
        ));
        if versions.len() == batch_size {
            repo.mark_indexed_versions(&versions).await?;
            versions.clear();
        }
    }
    repo.mark_indexed_versions(&versions).await?;
    fs::remove_file(&manifest)?;
    temporary.disarm();

    Ok(RebuildSummary { documents: total })
}

fn restore_after_failed_install(
    active: &Path,
    backup: &Path,
    error: anyhow::Error,
) -> Result<RebuildSummary> {
    if backup.exists()
        && let Err(restore_error) = fs::rename(backup, active)
    {
        return Err(anyhow::anyhow!(
            "install rebuilt index failed: {error}; restoring backup failed: {restore_error}; backup retained at {}",
            backup.display()
        ));
    }
    Err(error.context("install rebuilt index; previous index restored"))
}
