//! How a transfer goes on from what a cut one left (find-a-model design
//! D122): the `.part` and the hash it is of, its bytes hashed once before a
//! Range asks for the rest (D130), the answers that continue it or start it
//! again (D46), a `416` over a `.part` that is already the whole file
//! (D132), the boot's resume and its metered pause (D47), and the pause
//! going on by itself once the connection is no longer metered (D133).

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{start, JobCtx, JobProgress, PAUSED_METERED_NOTE};
use crate::hf;
use crate::jobs::JobKind;
use crate::machine::Metered;
use crate::state::SharedState;
use crate::store::{self, HfModelRow};

/// The stage of a transfer reading its kept `.part` (D130); the Downloads
/// page names it beside the share read.
pub const CHECKING: &str = "checking";

/// How much of a kept `.part` is read and hashed at a time: a step, not a
/// limit. The whole `.part` is read, once, front to back.
const HASH_STEP: usize = 8 * 1024 * 1024;

/// The sha256 an LFS file's hash names, when it is one: 64 hex digits, as
/// the hub's `x-linked-etag` and the listing's `lfs.oid` are. A file the hub
/// keeps in git has none, and is checked by its size alone (D130).
pub(super) fn sha256_of(oid: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(oid.trim(), &mut out).ok()?;
    Some(out)
}

/// What a cut transfer left: the `.part`'s bytes, when there are some and
/// the hash they are of is recorded.
pub(super) async fn kept(row: &HfModelRow, tmp: &Path) -> Option<u64> {
    row.oid.as_ref()?;
    tokio::fs::metadata(tmp)
        .await
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .filter(|n| *n > 0)
}

/// The first `have` bytes of the `.part`, hashed, read once front to back
/// on the blocking pool with progress as it goes (D130). `None`: the
/// transfer was cancelled meanwhile.
pub(super) async fn hash_prefix(
    ctx: &JobCtx,
    tmp: &Path,
    have: u64,
    detail: &Value,
) -> Result<Option<Sha256>, String> {
    let kept = hf::fmt_bytes(have);
    let what = |e: std::io::Error| format!("reading the {kept} already in {}: {e}", tmp.display());
    let path = tmp.to_path_buf();
    let mut file = tokio::task::spawn_blocking(move || std::fs::File::open(path))
        .await
        .map_err(|e| format!("reading {}: {e}", tmp.display()))?
        .map_err(&what)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_STEP.min(have as usize)];
    let mut done = 0u64;
    while done < have {
        if ctx.canceled() {
            return Ok(None);
        }
        let n = (have - done).min(buf.len() as u64) as usize;
        let (f, h, b) = tokio::task::spawn_blocking(move || {
            let read = file.read_exact(&mut buf[..n]);
            if read.is_ok() {
                hasher.update(&buf[..n]);
            }
            read.map(|()| (file, hasher, buf))
        })
        .await
        .map_err(|e| format!("reading {}: {e}", tmp.display()))?
        .map_err(&what)?;
        (file, hasher, buf) = (f, h, b);
        done += n as u64;
        ctx.progress(JobProgress {
            done,
            total: Some(have),
            stage: CHECKING.into(),
            detail: detail.clone(),
        })
        .await;
    }
    Ok(Some(hasher))
}

/// Where a transfer begins: the byte it goes on from (0 for a fresh one),
/// and why a kept `.part` was not continued.
pub(super) struct Begin {
    pub from: u64,
    pub restarted: Option<String>,
}

impl Begin {
    pub fn fresh() -> Self {
        Self {
            from: 0,
            restarted: None,
        }
    }
}

