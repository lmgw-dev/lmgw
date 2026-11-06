//! The `hf_download` job kind: stream one tracked Hugging Face file into its
//! container's models dir (§8, §9c).
//!
//! The durable record of *what is on disk* stays in `hf_models` — status, ETag,
//! size, destination — because that is the model registry's truth and it
//! outlives any single transfer. The job row is the *transfer*: it starts,
//! reports bytes, and ends. Retention trims the transfers; the file record
//! stays until untracked.
//!
//! **A cut transfer continues** (find-a-model design D122, [`resume`]). The
//! bytes go to `<dest>.part`, and the row records the content hash the hub
//! named for them (`hf_models.oid`, the hub hop's `x-linked-etag`) once the
//! `.part` is emptied for them, never before (D128). A transfer that finds
//! a `.part` hashes it, then asks the CDN for the rest of it (`Range:
//! bytes=<reached>-`) and appends, while the hub still names that hash and
//! the CDN answers `206` from that byte. Otherwise it starts again from
//! byte 0 and says why: the file changed on the hub, or the CDN sent the
//! whole file. A `416` over a `.part` that is already the whole file
//! finishes it (D132). A failed transfer keeps its `.part` for the next one;
//! a cancel deletes it. At boot, [`resume_at_boot`] queues the rows a
//! shutdown cut, except on a metered connection, where they wait for
//! "Resume" or for the connection to stop being metered (D133).
//!
//! **What is renamed into place is the file the hub named** (D130). Its
//! length is the response's, cross-checked with the hub's `x-linked-size`
//! and taken from it when the response gives none, and an LFS file (every
//! GGUF) is hashed as it arrives, its kept `.part` once before the rest:
//! the sha256 must be the hub's. A file the hub keeps in git has no such
//! hash and is checked by its length alone. A file that fails either check
//! is deleted, its hash forgotten, and the transfer fails with a sentence.
//!
//! **A stalled transfer stops** (D134): 30 s without a byte from the hub or
//! the CDN, the header read's bound ([`hf::remote::STALL`]), fails it with
//! a sentence and keeps its `.part`; a cancel is seen while it waits.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::{JobCtx, JobExecutor, JobKind, JobOutcome, JobProgress, Spawn};
use crate::agent::Cancel;
use crate::erased::Erased;
use crate::hf;
use crate::hf::remote::STALL;
use crate::state::SharedState;
use crate::store::{self, HfModelRow};

mod resume;

pub use resume::{go_on_when_unmetered, resume_at_boot, BootResume};
use resume::{sha256_of, Begin};

/// `hf_models.error` left behind by a cancelled transfer. The row goes back to
/// `failed` so the Downloads page offers "Retry" — a cancelled download is a
/// file that is not there, and saying so is the honest state.
pub const CANCELED_NOTE: &str = "canceled";

/// `hf_models.error` of a row the boot left queued because the connection is
/// metered (D122): the person confirmed the download on another connection.
/// The row stays `queued` with no transfer, which the Downloads page offers
/// "Resume" for, until the connection stops being metered (D133).
pub const PAUSED_METERED_NOTE: &str = "Paused: metered connection — lmgw doesn't go on with a \
     download by itself on a metered connection. Resume goes on from where it stopped, and so \
     does lmgw once the connection isn't metered.";

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
    /// The byte a continued transfer went on from (D122); absent when it
    /// began at byte 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<u64>,
    /// Why a kept `.part` was not continued and the transfer began again at
    /// byte 0 (D122, D46).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarted: Option<String>,
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

/// Cancel the transfer of a tracked row: the one running, or a row that
/// waits with none (paused on a metered connection, or cut and not queued
/// again), which ends as a cancelled transfer does.
pub async fn cancel_for_row(state: &SharedState, hf_model_id: i64) -> Result<String, String> {
    match state
        .jobs
        .live_by_key(JobKind::HfDownload, &job_key(hf_model_id))
    {
        Some(job) => super::cancel(state, job.id).await,
        None => resume::cancel_idle(state, hf_model_id)
            .await
            .unwrap_or_else(|| Err("no download is running or waiting for this entry".into())),
    }
}

pub struct HfDownloadExecutor;

impl JobExecutor for HfDownloadExecutor {
    fn kind(&self) -> JobKind {
        JobKind::HfDownload
    }

