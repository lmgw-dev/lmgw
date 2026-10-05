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
    queue_files_at(state, repo, files, target, "main", None).await
}

/// [`queue_files`] at `revision`: the listing and every queued row take it,
/// so the files come from that revision and the rows say so. `pin_note` is
/// added where the revision, or a file at it, is not there — what an audio
/// catalog download says about its spec's pin.
pub(crate) async fn queue_files_at(
    state: &SharedState,
    repo: &str,
    files: &[String],
    target: &str,
    revision: &str,
    pin_note: Option<&str>,
) -> Result<usize, String> {
    // Before the listing and before any row is written: a dev instance on a
    // models dir outside its data dir refuses here (`models_dir_to_write`).
    hf::models_dir_to_write(state, target)?;
    let snap = state.snapshot();
    let token = &snap.settings.hf_token;
    let available = hf::list_tree(&state.http, token, &hf::hf_base(), repo, revision)
        .await
        .map_err(|f| hf::listing_refusal(f, pin_note))?;
    let mut parts: Vec<String> = Vec::new();
    for file in files {
        if !available.iter().any(|f| &f.path == file) {
            let at = match revision == "main" {
                true => String::new(),
                false => format!(" at {revision}"),
            };
            let note = pin_note.map(|n| format!(" — {n}")).unwrap_or_default();
            return Err(format!("{file} not found in {repo}{at}{note}"));
        }
        for part in hf::expand_parts(file, &available) {
            if !parts.contains(&part) {
                parts.push(part);
            }
        }
    }
    for part in &parts {
        let dest = hf::dest_rel_path(repo, part)?;
        let id = store::upsert_hf_model_at(&state.db, repo, part, &dest, target, revision)
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
    hf::models_dir_to_write(state, &row.target)?;
    let settings = &state.snapshot().settings;
    // Asking twice used to be a silent no-op inside the download registry. The
    // job subsystem knows one is already in flight, so say so — before the
    // row's revision is touched: the running transfer fetches what the row
    // asked for when it started, and the row has to keep saying that.
    if let Some(job) = state
        .jobs
        .live_by_key(JobKind::HfDownload, &hf_download::job_key(id))
    {
        return Err(already_downloading(&row.file, job.id));
    }
    // The revision the row tracks now (audio/pins.rs): `main` under `latest`;
    // under `pinned`, the pin its spec names today — a moved pin, or a row
    // taken at `main` before pins were followed, is re-downloaded at it.
    let catalog = match row.target == "audio" {
        true => super::audio::catalog_cached(state).await,
        false => None,
    };
    let mode = settings.audio.catalog_revision;
    let revision = crate::audio::pins::tracked_revision(&row, catalog.as_ref(), mode);
    if revision != row.revision() {
        store::set_hf_revision(&state.db, id, revision)
            .await
            .map_err(|e| e.to_string())?;
    }
    let file = row.file.clone();
    match hf_download::start(state, &row).await? {
        Spawn::Started(_) => Ok(file),
        Spawn::AlreadyRunning(job) => Err(already_downloading(&file, job)),
    }
}

fn already_downloading(file: &str, job: i64) -> String {
    format!("{file} is already downloading (job {job}) — cancel it first to restart")
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
    // Asked before the row goes: a dev instance that may not delete the file
    // keeps the entry too, rather than untracking a file it leaves behind.
    let models_dir = models_dir_for_target(&state.snapshot().settings, &row.target);
    let dest = std::path::Path::new(&models_dir).join(&row.dest_path);
    if !models_dir.trim().is_empty() {
        state.refuse_shared_models_dir(&dest)?;
    }
    store::delete_hf_model(&state.db, id)
        .await
        .map_err(|e| e.to_string())?;
    if !models_dir.trim().is_empty() {
        let _ = tokio::fs::remove_file(format!("{}.part", dest.display())).await;
        let _ = tokio::fs::remove_file(&dest).await;
    }
    // One GGUF fewer beside a row's weights can change what that row
    // renders (the direct-file pick): a running container is stopped for
    // apply.
    if row.target == "audio" {
        crate::runtime::audio::stop_stale(state, &format!("deleting {}", row.dest_path)).await;
    }
    Ok(())
}

/// Stop a running transfer. The tracked row stays, flagged failed with the
/// cancellation as its error, so the entry offers a retry.
pub(crate) async fn cancel_download(state: &SharedState, id: i64) -> Result<String, String> {
    hf_download::cancel_for_row(state, id).await
}

/// ETag-compare every completed download for `target` against the hub, each
/// at the revision it tracks ([`crate::audio::pins::tracked_revision`]).
/// Returns (number of updates found, per-file errors).
pub(crate) async fn check_updates(state: &SharedState, target: &str) -> (usize, Vec<String>) {
    let rows = store::list_hf_models_by_target(&state.db, target)
        .await
        .unwrap_or_default();
    // Read once for the whole pass, not per row.
    let catalog = match target == "audio" {
        true => super::audio::catalog_cached(state).await,
        false => None,
    };
    let mode = state.snapshot().settings.audio.catalog_revision;
    let (mut updates, mut errors) = (0usize, Vec::new());
    for row in rows
        .iter()
        .filter(|r| r.status == "done" || r.status == "update_available")
    {
        let revision = crate::audio::pins::tracked_revision(row, catalog.as_ref(), mode);
        match hf::check_update_at(state, row, revision).await {
            Ok(true) => updates += 1,
            Ok(false) => {}
            Err(e) => errors.push(e),
        }
    }
    (updates, errors)
}