/// Decide whether the response to a resumed request continues the `.part`
/// (`have` bytes of the file `row.oid` names), and what to say if not
/// (D46).
pub(super) fn continues(
    row: &HfModelRow,
    hop: &hf::HubHop,
    resp: &reqwest::Response,
    have: u64,
) -> Begin {
    let kept = hf::fmt_bytes(have);
    let again = |why: String| Begin {
        from: 0,
        restarted: Some(why),
    };
    match &hop.linked_etag {
        None => {
            return again(format!(
                "Hugging Face didn't say which file it sent this time, so the {kept} already \
                 here were dropped and it starts again from the beginning"
            ))
        }
        Some(now) if !same_hash(Some(now), row.oid.as_deref()) => {
            return again(format!(
                "the file changed on Hugging Face since its download began, so the {kept} \
                 already here were dropped and it starts again from the beginning"
            ))
        }
        Some(_) => {}
    }
    let from_there = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT
        && hf::remote::content_range(resp.headers()).map(|(start, _)| start) == Some(have);
    if from_there {
        return Begin {
            from: have,
            restarted: None,
        };
    }
    let how = match resp.status() {
        reqwest::StatusCode::PARTIAL_CONTENT => "another part of it than the rest".to_string(),
        s => format!("the file from its beginning instead of the rest of it ({s})"),
    };
    again(format!(
        "Hugging Face sent {how}, so the {kept} already here were dropped and it starts again"
    ))
}

/// Two content hashes name the same file: both known and equal, case aside.
fn same_hash(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a.trim().eq_ignore_ascii_case(b.trim()))
}

/// `Content-Range: bytes */<total>`, the total a `416` names.
fn unsatisfied_total(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_RANGE)?
        .to_str()
        .ok()?
        .trim()
        .strip_prefix("bytes */")?
        .trim()
        .parse()
        .ok()
}

/// A `416` to the Range from byte `have` says the `.part` is already the
/// whole file (D132): the CDN names the file's length as `have`, the hub
/// names the hash the `.part` was begun on, and its length, when it gives
/// one, is `have` too. The caller still checks the hash.
pub(super) fn whole_on_disk(
    row: &HfModelRow,
    hop: &hf::HubHop,
    headers: &reqwest::header::HeaderMap,
    have: u64,
) -> bool {
    unsatisfied_total(headers) == Some(have)
        && same_hash(hop.linked_etag.as_deref(), row.oid.as_deref())
        && hop.linked_size.is_none_or(|n| n == have)
}

/// What the boot did with the downloads the last shutdown cut.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootResume {
    /// The rows queued again, by id.
    pub started: Vec<i64>,
    /// The rows left waiting for "Resume", by id: the connection is metered.
    pub paused: Vec<i64>,
    /// The device and NetworkManager's word for it, when it was metered.
    pub metered: Option<String>,
}

/// Queue again every download a previous run left `queued` or
/// `downloading` (D122). Called once at boot, after the orphaned job rows
/// are closed (`store::fail_orphaned_jobs`), so their `(kind, key)` guard
/// is free. Each transfer continues its `.part` where it can.
///
/// On a metered connection nothing goes on by itself (machine check D99,
/// [`crate::machine::connection_metered`]): each row stays `queued` with
/// [`PAUSED_METERED_NOTE`], its "Resume" goes on, and a watch on
/// NetworkManager lets them go on by themselves once the connection is no
/// longer metered ([`go_on_when_unmetered`], D133). What NetworkManager
/// can't tell counts as not metered, as the engine pre-fetch counts it,
/// and the log says why.
pub async fn resume_at_boot(state: &SharedState) -> BootResume {
    let rows: Vec<HfModelRow> = store::list_hf_models(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.status == "queued" || r.status == "downloading")
        .collect();
    let mut out = BootResume::default();
    if rows.is_empty() {
        return out;
    }
    match crate::machine::connection_metered(state).await {
        Metered::Yes(how) => {
            for row in &rows {
                if let Err(e) =
                    store::set_hf_status(&state.db, row.id, "queued", Some(PAUSED_METERED_NOTE))
                        .await
                {
                    tracing::warn!("pausing hf download {}/{}: {e}", row.repo, row.file);
                    continue;
                }
                out.paused.push(row.id);
            }
            tracing::info!(
                "{} hf download(s) cut by the last shutdown wait for Resume: the connection is \
                 metered ({how})",
                out.paused.len()
            );
            state.jobs.publish(state);
            out.metered = Some(how);
            watch_paused(state);
            return out;
        }
        Metered::No => {}
        Metered::Unknown(why) => tracing::info!(
            "whether the connection is metered is not known ({why}), so it counts as not \
             metered and the {} hf download(s) cut by the last shutdown go on",
            rows.len()
        ),
    }
    for row in &rows {
        tracing::info!("resuming hf download {}/{}", row.repo, row.file);
        match start(state, row).await {
            Ok(_) => out.started.push(row.id),
            Err(e) => tracing::warn!("resuming hf download {}/{}: {e}", row.repo, row.file),
        }
    }
    out
}

