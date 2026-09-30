//! Corpus export and import (§10).
//!
//! The unit of portability is the corpus **file** (§3), so export is a copy of
//! it rather than a serialization of it: no format to keep in sync, and a
//! restored file is byte-for-byte the database that was running. Two things
//! make that copy safe to take from a live gateway:
//!
//! * `wal_checkpoint(TRUNCATE)` first — a `.db` copied without it is missing
//!   whatever is still in the write-ahead log;
//! * the copy is taken to a temp file the caller owns, so a slow download never
//!   holds a read lock on the corpus anybody else is querying.
//!
//! Import is the reverse copy, plus the two checks §10 asks for: the file's
//! **schema version** (older is upgraded on the temp copy, newer is refused by
//! version rather than by the first missing column) and **embed-model
//! availability** (a corpus whose pinned model this gateway cannot resolve
//! would import into something unqueryable, so it is refused by name with the
//! model it wants).

use std::path::{Path, PathBuf};

use quickdoc_core::store::{self as qstore, CopyOutcome, Manifest};
use serde::Serialize;
use sqlx::SqlitePool;

use crate::state::SharedState;

use super::embed::alias_for_identity;

/// A corpus file staged on disk, deleted when this is dropped.
pub struct Staged {
    pub path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Staged {
    pub fn bytes(&self) -> std::io::Result<Vec<u8>> {
        std::fs::read(&self.path)
    }
}

fn stage(name: &str) -> Result<Staged, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("staging a corpus copy: {e}"))?;
    let path = dir.path().join(name);
    Ok(Staged { path, _dir: dir })
}

/// Copy the whole corpus database to a temp file, checkpointed first.
///
/// `corpus_id` narrows it to one corpus: a fresh database is created and that
/// corpus alone is copied into it, which is what the per-corpus export action
/// in the Docs tab (§11) hands over. Everything else about the artifact is
/// identical — same schema, same import path.
pub async fn export(state: &SharedState, corpus_id: Option<i64>) -> Result<Staged, String> {
    qstore::checkpoint(&state.corpus)
        .await
        .map_err(|e| format!("checkpointing the corpus DB: {e}"))?;
    match corpus_id {
        None => {
            let staged = stage("quickdoc.db")?;
            let src = state.data_dir.join(super::CORPUS_DB_FILE);
            std::fs::copy(&src, &staged.path)
                .map_err(|e| format!("copying {}: {e}", src.display()))?;
            Ok(staged)
        }
        Some(id) => {
            let corpus = qstore::get_corpus(&state.corpus, id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no corpus {id}"))?;
            let staged = stage(&format!(
                "{}-{}.db",
                sanitize(&corpus.library),
                sanitize(&corpus.version)
            ))?;
            let dest = qstore::open(&staged.path)
                .await
                .map_err(|e| format!("creating the export file: {e}"))?;
            qstore::copy_corpus(&state.corpus, &dest, id, false)
                .await
                .map_err(|e| e.to_string())?;
            qstore::checkpoint(&dest).await.map_err(|e| e.to_string())?;
            dest.close().await;
            Ok(staged)
        }
    }
}

/// Filenames come from library names; keep them to something a browser will
/// save without argument.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// What an export contains, for the UI to show before downloading it.
pub async fn manifest(state: &SharedState, corpus_id: Option<i64>) -> Result<Manifest, String> {
    let staged = export(state, corpus_id).await?;
    let byte_size = std::fs::metadata(&staged.path)
        .map(|m| m.len())
        .unwrap_or(0);
    let pool = qstore::open(&staged.path)
        .await
        .map_err(|e| e.to_string())?;
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let m = qstore::manifest_for(&pool, byte_size, &at)
        .await
        .map_err(|e| e.to_string())?;
    pool.close().await;
    Ok(m)
}

/// One corpus's verdict from an import.
#[derive(Debug, Clone, Serialize)]
pub struct ImportedCorpus {
    pub corpus_id: String,
    pub embed_model: String,
    /// The name on *this* gateway that routes to the pinned model, when one
    /// does.
    pub embed_alias: Option<String>,
    pub chunks: i64,
    pub documents: i64,
    pub replaced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportReport {
    pub schema_version: i64,
    pub file_schema_version: i64,
    /// True when nothing was written — the `validate_only` pass.
    pub dry_run: bool,
    pub imported: Vec<ImportedCorpus>,
}

/// Validate and (unless `validate_only`) import a corpus file.
///
/// Both checks happen before **anything** is written, and either one failing
/// fails the whole import: a half-imported file leaves the owner guessing which
/// half. `replace` is what allows overwriting a `library@version` that already
/// exists here.
pub async fn import(
    state: &SharedState,
    file: &Path,
    replace: bool,
    validate_only: bool,
) -> Result<ImportReport, String> {
    // Migrating in place is a write, so the upload is copied first — the
    // caller's file is never modified.
    let staged = stage("import.db")?;
    std::fs::copy(file, &staged.path).map_err(|e| format!("staging the upload: {e}"))?;
    let (src, file_schema_version) = qstore::open_import(&staged.path)
        .await
        .map_err(|e| e.to_string())?;

    let result = plan_and_apply(state, &src, replace, validate_only).await;
    src.close().await;
    let imported = result?;
    Ok(ImportReport {
        schema_version: qstore::schema_version(),
        file_schema_version,
        dry_run: validate_only,
        imported,
    })
}

async fn plan_and_apply(
    state: &SharedState,
    src: &SqlitePool,
    replace: bool,
    validate_only: bool,
) -> Result<Vec<ImportedCorpus>, String> {
    let corpora = qstore::list_corpora(src).await.map_err(|e| e.to_string())?;
    if corpora.is_empty() {
        return Err("that corpus file contains no corpora".into());
    }

    let snap = state.snapshot();
    let mut plan = Vec::new();
    let mut missing = Vec::new();
    for c in &corpora {
        let pinned = c.embed_identity();
        let alias = alias_for_identity(&snap, &pinned);
        if alias.is_none() {
            missing.push(format!("{} needs {pinned}", c.corpus_id()));
        }
        if !replace {
            if let Some(existing) = qstore::get_corpus_by_id(&state.corpus, &c.corpus_id())
                .await
                .map_err(|e| e.to_string())?
            {
                return Err(format!(
                    "corpus {} already exists here ({} chunks) — import with replace=true to \
                     overwrite it",
                    c.corpus_id(),
                    existing.chunk_count
                ));
            }
        }
        plan.push((c.clone(), alias, pinned.to_string()));
    }
    if !missing.is_empty() {
        return Err(format!(
            "refusing the import: no model on this gateway resolves to the embedding identity \
             {} was pinned to, so it would arrive unqueryable — configure the model first, or \
             re-embed after importing on a gateway that has it ({})",
            if missing.len() == 1 {
                "one corpus"
            } else {
                "some corpora"
            },
            missing.join("; ")
        ));
    }

    let mut out = Vec::new();
    for (c, alias, pinned) in plan {
        let (documents, chunks, replaced) = if validate_only {
            (0, c.chunk_count, false)
        } else {
            let CopyOutcome {
                documents,
                chunks,
                replaced,
                ..
            } = qstore::copy_corpus(src, &state.corpus, c.id, replace)
                .await
                .map_err(|e| e.to_string())?;
            (documents, chunks, replaced)
        };
        out.push(ImportedCorpus {
            corpus_id: c.corpus_id(),
            embed_model: pinned,
            embed_alias: alias,
            chunks,
            documents,
            replaced,
        });
    }
    Ok(out)
}
