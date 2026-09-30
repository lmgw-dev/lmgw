//! Hugging Face download actions (§11): queue a repo's GGUF files into a
//! models dir, re-download, untrack, and ETag-check for updates.
//!
//! Downloads are tagged with a `target` (`chat` / `aux` / `audio` / `image`) so
//! the same flow serves all four models dirs; which file kinds each accepts is
//! [`crate::hf::accepted_extensions`]'s business, not this module's. Everything here is `pub(crate)` and
//! called from both front doors — the `/api` plane (via [`crate::ops`]) and
//! the MCP tool plane — so neither has its own copy of the rules.

use crate::hf::{self, models_dir_for_target};
use crate::jobs::{hf_download, JobKind, Spawn};
use crate::state::SharedState;
use crate::store;

/// Queue an explicit set of repo files (e.g. an audio.cpp spec package) into
/// `target`'s models dir with a single repo listing. Split-GGUF names still
/// expand to all parts present in the repo. Returns the number of files
/// queued.
pub(crate) async fn queue_files(
    state: &SharedState,
    repo: &str,
    files: &[String],
    target: &str,
) -> Result<usize, String> {
    let snap = state.snapshot();
    hf::models_dir_or_refuse(&snap.settings, target)?;
    let available = hf::list_repo_files(&state.http, &snap.settings.hf_token, repo).await?;
    let mut parts: Vec<String> = Vec::new();
    for file in files {
        if !available.iter().any(|f| &f.path == file) {
            return Err(format!("{file} not found in {repo}"));
        }
        for part in hf::expand_parts(file, &available) {
            if !parts.contains(&part) {
                parts.push(part);
            }
        }
    }
    for part in &parts {
        let dest = hf::dest_rel_path(repo, part)?;
        let id = store::upsert_hf_model(&state.db, repo, part, &dest, target)
            .await
            .map_err(|e| e.to_string())?;
        if let Ok(Some(row)) = store::get_hf_model(&state.db, id).await {
            hf_download::start(state, &row).await?;
        }
    }
    Ok(parts.len())
}

/// Re-download a tracked row (also serves update / retry / resume). Returns the
/// file name on success.
pub(crate) async fn redownload(state: &SharedState, id: i64) -> Result<String, String> {
    let Ok(Some(row)) = store::get_hf_model(&state.db, id).await else {
        return Err("download entry not found".into());
    };
    hf::models_dir_or_refuse(&state.snapshot().settings, &row.target)?;
    let file = row.file.clone();
    // Asking twice used to be a silent no-op inside the download registry. The
    // job subsystem knows one is already in flight, so say so.
    match hf_download::start(state, &row).await? {
        Spawn::Started(_) => Ok(file),
        Spawn::AlreadyRunning(job) => Err(format!(
            "{file} is already downloading (job {job}) — cancel it first to restart"
        )),
    }
}

/// Untrack and delete the downloaded file (and any stray `.part`).
pub(crate) async fn delete_tracked(state: &SharedState, id: i64) -> Result<(), String> {
    let Ok(Some(row)) = store::get_hf_model(&state.db, id).await else {
        return Err("download entry not found".into());
    };
    if state
        .jobs
        .live_by_key(JobKind::HfDownload, &hf_download::job_key(id))
        .is_some()
    {
        return Err("download is running — cancel it or wait for it to finish".into());
    }
    store::delete_hf_model(&state.db, id)
        .await
        .map_err(|e| e.to_string())?;
    let models_dir = models_dir_for_target(&state.snapshot().settings, &row.target);
    if !models_dir.trim().is_empty() {
        let dest = std::path::Path::new(&models_dir).join(&row.dest_path);
        let _ = tokio::fs::remove_file(format!("{}.part", dest.display())).await;
        let _ = tokio::fs::remove_file(&dest).await;
    }
    Ok(())
}

/// Stop a running transfer. The tracked row stays, flagged failed with the
/// cancellation as its error, so the entry offers a retry.
pub(crate) async fn cancel_download(state: &SharedState, id: i64) -> Result<String, String> {
    hf_download::cancel_for_row(state, id).await
}

/// ETag-compare every completed download for `target` against the hub.
/// Returns (number of updates found, per-file errors).
pub(crate) async fn check_updates(state: &SharedState, target: &str) -> (usize, Vec<String>) {
    let rows = store::list_hf_models_by_target(&state.db, target)
        .await
        .unwrap_or_default();
    let (mut updates, mut errors) = (0usize, Vec::new());
    for row in rows
        .iter()
        .filter(|r| r.status == "done" || r.status == "update_available")
    {
        match hf::check_update(state, row).await {
            Ok(true) => updates += 1,
            Ok(false) => {}
            Err(e) => errors.push(e),
        }
    }
    (updates, errors)
}