/// The rows the boot paused that nothing transfers now.
pub(super) async fn paused_rows(state: &SharedState) -> Vec<HfModelRow> {
    store::list_hf_models(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.status == "queued" && r.error.as_deref() == Some(PAUSED_METERED_NOTE))
        .filter(|r| {
            state
                .jobs
                .live_by_key(JobKind::HfDownload, &super::job_key(r.id))
                .is_none()
        })
        .collect()
}

/// Begin [`go_on_when_unmetered`] on the machine's NetworkManager watch, or
/// say why the paused rows wait for Resume alone.
fn watch_paused(state: &SharedState) {
    match state.machine.watch_network() {
        Ok(changes) => {
            let st = state.clone();
            tokio::spawn(async move { go_on_when_unmetered(&st, changes).await });
        }
        Err(why) => tracing::info!(
            "the paused hf downloads wait for Resume: lmgw can't watch NetworkManager for a \
             change of connection ({why})"
        ),
    }
}

/// While paused rows wait, ask again whether the connection is metered each
/// time NetworkManager reports a change (D133), and once it isn't, let them
/// go on. Ends when they went on, when none waits any more (each was
/// resumed, cancelled or deleted), or when the watch ends; then the watch is
/// dropped, which ends NetworkManager's monitor.
pub async fn go_on_when_unmetered(state: &SharedState, mut changes: crate::machine::NetChanges) {
    while changes.recv().await.is_some() {
        let paused = paused_rows(state).await;
        if paused.is_empty() {
            return;
        }
        match crate::machine::connection_metered(state).await {
            Metered::Yes(_) => continue,
            Metered::No => tracing::info!(
                "the connection is no longer metered: {} paused hf download(s) go on",
                paused.len()
            ),
            Metered::Unknown(why) => tracing::info!(
                "the connection changed and whether it is metered is not known ({why}), so it \
                 counts as not metered: {} paused hf download(s) go on",
                paused.len()
            ),
        }
        for row in &paused {
            if let Err(e) = start(state, row).await {
                tracing::warn!("resuming hf download {}/{}: {e}", row.repo, row.file);
            }
        }
        return;
    }
    tracing::info!("NetworkManager's monitor ended: the paused hf downloads wait for Resume");
}

/// A Cancel on a row nothing transfers (paused, or cut and not queued
/// again): it ends as a running transfer's cancel does, its `.part` deleted
/// and its hash forgotten. `None` when the row isn't such a row.
pub(super) async fn cancel_idle(state: &SharedState, id: i64) -> Option<Result<String, String>> {
    let row = store::get_hf_model(&state.db, id).await.ok()??;
    if !matches!(row.status.as_str(), "queued" | "downloading") {
        return None;
    }
    let dir = hf::models_dir_for_target(&state.snapshot().settings, &row.target);
    if !dir.trim().is_empty() {
        let tmp = PathBuf::from(format!(
            "{}.part",
            Path::new(&dir).join(&row.dest_path).display()
        ));
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    let done = async {
        store::set_hf_oid(&state.db, id, None).await?;
        store::set_hf_status(&state.db, id, "failed", Some(super::CANCELED_NOTE)).await
    };
    let done = done.await;
    // No transfer ends here, so no job's end publishes it: a get waiting
    // for this row reads it at this frame.
    state.jobs.publish(state);
    Some(match done {
        Ok(()) => {
            tracing::info!(
                "hf download canceled while it waited: {}/{}",
                row.repo,
                row.file
            );
            Ok(format!(
                "{} won't download; Retry starts it again",
                row.file
            ))
        }
        Err(e) => Err(e.to_string()),
    })
}