    fn run<'a>(&'a self, ctx: JobCtx, input: Value) -> Erased<'a, Result<JobOutcome, String>> {
        Box::pin(async move {
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
        })
    }
}

/// A write into the `.part` that failed. A full disk says how much the
/// file still needs and how much is free, and the `.part` stays for Retry
/// (find-a-model design D125, D48).
fn write_failed(
    e: &std::io::Error,
    file: &str,
    tmp: &Path,
    received: u64,
    total: Option<u64>,
) -> String {
    let full =
        e.kind() == std::io::ErrorKind::StorageFull || e.raw_os_error() == Some(libc::ENOSPC);
    if !full {
        return format!("writing {}: {e}", tmp.display());
    }
    let needs = match total {
        Some(t) => format!("needs {} more", hf::fmt_bytes(t.saturating_sub(received))),
        None => "needs more".to_string(),
    };
    let free = match crate::backends::paths::free_bytes(tmp) {
        Some(f) => format!("{} is free", hf::fmt_bytes(f)),
        None => "how much is free couldn't be read".to_string(),
    };
    format!(
        "The disk is full: {file} {needs} on the disk, {free}. Free some space; Retry goes on \
         from the {} already here",
        hf::fmt_bytes(received)
    )
}

/// GET the row's resolve URL, from byte `from` when it is given (a `Range`
/// on the CDN's hop only, [`hf::get_with_commit`]).
async fn request(
    state: &SharedState,
    url: &str,
    from: Option<u64>,
) -> Result<(reqwest::Response, hf::HubHop), String> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(from) = from {
        let range = format!("bytes={from}-")
            .parse()
            .map_err(|e| format!("a Range from byte {from}: {e}"))?;
        headers.insert(reqwest::header::RANGE, range);
    }
    let token = state.snapshot().settings.hf_token.clone();
    hf::get_with_commit(&state.proxy_http, &state.http, &token, url, &headers).await
}

/// The sentence a failed resolve answers with (design §2.7): the body is read
/// first because a 403 only counts as gated when the hub says it is about the
/// licence.
async fn refusal(resp: reqwest::Response, row: &HfModelRow, url: &str, has_token: bool) -> String {
    let revision = row.revision();
    let status = resp.status();
    let error_code = resp
        .headers()
        .get("x-error-code")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.text().await.unwrap_or_default();
    if let Some(sentence) = hf::hub_refusal(status, &body, &row.repo, has_token) {
        return sentence;
    }
    // A pin that is gone is not swapped for `main` behind the owner's back;
    // the sentence says how to take the latest instead. Only when it is gone:
    // a rate limit or a 5xx is not a reason to give up the pin, and `main`
    // would meet it too.
    if revision != crate::audio::pins::MAIN
        && crate::audio::pins::pin_is_missing(status.as_u16(), error_code.as_deref())
    {
        return format!(
            "GET {url}: {status} — {}",
            crate::audio::pins::no_fallback(&row.repo, revision)
        );
    }
    format!("GET {url}: {status}")
}

/// [`request`], bounded by [`STALL`] and raced against the cancel (D134).
/// `Ok(None)`: cancelled.
async fn ask(
    ctx: &JobCtx,
    cancel: &Cancel,
    url: &str,
    file: &str,
    from: Option<u64>,
) -> Result<Option<(reqwest::Response, hf::HubHop)>, String> {
    match cancel
        .guard(tokio::time::timeout(STALL, request(&ctx.state, url, from)))
        .await
    {
        None => Ok(None),
        Some(Err(_)) => Err(stalled(file, from.unwrap_or(0))),
        Some(Ok(answer)) => answer.map(Some),
    }
}

/// The sentence a stalled transfer fails with (D134).
fn stalled(file: &str, kept: u64) -> String {
    let after = match kept {
        0 => "Retry asks again".to_string(),
        n => format!("Retry goes on from the {} already here", hf::fmt_bytes(n)),
    };
    format!(
        "Hugging Face sent nothing for {} s while lmgw downloaded {file}; {after}",
        STALL.as_secs()
    )
}

/// A cancelled transfer: its bytes are not kept for a resume.
async fn canceled(ctx: &JobCtx, row: &HfModelRow, tmp: &Path) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    // A cancelled download is one the owner stopped: its bytes are not kept
    // for a resume.
    let _ = tokio::fs::remove_file(tmp).await;
    let _ = store::set_hf_oid(&state.db, row.id, None).await;
    store::set_hf_status(&state.db, row.id, "failed", Some(CANCELED_NOTE))
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("hf download canceled: {}/{}", row.repo, row.file);
    Ok(JobOutcome::Canceled)
}

