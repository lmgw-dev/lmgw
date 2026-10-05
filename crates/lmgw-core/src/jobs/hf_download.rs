//! The `hf_download` job kind: stream one tracked Hugging Face file into its
//! container's models dir (§8, §9c).
//!
//! The durable record of *what is on disk* stays in `hf_models` — status, ETag,
//! size, destination — because that is the model registry's truth and it
//! outlives any single transfer. The job row is the *transfer*: it starts,
//! reports bytes, and ends. Retention trims the transfers; the file record
//! stays until untracked.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use super::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::hf;
use crate::state::SharedState;
use crate::store::{self, HfModelRow};

/// `hf_models.error` left behind by a cancelled transfer. The row goes back to
/// `failed` so the Downloads page offers "Retry" — a cancelled download is a
/// file that is not there, and saying so is the honest state.
pub const CANCELED_NOTE: &str = "canceled";

/// One live job per tracked row.
pub fn job_key(hf_model_id: i64) -> String {
    format!("hf:{hf_model_id}")
}

/// Request payload of an `hf_download` job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Input {
    pub hf_model_id: i64,
}

/// Kind-specific progress detail (`done`/`total` are bytes). Enough for the
/// feed to name the transfer without joining anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detail {
    pub hf_model_id: i64,
    pub repo: String,
    pub file: String,
    pub target: String,
}

/// Start downloading a tracked row, or report the job already doing it.
pub async fn start(state: &SharedState, row: &HfModelRow) -> Result<Spawn, String> {
    super::spawn(
        state,
        JobKind::HfDownload,
        Some(job_key(row.id)),
        format!("{}/{}", row.repo, row.file),
        json!({ "hf_model_id": row.id }),
    )
    .await
}

/// Cancel the transfer of a tracked row, if one is running.
pub async fn cancel_for_row(state: &SharedState, hf_model_id: i64) -> Result<String, String> {
    let job = state
        .jobs
        .live_by_key(JobKind::HfDownload, &job_key(hf_model_id))
        .ok_or("no download is running for this entry")?;
    super::cancel(state, job.id).await
}

pub struct HfDownloadExecutor;

#[async_trait::async_trait]
impl JobExecutor for HfDownloadExecutor {
    fn kind(&self) -> JobKind {
        JobKind::HfDownload
    }

    async fn run(&self, ctx: JobCtx, input: Value) -> Result<JobOutcome, String> {
        let input: Input =
            serde_json::from_value(input).map_err(|e| format!("hf_download input: {e}"))?;
        let row = store::get_hf_model(&ctx.state.db, input.hf_model_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("download entry {} no longer exists", input.hf_model_id))?;
        match transfer(&ctx, &row).await {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                tracing::warn!("hf download {}/{} failed: {e}", row.repo, row.file);
                let _ = store::set_hf_status(&ctx.state.db, row.id, "failed", Some(&e)).await;
                Err(e)
            }
        }
    }
}

async fn transfer(ctx: &JobCtx, row: &HfModelRow) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let snap = state.snapshot();
    // Every transfer asks, not only the verbs that queue one: boot resumes
    // the rows a previous run left queued or downloading, straight into here.
    let models_dir = hf::models_dir_to_write(state, &row.target)?;
    store::set_hf_status(&state.db, row.id, "downloading", None)
        .await
        .map_err(|e| e.to_string())?;

    let detail = serde_json::to_value(Detail {
        hf_model_id: row.id,
        repo: row.repo.clone(),
        file: row.file.clone(),
        target: row.target.clone(),
    })
    .unwrap_or(Value::Null);
    let report = |done: u64, total: Option<u64>| JobProgress {
        done,
        total,
        stage: "downloading".into(),
        detail: detail.clone(),
    };

    // The revision the row asks for: `main`, or the commit an audio catalog
    // spec pins. The commit the hub resolves it to is recorded with the file.
    let revision = row.revision();
    let url = hf::resolve_url(&row.repo, revision, &row.file);
    let (mut resp, commit) = hf::get_with_commit(
        &state.proxy_http,
        &state.http,
        &snap.settings.hf_token,
        &url,
    )
    .await?;
    if !resp.status().is_success() {
        // The row's `error` is what the Downloads page and `lmgw__hf_downloads`
        // show, so a gated repo says what to do about it rather than "GET …:
        // 401 Unauthorized" (design §2.7). The body is read first because a 403
        // only counts when the hub says it is about the licence.
        let status = resp.status();
        let error_code = resp
            .headers()
            .get("x-error-code")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp.text().await.unwrap_or_default();
        if let Some(sentence) =
            hf::hub_refusal(status, &body, &row.repo, !snap.settings.hf_token.is_empty())
        {
            return Err(sentence);
        }
        // A pin that is gone is not swapped for `main` behind the owner's
        // back; the sentence says how to take the latest instead. Only when
        // it is gone: a rate limit or a 5xx is not a reason to give up the
        // pin, and `main` would meet it too.
        if revision != crate::audio::pins::MAIN
            && crate::audio::pins::pin_is_missing(status.as_u16(), error_code.as_deref())
        {
            return Err(format!(
                "GET {url}: {status} — {}",
                crate::audio::pins::no_fallback(&row.repo, revision)
            ));
        }
        return Err(format!("GET {url}: {status}"));
    }
    // No header (a mirror, a mock): the revision asked for when that is a
    // commit — those bytes are its by definition — else unknown.
    let commit = commit
        .or_else(|| crate::audio::pins::is_commit(revision).then(|| revision.to_ascii_lowercase()));
    let etag = hf::etag_from_headers(resp.headers());
    let total = resp.content_length();
    ctx.progress(report(0, total)).await;

    let dest = PathBuf::from(&models_dir).join(&row.dest_path);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    // Stream to `.part`, rename on success — llama-server never sees a
    // half-written GGUF.
    let tmp = PathBuf::from(format!("{}.part", dest.display()));
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
    let mut received: u64 = 0;
    loop {
        if ctx.canceled() {
            drop(file);
            // The transfer has no resume (no Range request), so a partial file
            // is dead weight the owner did not ask to keep.
            let _ = tokio::fs::remove_file(&tmp).await;
            store::set_hf_status(&state.db, row.id, "failed", Some(CANCELED_NOTE))
                .await
                .map_err(|e| e.to_string())?;
            tracing::info!("hf download canceled: {}/{}", row.repo, row.file);
            return Ok(JobOutcome::Canceled);
        }
        let Some(chunk) = resp.chunk().await.map_err(|e| format!("download: {e}"))? else {
            break;
        };
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        received += chunk.len() as u64;
        ctx.progress(report(received, total)).await;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    drop(file);
    tokio::fs::rename(&tmp, &dest)
        .await
        .map_err(|e| format!("renaming to {}: {e}", dest.display()))?;
    store::mark_hf_done(
        &state.db,
        row.id,
        etag.as_deref(),
        received as i64,
        revision,
        commit.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;
    tracing::info!(
        "hf download done: {} ({} bytes, {revision} at {})",
        dest.display(),
        received,
        commit.as_deref().unwrap_or("an unknown commit")
    );
    // A new GGUF beside a row's weights can change what that row renders
    // (the direct-file pick): a running container is stopped for apply.
    if row.target == "audio" {
        crate::runtime::audio::stop_stale(state, &format!("the download of {}", row.dest_path))
            .await;
    }
    Ok(JobOutcome::Done(json!({
        "hf_model_id": row.id,
        "dest_path": row.dest_path,
        "bytes": received,
        "etag": etag,
        "requested_revision": revision,
        "resolved_commit": commit,
    })))
}