/// A `.part` that isn't the file the hub named: deleted and its hash
/// forgotten, so the next transfer starts from byte 0, and `why` is the
/// failure (D130).
async fn discard(state: &SharedState, row: &HfModelRow, tmp: &Path, why: String) -> String {
    let _ = tokio::fs::remove_file(tmp).await;
    let _ = store::set_hf_oid(&state.db, row.id, None).await;
    why
}

/// The sentence of a file whose bytes don't hash to the hub's sha256.
fn mismatch(file: &str, want: &str) -> String {
    format!(
        "the downloaded {file} doesn't match the sha256 Hugging Face names for it ({}…), so lmgw \
         deleted it; Retry downloads it again from the beginning",
        &want[..want.len().min(12)]
    )
}

/// Whether `hasher` holds the bytes `oid` names; `true` when the hub named
/// no sha256 (a file kept in git: its length was its check).
fn hash_holds(hasher: Option<Sha256>, oid: Option<&str>) -> bool {
    match (hasher, oid.and_then(sha256_of)) {
        (Some(h), Some(want)) => h.finalize().as_slice() == want.as_slice(),
        _ => true,
    }
}

async fn transfer(ctx: &JobCtx, row: &HfModelRow) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    let snap = state.snapshot();
    let has_token = !snap.settings.hf_token.is_empty();
    // Every transfer asks, not only the verbs that queue one: boot resumes
    // the rows a previous run left queued or downloading, straight into here.
    let models_dir = hf::models_dir_to_write(state, &row.target)?;
    store::set_hf_status(&state.db, row.id, "downloading", None)
        .await
        .map_err(|e| e.to_string())?;
    let cancel = Cancel::flag(ctx.cancel_signal());

    let dest = PathBuf::from(&models_dir).join(&row.dest_path);
    // Stream to `.part`, rename on success — llama-server never sees a
    // half-written GGUF.
    let tmp = PathBuf::from(format!("{}.part", dest.display()));
    let base = Detail {
        hf_model_id: row.id,
        repo: row.repo.clone(),
        file: row.file.clone(),
        target: row.target.clone(),
        resumed_from: None,
        restarted: None,
    };
    // What a cut transfer left, and its bytes hashed before the rest is
    // asked for, when the hash they are of is a sha256 (D130).
    let kept = resume::kept(row, &tmp).await;
    let mut prefix: Option<Sha256> = None;
    // How long that read of the `.part` took: part of the job's run time,
    // but not of the transfer, so the download rate leaves it out
    // (`hf::rate`, find-a-model design D58).
    let mut check_secs = 0f64;
    if let (Some(have), Some(_)) = (kept, row.oid.as_deref().and_then(sha256_of)) {
        let detail = serde_json::to_value(&base).unwrap_or(Value::Null);
        let began = std::time::Instant::now();
        match resume::hash_prefix(ctx, &tmp, have, &detail).await? {
            Some(h) => prefix = Some(h),
            None => return canceled(ctx, row, &tmp).await,
        }
        check_secs = began.elapsed().as_secs_f64();
    }

    // The revision the row asks for: `main`, or the commit an audio catalog
    // spec pins. The commit the hub resolves it to is recorded with the file.
    let revision = row.revision();
    let url = hf::resolve_url(&row.repo, revision, &row.file);
    let Some((mut resp, mut hop)) = ask(ctx, &cancel, &url, &row.file, kept).await? else {
        return canceled(ctx, row, &tmp).await;
    };
    let mut begin = Begin::fresh();
    if let Some(have) = kept {
        if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // The `.part` may be the whole file already (D132).
            if resume::whole_on_disk(row, &hop, resp.headers(), have) {
                let etag = hf::etag_from_headers(resp.headers());
                return finish_kept(ctx, row, &tmp, &dest, have, prefix, hop, etag).await;
            }
            // A `.part` as long as the file or longer, of another hash or
            // length: nothing to send from there, so it starts again.
            begin.restarted = Some(format!(
                "Hugging Face had nothing past the {} already here, so it starts again from \
                 the beginning",
                hf::fmt_bytes(have)
            ));
            let Some(again) = ask(ctx, &cancel, &url, &row.file, None).await? else {
                return canceled(ctx, row, &tmp).await;
            };
            (resp, hop) = again;
        } else if resp.status().is_success() {
            begin = resume::continues(row, &hop, &resp, have);
            // A 206 of another file, or of another offset, is no use: ask
            // again from byte 0. A 200 is the whole file already.
            if begin.from == 0 && resp.status() != reqwest::StatusCode::OK {
                let Some(again) = ask(ctx, &cancel, &url, &row.file, None).await? else {
                    return canceled(ctx, row, &tmp).await;
                };
                (resp, hop) = again;
            }
        }
    }
    if !resp.status().is_success() {
        return Err(refusal(resp, row, &url, has_token).await);
    }
    if let Some(why) = &begin.restarted {
        tracing::info!("hf download {}/{}: {why}", row.repo, row.file);
    }
    // No header (a mirror, a mock): the revision asked for when that is a
    // commit — those bytes are its by definition — else unknown.
    let commit = hop
        .commit
        .clone()
        .or_else(|| crate::audio::pins::is_commit(revision).then(|| revision.to_ascii_lowercase()));
    let etag = hf::etag_from_headers(resp.headers());
    // The file's length as the response says it, cross-checked with the
    // hub's and taken from it when the response says none (D130).
    let said = match begin.from {
        0 => resp.content_length(),
        from => hf::remote::content_range(resp.headers())
            .and_then(|(_, total)| total)
            .or_else(|| resp.content_length().map(|n| from + n)),
    };
    let total = match (said, hop.linked_size) {
        (Some(sent), Some(named)) if sent != named => {
            return Err(format!(
                "Hugging Face names {} for {} and its download says {}, so lmgw didn't take \
                 it; Retry asks again",
                hf::fmt_bytes(named),
                row.file,
                hf::fmt_bytes(sent)
            ))
        }
        (sent, named) => sent.or(named),
    };
    // The hash these bytes must have: the one the hub named for them, or
    // the one the kept `.part` was begun on, which the hub still names.
    let oid = match begin.from {
        0 => hop.linked_etag.clone(),
        _ => row.oid.clone(),
    };
    let mut hasher: Option<Sha256> = match begin.from {
        0 => oid.as_deref().and_then(sha256_of).map(|_| Sha256::new()),
        _ => prefix.take(),
    };

    let detail = serde_json::to_value(Detail {
        resumed_from: (begin.from > 0).then_some(begin.from),
        restarted: begin.restarted.clone(),
        ..base
    })
    .unwrap_or(Value::Null);
    let report = |done: u64, total: Option<u64>| JobProgress {
        done,
        total,
        stage: "downloading".into(),
        detail: detail.clone(),
    };
    ctx.progress(report(begin.from, total)).await;

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let mut file = match begin.from {
        0 => tokio::fs::File::create(&tmp).await,
        _ => tokio::fs::OpenOptions::new().append(true).open(&tmp).await,
    }
    .map_err(|e| format!("opening {}: {e}", tmp.display()))?;
    if begin.from == 0 {
        // The hash these bytes are of, which the next transfer continues
        // against; none when the hub named none. Only now that the `.part`
        // is empty (D128): written before, a stop in between left another
        // file's bytes under this hash, and the next transfer appended to
        // them.
        store::set_hf_oid(&state.db, row.id, hop.linked_etag.as_deref())
            .await
            .map_err(|e| e.to_string())?;
    }
    let mut received: u64 = begin.from;
    loop {
        let chunk = match cancel
            .guard(tokio::time::timeout(STALL, resp.chunk()))
            .await
        {
            None => {
                drop(file);
                return canceled(ctx, row, &tmp).await;
            }
            Some(Err(_)) => return Err(stalled(&row.file, received)),
            Some(Ok(Err(e))) => return Err(format!("download: {e}")),
            Some(Ok(Ok(None))) => break,
            Some(Ok(Ok(Some(chunk)))) => chunk,
        };
        file.write_all(&chunk)
            .await
            .map_err(|e| write_failed(&e, &row.file, &tmp, received, total))?;
        if let Some(h) = hasher.as_mut() {
            h.update(&chunk);
        }
        received += chunk.len() as u64;
        ctx.progress(report(received, total)).await;
    }
    file.flush()
        .await
        .map_err(|e| write_failed(&e, &row.file, &tmp, received, total))?;
    drop(file);
    match total {
        // A body that ended short of the length it was sent with is a cut
        // transfer, whatever the connection said: its `.part` stays for
        // the next (D48).
        Some(total) if received < total => {
            return Err(format!(
                "the download of {} ended at {}; Retry goes on from there",
                row.file,
                lmgw_api_types::units::fmt_bytes_of(received, total)
            ))
        }
        Some(total) if received > total => {
            return Err(discard(
                state,
                row,
                &tmp,
                format!(
                    "Hugging Face sent {} of {}, more than the {} it names, so lmgw deleted it; \
                     Retry downloads it again from the beginning",
                    hf::fmt_bytes(received),
                    row.file,
                    hf::fmt_bytes(total)
                ),
            )
            .await)
        }
        _ => {}
    }
    if !hash_holds(hasher, oid.as_deref()) {
        let want = oid.clone().unwrap_or_default();
        return Err(discard(state, row, &tmp, mismatch(&row.file, &want)).await);
    }
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
        "hf download done: {} ({} bytes{}, {revision} at {})",
        dest.display(),
        received,
        match begin.from {
            0 => String::new(),
            from => format!(", gone on from byte {from}"),
        },
        commit.as_deref().unwrap_or("an unknown commit")
    );
    after_done(state, row).await;
    Ok(JobOutcome::Done(json!({
        "hf_model_id": row.id,
        "dest_path": row.dest_path,
        "bytes": received,
        "etag": etag,
        "requested_revision": revision,
        "resolved_commit": commit,
        "resumed_from": (begin.from > 0).then_some(begin.from),
        "restarted": begin.restarted,
        "check_secs": check_secs,
    })))
}

/// What a finished download changes beside its row.
async fn after_done(state: &SharedState, row: &HfModelRow) {
    // A new GGUF beside a row's weights can change what that row renders
    // (the direct-file pick): a running container is stopped for apply.
    if row.target == "audio" {
        crate::runtime::audio::stop_stale(state, &format!("the download of {}", row.dest_path))
            .await;
    }
}

/// A `.part` the CDN's `416` says is the whole file (D132): checked against
/// the hub's hash (its bytes hashed already, `prefix`) and renamed into
/// place, with nothing downloaded again. The ETag the update check compares
/// is asked of the hub when the `416` carries none.
#[allow(clippy::too_many_arguments)]
async fn finish_kept(
    ctx: &JobCtx,
    row: &HfModelRow,
    tmp: &Path,
    dest: &Path,
    have: u64,
    prefix: Option<Sha256>,
    hop: hf::HubHop,
    etag: Option<String>,
) -> Result<JobOutcome, String> {
    let state = &ctx.state;
    if !hash_holds(prefix, row.oid.as_deref()) {
        let want = row.oid.clone().unwrap_or_default();
        return Err(discard(state, row, tmp, mismatch(&row.file, &want)).await);
    }
    let revision = row.revision();
    let etag = match etag {
        Some(e) => Some(e),
        None => {
            let token = state.snapshot().settings.hf_token.clone();
            hf::remote_etag(&state.http, &token, &row.repo, revision, &row.file)
                .await
                .unwrap_or_else(|e| {
                    tracing::info!(
                        "hf download {}/{}: its ETag could not be asked ({e}); the next update \
                         check reads it as changed",
                        row.repo,
                        row.file
                    );
                    None
                })
        }
    };
    let commit = hop
        .commit
        .clone()
        .or_else(|| crate::audio::pins::is_commit(revision).then(|| revision.to_ascii_lowercase()));
    tokio::fs::rename(tmp, dest)
        .await
        .map_err(|e| format!("renaming to {}: {e}", dest.display()))?;
    store::mark_hf_done(
        &state.db,
        row.id,
        etag.as_deref(),
        have as i64,
        revision,
        commit.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;
    tracing::info!(
        "hf download done: {} ({have} bytes, all of them kept from before)",
        dest.display()
    );
    after_done(state, row).await;
    Ok(JobOutcome::Done(json!({
        "hf_model_id": row.id,
        "dest_path": row.dest_path,
        "bytes": have,
        "etag": etag,
        "requested_revision": revision,
        "resolved_commit": commit,
        "resumed_from": have,
        "restarted": Value::Null,
    })))
}
